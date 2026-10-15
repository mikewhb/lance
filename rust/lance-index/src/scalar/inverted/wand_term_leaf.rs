// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! One-term Boolean Match leaf scorer: a leaf with exactly one ungrouped
//! posting steps its posting directly instead of paying the WAND wrapper.
//! See [`TermLeafScorer`] for the floor contract.

use std::cell::Cell;
use std::cell::OnceCell;
use std::sync::Arc;

use lance_core::{Error, Result};

use super::super::DocInfo;
use super::super::compound::{BlockBatch, ComposableScorer, ScoreBounds};
use super::super::scorer::MemBM25Scorer;
use crate::metrics::MetricsCollector;

use super::{PostingIterator, WandDocuments, conservative_score_sum, shallow_score_bound};

/// One-term Boolean Match leaf: posting GEQ for approximation, BM25 on `score()`.
///
/// Unlike [`WandCursor`], this is not a WAND machine: it never uses a
/// competitive floor to skip blocks or drop documents. `ReqOptScorer` holds
/// the floor itself and forwards it to neither side, so this scorer must keep
/// every hit with `required < floor ≤ required + optional`.
pub struct TermLeafScorer<'a, D: WandDocuments> {
    posting: PostingIterator,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    cost: usize,
    /// Reused frequency scratch for the fused fold_optional_scores pass
    /// (plain capacity; contents are fully rewritten per call).
    fold_scratch: Vec<u32>,
    /// Block-batch work steps taken by the batch profile (candidates taken,
    /// scored, key-resolved, folded or merged — a bulk pass counts its
    /// candidate slice, a merge adds one per live frequency, and the
    /// per-candidate calls count one each). Reported through
    /// `MetricsCollector::record_batch_steps` so batch routing is never
    /// inferred from the generic comparison counter.
    batch_steps: Cell<usize>,
    global_score_upper_bound: OnceCell<Option<f32>>,
    current_doc: Option<DocInfo>,
    current_document_key: Option<u64>,
    current_score: f32,
    score_ready: bool,
    shallow: Option<(u64, u64, f32)>,
    comparisons: usize,
    metrics_recorded: bool,
    metrics: &'a dyn MetricsCollector,
}

impl<'a, D: WandDocuments> TermLeafScorer<'a, D> {
    pub fn new(
        posting: PostingIterator,
        documents: &'a D,
        scorer: Arc<MemBM25Scorer>,
        metrics: &'a dyn MetricsCollector,
    ) -> Self {
        let cost = posting.cost().min(documents.visible_cost_upper_bound());
        Self {
            posting,
            documents,
            scorer,
            cost,
            fold_scratch: Vec::new(),
            batch_steps: Cell::new(0),
            global_score_upper_bound: OnceCell::new(),
            current_doc: None,
            current_document_key: None,
            current_score: 0.0,
            score_ready: false,
            shallow: None,
            comparisons: 0,
            metrics_recorded: false,
            metrics,
        }
    }

    pub fn doc(&self) -> Option<u64> {
        self.current_doc.map(|doc| doc.doc_id())
    }

    pub fn document_key(&self) -> Option<u64> {
        self.current_document_key
    }

    fn clear_current(&mut self) {
        self.current_doc = None;
        self.current_document_key = None;
        self.current_score = 0.0;
        self.score_ready = false;
        self.shallow = None;
    }

    fn record_metrics(&mut self) {
        if !self.metrics_recorded {
            self.metrics.record_comparisons(self.comparisons);
            let batch_steps = self.batch_steps.get();
            if batch_steps > 0 {
                self.metrics.record_batch_steps(batch_steps);
            }
            self.metrics_recorded = true;
        }
    }

    fn position_geq(&mut self, mut target: u64) -> Result<Option<u64>> {
        self.clear_current();
        loop {
            self.posting.next_doc_id(target, true);
            let Some(doc) = self.posting.current_doc else {
                self.record_metrics();
                return Ok(None);
            };
            self.comparisons += 1;
            let Some(document_key) = self.documents.document_key(&doc) else {
                target = doc.doc_id().saturating_add(1);
                continue;
            };
            self.current_doc = Some(doc);
            self.current_document_key = Some(document_key);
            self.current_score = 0.0;
            self.score_ready = false;
            return Ok(Some(doc.doc_id()));
        }
    }

    pub fn next(&mut self) -> Result<Option<u64>> {
        let target = match self.doc() {
            None => 0,
            Some(u64::MAX) => {
                self.clear_current();
                self.record_metrics();
                return Ok(None);
            }
            Some(current) => current.saturating_add(1),
        };
        self.position_geq(target)
    }

    pub fn advance(&mut self, target: u64) -> Result<Option<u64>> {
        if self.doc().is_some_and(|doc| doc >= target) {
            return Ok(self.doc());
        }
        self.position_geq(target)
    }

