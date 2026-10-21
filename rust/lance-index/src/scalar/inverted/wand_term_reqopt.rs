// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! A `+must should...` Boolean query over its raw one-term postings.

use std::sync::Arc;

use lance_core::{Error, Result};

use super::super::query::FtsSearchParams;
use super::super::scorer::{MemBM25Scorer, bm25_doc_weight_with_norm};
use super::{PostingIterator, WandDocuments, conservative_score_sum, norm_addends};
use crate::metrics::MetricsCollector;

/// One or more MUST and one or more SHOULD one-term postings, scored a driver
/// block at a time.
///
/// Emits exactly the documents and score bits of the generic
/// `ReqOpt(Conjunction(MUST...), Disjunction(SHOULD...))` tree over the same
/// postings that can still reach the competitive floor: the MUST scores summed
/// in clause order from zero, plus the matched SHOULD scores summed the same way.
///
/// The cheapest MUST posting drives until the floor proves that every
/// competitive document also contains one cheaper SHOULD clause; from then on
/// that clause drives.
/// A driver block whose bound plus the other clauses' window bounds is below
/// the floor is skipped without decoding the other postings.
pub(in super::super) struct TermReqOptScorer<'a, D: WandDocuments> {
    /// The MUST postings, then the SHOULD postings, each in clause order.
    postings: Vec<PostingIterator>,
    musts: usize,
    global_uppers: Vec<f32>,
    driver: usize,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    /// Per-document norm codes and their BM25 denominator addends; scoring
    /// through them is bit-identical to `Scorer::doc_weight`.
    norms: Option<(&'a [u8], Arc<[f32; 256]>)>,
    floor: f32,
    exhausted: bool,
    hits: Vec<(u64, u32)>,
    docs: Vec<u64>,
    freqs: Vec<u32>,
    must_scores: Vec<Option<f32>>,
    should_sums: Vec<Option<f32>>,
    scores: Vec<f32>,
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
        musts: Vec<PostingIterator>,
        shoulds: Vec<PostingIterator>,
        documents: &'a D,
        scorer: Arc<MemBM25Scorer>,
        metrics: &'a dyn MetricsCollector,
    ) -> Self {
        let lead = (0..musts.len())
            .min_by_key(|&must| musts[must].cost())
            .unwrap_or_default();
        let musts_len = musts.len();
        let postings = musts.into_iter().chain(shoulds).collect::<Vec<_>>();
        let global_uppers = postings
            .iter()
            .map(|posting| posting.global_upper_bound(&scorer))
            .collect();
        let norms = documents.scoring_norms().zip(norm_addends(scorer.as_ref()));
        Self {
            postings,
            musts: musts_len,
            global_uppers,
            driver: lead,
            documents,
            scorer,
            norms,
            floor: f32::NEG_INFINITY,
            exhausted: false,
            hits: Vec::new(),
            docs: Vec::new(),
            freqs: Vec::new(),
            must_scores: Vec::new(),
            should_sums: Vec::new(),
            scores: Vec::new(),
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
        self.postings[..self.musts]
            .iter()
            .map(PostingIterator::cost)
            .min()
            .unwrap_or_default()
            .min(self.documents.visible_cost_upper_bound())
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
        if self.next_hit == self.hits.len() {
            self.seek_driver(target);
        }
        self.next()
    }

    fn seek_driver(&mut self, target: u64) {
        let driver = &mut self.postings[self.driver];
        if driver.current_doc_id().is_some_and(|doc| doc < target) {
            // Compressed postings hold u32 doc ids, so a larger target is past
            // every posting; `PostingIterator::next` would truncate it.
            if target > u64::from(u32::MAX) {
                self.exhausted = true;
            } else {
                driver.next(target);
            }
        }
    }

    /// Global bound of `clause`; an exhausted posting cannot add to any later
    /// document.
    fn live_upper(&self, clause: usize) -> f32 {
        if self.postings[clause].current_doc_id().is_some() {
            self.global_uppers[clause]
        } else {
            0.0
        }
    }

    /// Hands the drive to the rarest SHOULD clause, cheaper than the driving
    /// MUST clause, that every document still reaching the floor must contain:
    /// one without which the other clauses' bounds sum below the floor.
    fn promote_required_should(&mut self) {
        let lead_cost = self.postings[self.driver].cost();
        let required = (self.musts..self.postings.len())
            .filter(|&should| self.postings[should].cost() < lead_cost)
            .filter(|&should| {
                conservative_score_sum(
                    (0..self.postings.len())
                        .filter(|&clause| clause != should)
                        .map(|clause| self.live_upper(clause)),
                ) < self.floor
            })
            .min_by_key(|&should| self.postings[should].cost());
        if let Some(should) = required {
            let frontier = self.postings[self.driver].current_doc_id();
            self.driver = should;
            match frontier {
                Some(frontier) => self.seek_driver(frontier),
                None => self.exhausted = true,
            }
        }
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

    /// Takes the next driver block that can still reach the floor into `hits`.
    fn take_driver_block(&mut self) -> bool {
        loop {
            if self.exhausted
                || self.postings[..self.musts]
                    .iter()
                    .any(|must| must.current_doc_id().is_none())
            {
                return false;
            }
            if self.floor > 0.0 {
                let live = (0..self.postings.len()).map(|clause| self.live_upper(clause));
                if conservative_score_sum(live) < self.floor {
                    return false;
                }
                if self.driver < self.musts {
                    self.promote_required_should();
                    if self.exhausted {
                        return false;
                    }
                }
            }
            let driver = self.driver;
            let Some(start) = self.postings[driver].current_doc_id() else {
                return false;
            };
            if self.floor > 0.0 {
                let scorer = self.scorer.as_ref();
                self.postings[driver].shallow_next(start);
                let end = self.postings[driver].block_end_doc();
                let driver_upper = self.postings[driver].block_max_score(scorer);
                let upper = conservative_score_sum(
                    std::iter::once(driver_upper).chain(
                        self.postings
                            .iter_mut()
                            .enumerate()
                            .filter(|(clause, _)| *clause != driver)
                            .map(|(_, posting)| Self::window_upper(posting, start, end, scorer)),
                    ),
                );
                if upper < self.floor {
                    if !self.postings[driver].skip_current_block() {
                        return false;
                    }
                    continue;
                }
            }
            self.postings[driver].take_docs_one_block_upto(u64::MAX, &mut self.hits);
            if !self.hits.is_empty() {
                return true;
            }
        }
    }

    /// Scores the next driver block that can still reach the floor into `hits`
    /// and `scores`, keeping only documents that contain every MUST term.
    /// Returns false once nothing can.
    fn score_next_block(&mut self) -> Result<bool> {
        self.hits.clear();
        self.next_hit = 0;
        if !self.take_driver_block() {
            return Ok(false);
        }
        self.comparisons += self.hits.len();

        let count = self.hits.len();
        let driver = self.driver;
        let musts = self.musts;
        let Self {
            postings,
            documents,
            scorer,
            norms,
            hits,
            docs,
            freqs,
            must_scores,
            should_sums,
            scores,
            ..
        } = self;
        let norms = norms
            .as_ref()
            .map(|(codes, addends)| (*codes, addends.as_ref()));
        docs.clear();
        docs.extend(hits.iter().map(|(doc, _)| *doc));
        must_scores.clear();
        must_scores.resize(count, Some(0.0));
        should_sums.clear();
        should_sums.resize(count, None);
        for (clause, posting) in postings.iter_mut().enumerate() {
            freqs.clear();
            if clause == driver {
                freqs.extend(hits.iter().map(|(_, freq)| *freq));
            } else {
                freqs.resize(count, 0);
                posting.merge_optional_freqs(docs, freqs);
            }
            for (slot, &freq) in freqs.iter().enumerate() {
                if freq == 0 {
                    if clause < musts {
                        must_scores[slot] = None;
                    }
                    continue;
                }
                let score = term_score(posting, docs[slot], freq, norms, scorer, *documents);
                if clause < musts {
                    must_scores[slot] = must_scores[slot].map(|sum| sum + (0.0_f32 + score));
                } else {
                    should_sums[slot] = Some(should_sums[slot].unwrap_or(0.0) + score);
                }
            }
        }

        scores.clear();
        let mut kept = 0;
        for slot in 0..count {
            let Some(must_score) = must_scores[slot] else {
                continue;
            };
            let score = should_sums[slot].map_or(must_score, |should| must_score + should);
            if !score.is_finite() {
                return Err(Error::internal(format!(
                    "required-plus-optional FTS scorer produced a non-finite score: {score}"
                )));
            }
            hits[kept] = hits[slot];
            scores.push(score);
            kept += 1;
        }
        hits.truncate(kept);
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
