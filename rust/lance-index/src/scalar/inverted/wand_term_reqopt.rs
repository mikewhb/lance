// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! A `+must should...` Boolean query over its raw one-term postings.

use std::sync::Arc;

use lance_core::{Error, Result};

use super::super::query::FtsSearchParams;
use super::super::scorer::{MemBM25Scorer, bm25_doc_weight_with_norm};
use super::{PostingIterator, WandDocuments, conservative_score_sum, norm_addends};
use crate::metrics::MetricsCollector;

/// One MUST and one or more SHOULD one-term postings, scored a MUST block at a
/// time.
///
/// Emits exactly the documents and score bits of the generic
/// `ReqOpt(MUST, Disjunction(SHOULD...))` tree over the same postings that can
/// still reach the competitive floor: the MUST score, plus the matched SHOULD
/// scores summed in clause order from zero. A MUST block whose combined window
/// bound is below the floor is skipped without decoding the SHOULD postings.
pub(in super::super) struct TermReqOptScorer<'a, D: WandDocuments> {
    must: PostingIterator,
    shoulds: Vec<PostingIterator>,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    /// Per-document norm codes and their BM25 denominator addends; scoring
    /// through them is bit-identical to `Scorer::doc_weight`.
    norms: Option<(&'a [u8], Arc<[f32; 256]>)>,
    global_upper: f32,
    cost: usize,
    floor: f32,
    exhausted: bool,
    hits: Vec<(u64, u32)>,
    docs: Vec<u64>,
    scores: Vec<f32>,
    should_sums: Vec<f32>,
    should_matched: Vec<bool>,
    should_freqs: Vec<u32>,
    next_hit: usize,
    /// `(doc, document key, score)` of the emitted document.
    current: Option<(u64, u64, f32)>,
    comparisons: usize,
    metrics_recorded: bool,
    metrics: &'a dyn MetricsCollector,
}

impl<'a, D: WandDocuments> TermReqOptScorer<'a, D> {
    /// Whether one clause's `postings` can be read directly with identical
    /// results.
    pub(in super::super) fn accepts(
        postings: &[PostingIterator],
        params: &FtsSearchParams,
    ) -> bool {
        params.phrase_slop.is_none()
            && matches!(postings, [posting] if posting.is_compressed() && !posting.has_grouped_terms())
    }

    /// `scorer` must give every clause's posting the same document weight as
    /// that clause's own scorer (equal `Scorer::doc_weight_cache_key`).
    pub(in super::super) fn new(
        must: PostingIterator,
        shoulds: Vec<PostingIterator>,
        documents: &'a D,
        scorer: Arc<MemBM25Scorer>,
        metrics: &'a dyn MetricsCollector,
    ) -> Self {
        let cost = must.cost().min(documents.visible_cost_upper_bound());
        let global_upper = conservative_score_sum(
            std::iter::once(&must)
                .chain(&shoulds)
                .map(|posting| posting.global_upper_bound(&scorer)),
        );
        let norms = documents.scoring_norms().zip(norm_addends(scorer.as_ref()));
        Self {
            must,
            shoulds,
            documents,
            scorer,
            norms,
            global_upper,
            cost,
            floor: f32::NEG_INFINITY,
            exhausted: false,
            hits: Vec::new(),
            docs: Vec::new(),
            scores: Vec::new(),
            should_sums: Vec::new(),
            should_matched: Vec::new(),
            should_freqs: Vec::new(),
            next_hit: 0,
            current: None,
            comparisons: 0,
            metrics_recorded: false,
            metrics,
        }
    }

    pub(in super::super) fn doc(&self) -> Option<u64> {
        self.current.map(|(doc, _, _)| doc)
    }

    pub(in super::super) fn document_key(&self) -> Option<u64> {
        self.current.map(|(_, key, _)| key)
    }

    pub(in super::super) fn current_score(&self) -> Option<f32> {
        self.current.map(|(_, _, score)| score)
    }

    pub(in super::super) fn cost(&self) -> usize {
        self.cost
    }

    pub(in super::super) fn raise_floor(&mut self, floor: f32) {
        self.floor = self.floor.max(floor);
    }

    pub(in super::super) fn next(&mut self) -> Result<Option<u64>> {
        loop {
            while self.next_hit < self.hits.len() {
                let index = self.next_hit;
                self.next_hit += 1;
                let score = self.scores[index];
                if score < self.floor {
                    continue;
                }
                let doc = self.hits[index].0;
                let Some(key) = self.documents.document_key_for_doc_id(doc as u32) else {
                    continue;
                };
                self.current = Some((doc, key, score));
                return Ok(Some(doc));
            }
            if !self.score_next_block()? {
                self.current = None;
                if !self.metrics_recorded {
                    self.metrics.record_comparisons(self.comparisons);
                    self.metrics_recorded = true;
                }
                return Ok(None);
            }
        }
    }