    pub fn cost(&self) -> usize {
        self.cost
    }

    pub fn global_score_upper_bound(&self) -> Option<f32> {
        *self.global_score_upper_bound.get_or_init(|| {
            if self.posting.has_grouped_terms() {
                return None;
            }
            let upper = conservative_score_sum(std::iter::once(
                self.posting.global_upper_bound(self.scorer.as_ref()),
            ));
            (upper.is_finite() && upper >= 0.0).then_some(upper)
        })
    }

    pub fn current_score(&mut self) -> Result<f32> {
        if self.current_doc.is_none() {
            return Err(Error::internal(
                "posting FTS scorer is not positioned on a document",
            ));
        }
        if self.score_ready {
            return Ok(self.current_score);
        }
        let scored = self.posting.doc().ok_or_else(|| {
            Error::internal("single-term posting is not positioned on a document")
        })?;
        let doc_length = self.documents.doc_length(&scored);
        let score = self
            .posting
            .score(self.scorer.as_ref(), scored.frequency(), doc_length);
        self.current_doc = Some(scored);
        self.current_score = score;
        self.score_ready = true;
        Ok(score)
    }

    pub fn advance_shallow(&mut self, target: u64) -> Result<u64> {
        self.posting.shallow_next(target);
        if let Some(first) = self.posting.block_first_doc()
            && first > target
        {
            let up_to = first.saturating_sub(1).max(target);
            self.shallow = Some((target, up_to, 0.0));
            return Ok(up_to);
        }
        let up_to = self.posting.block_end_doc().max(target);
        let upper = conservative_score_sum(std::iter::once(
            self.posting
                .window_max_score(Some(up_to), self.scorer.as_ref()),
        ));
        self.shallow = Some((target, up_to, upper));
        Ok(up_to)
    }

    pub fn score_upper_bound(&self, up_to: u64) -> Result<f32> {
        shallow_score_bound(self.shallow, up_to)
    }

    pub fn set_min_competitive_score(&mut self, min_score: f32) -> Result<()> {
        if min_score.is_nan() {
            return Err(Error::invalid_input(
                "minimum competitive FTS score cannot be NaN",
            ));
        }
        Ok(())
    }

    pub fn scored_upper_bound(&self) -> Option<f32> {
        self.score_ready.then_some(self.current_score)
    }

    #[cfg(test)]
    pub fn frequency_blocks_decoded(&self) -> usize {
        self.posting.frequency_blocks_decoded()
    }
}

impl<D: WandDocuments> Drop for TermLeafScorer<'_, D> {
    fn drop(&mut self) {
        self.record_metrics();
    }
}

