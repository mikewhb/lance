// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Single-posting leaf for one-term clauses of a `+a b ...` Boolean query.

use std::cell::OnceCell;
use std::sync::Arc;

use lance_core::{Error, Result};

use super::super::query::FtsSearchParams;
use super::super::scorer::MemBM25Scorer;
use super::{DocInfo, PostingIterator, WandDocuments, conservative_score_sum};
use crate::metrics::MetricsCollector;

/// A one-term Match leaf that steps its posting directly.
///
/// It produces the same documents, keys and scores as a [`super::WandCursor`]
/// over the same posting, without the WAND heaps. It ignores the competitive
/// floor; pruning is left to the parent's window bounds.
pub(in super::super) struct TermLeaf<'a, D: WandDocuments> {
    posting: PostingIterator,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    cost: usize,
    global_score_upper_bound: OnceCell<Option<f32>>,
    current_doc: Option<DocInfo>,
    current_document_key: Option<u64>,
    /// Computed on first use; most positioned documents are never scored.
    current_score: Option<f32>,
    /// `(target, up_to, upper)` of the last `advance_shallow`.
    shallow: Option<(u64, u64, f32)>,
    comparisons: usize,
    metrics_recorded: bool,
    metrics: &'a dyn MetricsCollector,
}

impl<'a, D: WandDocuments> TermLeaf<'a, D> {
    /// Whether `postings` can be served by a `TermLeaf` with identical results.
    pub(in super::super) fn accepts(
        postings: &[PostingIterator],
        params: &FtsSearchParams,
    ) -> bool {
        params.phrase_slop.is_none()
            && matches!(postings, [posting] if posting.is_compressed() && !posting.has_grouped_terms())
    }

    pub(in super::super) fn new(
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
            global_score_upper_bound: OnceCell::new(),
            current_doc: None,
            current_document_key: None,
            current_score: None,
            shallow: None,
            comparisons: 0,
            metrics_recorded: false,
            metrics,
        }
    }

    fn record_metrics(&mut self) {
        if !self.metrics_recorded {
            self.metrics.record_comparisons(self.comparisons);
            self.metrics_recorded = true;
        }
    }

    fn position(&mut self, mut target: u64) -> Option<u64> {
        self.current_doc = None;
        self.current_document_key = None;
        self.current_score = None;
        self.shallow = None;
        loop {
            // Compressed postings hold u32 doc ids, so a larger target is past
            // every posting; `PostingIterator::next` would truncate it.
            if target > u64::from(u32::MAX) {
                self.record_metrics();
                return None;
            }
            self.posting.next(target);
            let Some(doc) = self.posting.doc() else {
                self.record_metrics();
                return None;
            };
            self.comparisons += 1;
            let doc_id = doc.doc_id();
            let Some(document_key) = self.documents.document_key(&doc) else {
                target = doc_id.saturating_add(1);
                continue;
            };
            self.current_doc = Some(doc);
            self.current_document_key = Some(document_key);
            return Some(doc_id);
        }
    }
    pub(in super::super) fn doc(&self) -> Option<u64> {
        self.current_doc.map(|doc| doc.doc_id())
    }

    pub(in super::super) fn document_key(&self) -> Option<u64> {
        self.current_document_key
    }

    pub(in super::super) fn next(&mut self) -> Option<u64> {
        let target = match self.doc() {
            None if self.comparisons == 0 => 0,
            None => return None,
            Some(doc) => doc.saturating_add(1),
        };
        self.position(target)
    }

    pub(in super::super) fn advance(&mut self, target: u64) -> Option<u64> {
        if self.doc().is_some_and(|doc| doc >= target) {
            return self.doc();
        }
        self.position(target)
    }

    pub(in super::super) fn cost(&self) -> usize {
        self.cost
    }

    pub(in super::super) fn score(&mut self) -> Result<f32> {
        if let Some(score) = self.current_score {
            return Ok(score);
        }
        let doc = self
            .current_doc
            .ok_or_else(|| Error::internal("posting FTS scorer is not positioned on a document"))?;
        let doc_length = self.documents.doc_length(&doc);
        // Same fold-from-zero as `Wand::score_in_query_order`, so the bits match.
        let score = 0.0_f32
            + self
                .posting
                .score(&self.scorer, doc.frequency(), doc_length);
        self.current_score = Some(score);
        Ok(score)
    }

    pub(in super::super) fn advance_shallow(&mut self, target: u64) -> u64 {
        self.posting.shallow_next(target);
        let (up_to, upper) = match self.posting.block_first_doc() {
            // `target` falls in the gap before the anchored block: no postings.
            Some(first) if first > target => (first.saturating_sub(1).max(target), 0.0),
            _ => {
                let up_to = self.posting.block_end_doc().max(target);
                let upper = conservative_score_sum(std::iter::once(
                    self.posting.window_max_score(Some(up_to), &self.scorer),
                ));
                (up_to, upper)
            }
        };
        self.shallow = Some((target, up_to, upper));
        up_to
    }

    pub(in super::super) fn score_upper_bound(&self, up_to: u64) -> Result<f32> {
        let (target, shallow_up_to, upper) = self.shallow.ok_or_else(|| {
            Error::internal("score bound requires advance_shallow on the posting FTS scorer")
        })?;
        if up_to < target || up_to > shallow_up_to {
            return Err(Error::internal(format!(
                "posting FTS score bound up_to={up_to} is outside shallow range [{target}, {shallow_up_to}]"
            )));
        }
        Ok(upper)
    }

    pub(in super::super) fn global_score_upper_bound(&self) -> Option<f32> {
        *self.global_score_upper_bound.get_or_init(|| {
            let upper = conservative_score_sum(std::iter::once(
                self.posting.global_upper_bound(&self.scorer),
            ));
            (upper.is_finite() && upper >= 0.0).then_some(upper)
        })
    }
}

impl<D: WandDocuments> Drop for TermLeaf<'_, D> {
    fn drop(&mut self) {
        self.record_metrics();
    }
}