    pub(in super::super) fn advance(&mut self, target: u64) -> Result<Option<u64>> {
        if self.doc().is_some_and(|doc| doc >= target) {
            return Ok(self.doc());
        }
        while self
            .hits
            .get(self.next_hit)
            .is_some_and(|(doc, _)| *doc < target)
        {
            self.next_hit += 1;
        }
        if self.next_hit == self.hits.len()
            && self.must.current_doc_id().is_some_and(|doc| doc < target)
        {
            // Compressed postings hold u32 doc ids, so a larger target is past
            // every posting; `PostingIterator::next` would truncate it.
            if target > u64::from(u32::MAX) {
                self.exhausted = true;
            } else {
                self.must.next(target);
            }
        }
        self.next()
    }

    /// Conservative bound of `posting` over `[start, end]`.
    fn window_upper(
        posting: &mut PostingIterator,
        start: u64,
        end: u64,
        scorer: &MemBM25Scorer,
    ) -> f32 {
        let Some(from) = posting.current_doc_id().map(|doc| doc.max(start)) else {
            return 0.0;
        };
        if from > end {
            return 0.0;
        }
        posting.shallow_next(from);
        if posting.block_first_doc().is_some_and(|first| first > end) {
            return 0.0;
        }
        posting.block_max_score_up_to_with_stats(end, scorer).score
    }

    /// Scores the next MUST block that can still reach the floor into `hits`
    /// and `scores`. Returns false once nothing can.
    fn score_next_block(&mut self) -> Result<bool> {
        self.hits.clear();
        self.next_hit = 0;
        loop {
            let Some(start) = self.must.current_doc_id().filter(|_| !self.exhausted) else {
                return Ok(false);
            };
            if self.floor > 0.0 {
                if self.global_upper < self.floor {
                    return Ok(false);
                }
                self.must.shallow_next(start);
                let end = self.must.block_end_doc();
                let must_upper = self.must.block_max_score(self.scorer.as_ref());
                let scorer = self.scorer.as_ref();
                let upper = conservative_score_sum(
                    std::iter::once(must_upper).chain(
                        self.shoulds
                            .iter_mut()
                            .map(|should| Self::window_upper(should, start, end, scorer)),
                    ),
                );
                if upper < self.floor {
                    if !self.must.skip_current_block() {
                        return Ok(false);
                    }
                    continue;
                }
            }
            self.must.take_docs_one_block_upto(u64::MAX, &mut self.hits);
            if !self.hits.is_empty() {
                break;
            }
        }
        self.comparisons += self.hits.len();

        let count = self.hits.len();
        let Self {
            must,
            shoulds,
            documents,
            scorer,
            norms,
            hits,
            docs,
            scores,
            should_sums,
            should_matched,
            should_freqs,
            ..
        } = self;
        let norms = norms
            .as_ref()
            .map(|(codes, addends)| (*codes, addends.as_ref()));
        docs.clear();
        docs.extend(hits.iter().map(|(doc, _)| *doc));
        should_sums.clear();
        should_sums.resize(count, 0.0);
        should_matched.clear();
        should_matched.resize(count, false);
        should_freqs.resize(count, 0);
        for should in shoulds.iter_mut() {
            should.merge_optional_freqs(docs, should_freqs);
            for (slot, &freq) in should_freqs.iter().enumerate() {
                if freq != 0 {
                    should_sums[slot] +=
                        term_score(should, docs[slot], freq, norms, scorer, *documents);
                    should_matched[slot] = true;
                }
            }
        }

        scores.clear();
        for (slot, &(doc, freq)) in hits.iter().enumerate() {
            let must_score = 0.0_f32 + term_score(must, doc, freq, norms, scorer, *documents);
            let score = if should_matched[slot] {
                must_score + should_sums[slot]
            } else {
                must_score
            };
            if !score.is_finite() {
                return Err(Error::internal(format!(
                    "required-plus-optional FTS scorer produced a non-finite score: {score}"
                )));
            }
            scores.push(score);
        }
        Ok(true)
    }
}

/// Same expression as `PostingIterator::score` for a one-term posting.
fn term_score<D: WandDocuments>(
    posting: &PostingIterator,
    doc: u64,
    freq: u32,
    norms: Option<(&[u8], &[f32; 256])>,
    scorer: &Arc<MemBM25Scorer>,
    documents: &D,
) -> f32 {
    match norms {
        Some((codes, addends)) => {
            posting.query_weight
                * bm25_doc_weight_with_norm(freq, addends[codes[doc as usize] as usize])
        }
        None => posting.score(scorer, freq, documents.scoring_num_tokens(doc as u32)),
    }
}