/// Block-batch supply: drives the inner posting directly through the same
/// moves the batch state machines use (`take_docs_one_block_upto` /
/// `merge_optional_freqs` / `score(scorer, freq, scoring_num_tokens)`).
/// After batch stepping the
/// leaf's derived caches (the `current` family) are all invalidated — the
/// host state machine never reads them; resumption relies on pure-data
/// breakpoints.
impl<D: WandDocuments> BlockBatch for TermLeafScorer<'_, D> {
    fn document_key_for_doc_id(&self, doc_id: u32) -> Option<u64> {
        self.batch_steps.set(self.batch_steps.get() + 1);
        self.documents.document_key_for_doc_id(doc_id)
    }

    fn take_docs_one_block_upto(&mut self, upto: u64, out: &mut Vec<(u64, u32)>) {
        self.posting.take_docs_one_block_upto(upto, out);
        self.batch_steps.set(self.batch_steps.get() + out.len());
        self.clear_current();
    }

    fn merge_optional_freqs(&mut self, docs: &[u64], out_freqs: &mut [u32]) {
        self.posting.merge_optional_freqs(docs, out_freqs);
        self.batch_steps.set(
            self.batch_steps.get() + docs.len() + out_freqs.iter().filter(|f| **f != 0).count(),
        );
        self.clear_current();
    }

    fn fold_optional_scores(&mut self, docs: &[u64], sums: &mut [f32]) {
        // Fused form: merge into reused scratch, then one tight scoring loop
        // with scorer state hoisted out. merge_optional_freqs zeroes every
        // entry itself, so a plain resize (no clear) suffices.
        debug_assert_eq!(docs.len(), sums.len());
        self.batch_steps.set(self.batch_steps.get() + docs.len());
        self.fold_scratch.resize(docs.len(), 0);
        self.posting
            .merge_optional_freqs(docs, &mut self.fold_scratch);
        let scorer = self.scorer.as_ref();
        for (i, &freq) in self.fold_scratch.iter().enumerate() {
            if freq != 0 {
                sums[i] += self.posting.score(
                    scorer,
                    freq,
                    self.documents.scoring_num_tokens(docs[i] as u32),
                );
            }
        }
        self.clear_current();
    }

    fn current_doc_id(&self) -> Option<u64> {
        self.posting.current_doc_id()
    }

    fn seek_eq(&mut self, doc: u64) -> Option<u32> {
        self.batch_steps.set(self.batch_steps.get() + 1);
        if self.posting.doc().is_some_and(|info| info.doc_id() < doc) {
            self.posting.next(doc);
        }
        match self.posting.doc() {
            Some(info) if info.doc_id() == doc => Some(info.frequency()),
            _ => None,
        }
    }

    fn global_upper_bound(&self) -> f32 {
        self.posting.global_upper_bound(self.scorer.as_ref())
    }

    fn cost(&self) -> usize {
        // Raw posting df, matching what `PostingIterator::cost` returns and
        // what the shape router reads — not the visibility-truncated value
        // `TermLeafScorer::cost` returns for the same leaf.
        self.posting.cost()
    }

    fn term_score(&self, doc: u64, freq: u32) -> f32 {
        self.batch_steps.set(self.batch_steps.get() + 1);
        self.posting.score(
            self.scorer.as_ref(),
            freq,
            self.documents.scoring_num_tokens(doc as u32),
        )
    }

    fn term_scores_batch(&self, docs: &[u64], freqs: &[u32], out: &mut [f32]) {
        // Hoist scorer state out of the loop: identical per-entry arithmetic
        // to term_score, one dispatch per block.
        debug_assert_eq!(docs.len(), freqs.len());
        debug_assert_eq!(docs.len(), out.len());
        self.batch_steps.set(self.batch_steps.get() + docs.len());
        let scorer = self.scorer.as_ref();
        for ((&doc, &freq), slot) in docs.iter().zip(freqs.iter()).zip(out.iter_mut()) {
            *slot = if freq != 0 {
                self.posting
                    .score(scorer, freq, self.documents.scoring_num_tokens(doc as u32))
            } else {
                0.0
            };
        }
    }

    fn document_keys_batch(&self, docs: &[u64], out: &mut Vec<Option<u64>>) {
        self.batch_steps.set(self.batch_steps.get() + docs.len());
        out.clear();
        out.reserve(docs.len());
        for &doc in docs {
            out.push(self.documents.document_key_for_doc_id(doc as u32));
        }
    }

    fn sync_after_batch(&mut self) {
        self.clear_current();
    }
}

impl<D: WandDocuments + Sync> ComposableScorer for TermLeafScorer<'_, D> {
    fn block_batch(&mut self) -> Option<&mut dyn BlockBatch> {
        // The capability contract above is a *compressed* single-posting leaf:
        // batch steps address candidates as dense u32 doc-ids and resolve them
        // through `document_key_for_doc_id` / `scoring_num_tokens`. Plain
        // postings store row-ids in the legacy domain (a `DocInfo::Raw`
        // doc-id is a row-id there), so handing one to the batch FSM feeds
        // row-ids into dense-index lookups — a wrong mapping or an
        // out-of-bounds panic. Refuse here: this is the single capability
        // entry every batch consumer probes; the generic per-document walk
        // keeps using this leaf.
        if self.posting.is_compressed() {
            Some(self)
        } else {
            None
        }
    }

    fn doc(&self) -> Option<u64> {
        self.doc()
    }

    fn document_key(&self) -> Option<u64> {
        self.document_key()
    }

    fn next(&mut self) -> Result<Option<u64>> {
        self.next()
    }

    fn advance(&mut self, target: u64) -> Result<Option<u64>> {
        self.advance(target)
    }

    fn cost(&self) -> usize {
        self.cost()
    }

    fn score(&mut self) -> Result<f32> {
        self.current_score()
    }

    fn advance_shallow(&mut self, target: u64) -> Result<u64> {
        self.advance_shallow(target)
    }

    fn score_bounds(&mut self, up_to: u64) -> Result<ScoreBounds> {
        Ok(ScoreBounds {
            lower: 0.0,
            upper: self.score_upper_bound(up_to)?,
        })
    }

    fn global_score_upper_bound(&self) -> Option<f32> {
        TermLeafScorer::global_score_upper_bound(self)
    }

    fn set_min_competitive_score(&mut self, min_score: f32) -> Result<()> {
        self.set_min_competitive_score(min_score)
    }

    fn current_score_upper_bound(&mut self) -> Result<Option<f32>> {
        Ok(self.scored_upper_bound())
    }

    fn matches(&mut self) -> Result<bool> {
        Ok(self.doc().is_some())
    }

    fn scores_non_negative(&self) -> bool {
        true
    }
}
