// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Flat posting source for a multi-clause leaf that serves block execution.
//!
//! A leaf's postings normally live inside `Wand`'s head/lead/tail heaps,
//! which cannot hand a stable `&mut [Box<PostingIterator>]` to a window kernel.
//! When a leaf is eligible for block execution this set takes ownership of the
//! postings at construction and answers both faces the scorer tree needs:
//!
//! - the block face: [`BlockScorer`] (`seek`, `next_block`, `block_end_at`,
//!   `upper_bound`) plus [`BatchEvaluator::eval_at`];
//! - the per-document face: `seek` positions the cursors and `eval_at` totals
//!   the document, which is how a cursor answers `next`/`advance`/`score`.
//!
//! Both faces share the bulk kernels (`open_window` + `merge_window` for
//! conjunctions, `fill_window` for disjunctions), so the documents and the
//! f32 score totals are the same additions the classic per-document cursor
//! performs. Nothing here prunes: a competitive floor belongs to the caller,
//! which keeps emitted documents independent of visiting order.

use std::sync::Arc;

use lance_core::Result;

use super::super::block::{
    BatchEvaluator, BlockBuf, BlockScorer, BlockStatus, Capacities, EvalOut, Window,
};
use super::super::compound::{BlockBatch, ScoreBounds};
use super::super::query::Operator;
use super::super::scorer::MemBM25Scorer;
use super::and_kernel::{AndMergeKernel, AndWindowScratch};
use super::or_kernel::OrUnionKernel;
use super::{
    PostingIterator, PostingList, WandDocuments, WindowAccumulator, conservative_score_sum,
};

/// Compressed posting-block sizes that carry a true window bound.
const WINDOW_BOUND_BLOCK_SIZES: [usize; 2] = [128, 256];

/// Largest document id a compressed posting can encode.
const MAX_DOC_ID: u64 = u32::MAX as u64;

/// Owned, flat posting list behind a multi-clause leaf's block face.
pub(super) struct BlockPostingSet<'a, D: WandDocuments> {
    /// Clause postings. Disjunctions keep the order the query listed them in,
    /// so the window accumulator sums contributions in query order;
    /// conjunctions sort by cost, the same lead choice `Wand::new` makes.
    /// Boxed like the classic cursor's clauses so a window kernel can hand out
    /// a stable `&mut [Box<PostingIterator>]`.
    #[allow(clippy::vec_box)]
    postings: Vec<Box<PostingIterator>>,
    /// Clause indices in query order: ascending `(position, token_id)`.
    score_order: Vec<usize>,
    operator: Operator,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    caps: Capacities,
    acc: WindowAccumulator,
    and_scratch: AndWindowScratch,
    eval: EvalOut,
    /// Reused frequency scratch for the fused optional fold. Contents are
    /// fully rewritten by `merge_optional_freqs` per call.
    fold_freqs: Vec<u32>,
}

/// Capabilities of `postings`, computed before they are moved into a block
/// source.
///
/// `dense_doc_ids` reports whether the document set indexes its scoring
/// lengths by the posting's doc id; row-address document sets answer in a
/// different key space and keep the per-document path.
pub(super) fn caps_for(
    postings: &[PostingIterator],
    phrase: bool,
    dense_doc_ids: bool,
) -> Capacities {
    // Window opening decompresses one block per clause and then slices it, so
    // it needs a live cursor for every clause: a clause that is already empty
    // has no block to open and keeps the leaf on the per-document path, as the
    // conjunction terminal does.
    let compressed = dense_doc_ids
        && !postings.is_empty()
        && postings
            .iter()
            .all(|posting| posting.is_compressed() && posting.current_doc_id().is_some());
    let grouped = postings.iter().any(|posting| posting.has_grouped_terms());
    let block_size = postings
        .iter()
        .find_map(|posting| match &posting.list {
            PostingList::Compressed(list) => Some(list.block_size),
            PostingList::Plain(_) => None,
        })
        .unwrap_or(0);
    let window_bounds = compressed
        && postings.iter().all(|posting| match &posting.list {
            PostingList::Compressed(list) => WINDOW_BOUND_BLOCK_SIZES.contains(&list.block_size),
            PostingList::Plain(_) => false,
        });
    Capacities {
        compressed,
        grouped,
        phrase,
        scores_non_negative: true,
        block_size,
        window_bounds,
    }
}

impl<'a, D: WandDocuments> BlockPostingSet<'a, D> {
    pub(super) fn new(
        operator: Operator,
        postings: Vec<PostingIterator>,
        documents: &'a D,
        scorer: Arc<MemBM25Scorer>,
        caps: Capacities,
    ) -> Self {
        // A clause with no document cannot contribute to any candidate; the
        // classic cursor drops it too, and `num_terms`/`score_order` are built
        // from what survives.
        let mut postings = postings
            .into_iter()
            .filter(|posting| posting.current_doc_id().is_some())
            .collect::<Vec<_>>();
        if operator == Operator::And {
            postings.sort_unstable_by_key(|posting| posting.cost());
        } else {
            // The OR window kernel accumulates in array order. Keep the array
            // in query order so it matches `score_order` and `eval_at`.
            postings.sort_unstable_by_key(|posting| (posting.position, posting.token_id));
        }
        let num_terms = postings.len();
        let mut score_order = (0..num_terms).collect::<Vec<_>>();
        score_order.sort_unstable_by_key(|&index| {
            let posting = &postings[index];
            (posting.position, posting.token_id)
        });
        Self {
            postings: postings.into_iter().map(Box::new).collect(),
            score_order,
            operator,
            documents,
            scorer,
            caps,
            acc: WindowAccumulator::new(num_terms),
            and_scratch: AndWindowScratch::new(num_terms),
            eval: EvalOut::default(),
            fold_freqs: Vec::new(),
        }
    }

    pub(super) fn operator(&self) -> Operator {
        self.operator
    }

    pub(super) fn num_terms(&self) -> usize {
        self.postings.len()
    }

    /// Visible document key of `doc`, or `None` when the document is filtered
    /// out of this partition.
    pub(super) fn document_key(&self, doc: u64) -> Option<u64> {
        u32::try_from(doc)
            .ok()
            .and_then(|doc| self.documents.document_key_for_doc_id(doc))
    }

    /// Total score of `doc`, in query order.
    ///
    /// `doc` must not be below the clause cursors; the caller positions them
    /// with [`BlockScorer::seek`] first.
    pub(super) fn score_at(&mut self, doc: u64) -> Result<f32> {
        let mut eval = std::mem::take(&mut self.eval);
        let result = self.eval_at(&[doc], &mut eval);
        let score = eval.scores.first().copied().unwrap_or(0.0);
        self.eval = eval;
        result.map(|()| score)
    }

    /// Conservative list-wide score bound, independent of cursor position.
    /// `None` when a clause cannot supply one, which keeps the caller on exact
    /// composition.
    pub(super) fn global_upper_bound(&self) -> Option<f32> {
        // Grouped expansions score their terms separately, while their union
        // posting stores one aggregate bound that is not proven conservative
        // for that exact sum.
        if self
            .postings
            .iter()
            .any(|posting| posting.has_grouped_terms())
        {
            return None;
        }
        let upper = conservative_score_sum(
            self.postings
                .iter()
                .map(|posting| posting.global_upper_bound(&self.scorer)),
        );
        (upper.is_finite() && upper >= 0.0).then_some(upper)
    }

    fn and_kernel(&mut self) -> AndMergeKernel<'_, '_, MemBM25Scorer, D> {
        AndMergeKernel::new(
            &mut self.postings,
            &self.score_order,
            self.documents,
            &self.scorer,
            &mut self.and_scratch,
        )
    }

    fn or_kernel(&mut self) -> OrUnionKernel<'_, '_, MemBM25Scorer, D> {
        OrUnionKernel::new(
            &mut self.postings,
            &self.score_order,
            self.documents,
            &self.scorer,
            &mut self.acc,
        )
    }

    /// Block-mode `next`/`advance`: the first visible document at or after
    /// `target`, with its key and query-order score.
    ///
    /// Clause cursors only move forward, so `target` must be non-decreasing
    /// across calls; a target past the u32 document domain ends the walk
    /// instead of seeking past the encodable range.
    pub(super) fn next_visible(&mut self, target: u64) -> Result<Option<(u64, u64, f32)>> {
        let mut target = target;
        while target <= MAX_DOC_ID {
            let Some(doc) = BlockScorer::seek(self, target)? else {
                return Ok(None);
            };
            if let Some(key) = self.document_key(doc) {
                let score = self.score_at(doc)?;
                return Ok(Some((doc, key, score)));
            }
            target = doc.saturating_add(1);
        }
        Ok(None)
    }
}

impl<D: WandDocuments> BatchEvaluator for BlockPostingSet<'_, D> {
    fn eval_at(&mut self, docs: &[u64], out: &mut EvalOut) -> Result<()> {
        match self.operator {
            Operator::Or => self.or_kernel().eval_at(docs, out),
            Operator::And => {
                out.clear(docs.len());
                for (index, &doc) in docs.iter().enumerate() {
                    let doc_length = self.documents.scoring_num_tokens(doc as u32);
                    let mut total = 0.0_f32;
                    let mut matched = true;
                    // Membership needs every clause, so the first clause
                    // without this document ends the total: an unmatched
                    // document's partial score is never read.
                    for &clause in &self.score_order {
                        let posting = &mut self.postings[clause];
                        if posting.doc().is_none_or(|current| current.doc_id() < doc) {
                            posting.next(doc);
                        }
                        let Some(current) = posting.doc() else {
                            matched = false;
                            break;
                        };
                        if current.doc_id() != doc {
                            matched = false;
                            break;
                        }
                        total += posting.score(&self.scorer, current.frequency(), doc_length);
                    }
                    out.scores[index] = total;
                    out.matched[index] = matched;
                }
                // A conjunction is exhausted as soon as any required clause
                // has no further document.
                out.exhausted = self.postings.iter().any(|posting| posting.doc().is_none());
                Ok(())
            }
        }
    }
}

impl<D: WandDocuments> BlockScorer for BlockPostingSet<'_, D> {
    fn caps(&self) -> Capacities {
        self.caps
    }

    /// A target past the u32 document domain leaves nothing to seek to.
    fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        if target > MAX_DOC_ID {
            return Ok(None);
        }
        match self.operator {
            Operator::Or => self.or_kernel().seek(target),
            Operator::And => self.and_kernel().seek(target),
        }
    }

    fn next_block(
        &mut self,
        target: u64,
        budget: usize,
        out: &mut BlockBuf,
    ) -> Result<BlockStatus> {
        if target > MAX_DOC_ID {
            return Ok(BlockStatus::Exhausted);
        }
        match self.operator {
            Operator::Or => self.or_kernel().next_block(target, budget, out),
            Operator::And => self.and_kernel().next_block(target, budget, out),
        }
    }

    /// Inclusive end of the block holding `target`, taken over every clause.
    fn block_end_at(&mut self, target: u64) -> Result<u64> {
        if target > MAX_DOC_ID {
            return Ok(u64::MAX);
        }
        match self.operator {
            Operator::Or => self.or_kernel().block_end_at(target),
            Operator::And => self.and_kernel().block_end_at(target),
        }
    }

    /// A window starting past the u32 document domain holds no document, so
    /// any finite bound covers it.
    fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds> {
        if window.start > MAX_DOC_ID {
            return Ok(ScoreBounds::ZERO);
        }
        match self.operator {
            Operator::Or => self.or_kernel().upper_bound(window),
            Operator::And => self.and_kernel().upper_bound(window),
        }
    }
}

impl<D: WandDocuments> BlockBatch for BlockPostingSet<'_, D> {
    fn take_docs_one_block_upto(&mut self, upto: u64, out: &mut Vec<(u64, u32)>) {
        if self.postings.len() != 1 {
            out.clear();
            return;
        }
        self.postings[0].take_docs_one_block_upto(upto, out);
    }

    fn term_scores_batch(&self, docs: &[u64], freqs: &[u32], out: &mut [f32]) {
        debug_assert_eq!(docs.len(), freqs.len());
        debug_assert_eq!(docs.len(), out.len());
        if self.postings.len() != 1 {
            out.fill(0.0);
            return;
        }
        let posting = &self.postings[0];
        for i in 0..docs.len() {
            let freq = freqs[i];
            out[i] = if freq == 0 {
                0.0
            } else {
                posting.score(
                    &self.scorer,
                    freq,
                    self.documents.scoring_num_tokens(docs[i] as u32),
                )
            };
        }
    }

    fn document_keys_batch(&self, docs: &[u64], out: &mut Vec<Option<u64>>) {
        out.clear();
        out.extend(
            docs.iter()
                .map(|&doc| self.documents.document_key_for_doc_id(doc as u32)),
        );
    }

    fn fold_optional_scores(&mut self, docs: &[u64], sums: &mut [f32]) {
        debug_assert_eq!(docs.len(), sums.len());
        if self.postings.len() != 1 {
            return;
        }
        self.fold_freqs.clear();
        self.fold_freqs.resize(docs.len(), 0);
        self.postings[0].merge_optional_freqs(docs, &mut self.fold_freqs);
        for i in 0..docs.len() {
            let freq = self.fold_freqs[i];
            if freq != 0 {
                sums[i] += self.postings[0].score(
                    &self.scorer,
                    freq,
                    self.documents.scoring_num_tokens(docs[i] as u32),
                );
            }
        }
    }

    fn seek_eq(&mut self, doc: u64) -> Option<u32> {
        if self.postings.len() != 1 {
            return None;
        }
        self.postings[0].next(doc);
        match self.postings[0].doc() {
            Some(current) if current.doc_id() == doc => Some(current.frequency()),
            _ => None,
        }
    }

    fn term_score(&self, doc: u64, freq: u32) -> f32 {
        if self.postings.len() != 1 {
            return 0.0;
        }
        self.postings[0].score(
            &self.scorer,
            freq,
            self.documents.scoring_num_tokens(doc as u32),
        )
    }

    fn current_doc_id(&self) -> Option<u64> {
        self.postings[0].current_doc_id()
    }

    fn global_upper_bound(&self) -> Option<f32> {
        if self.postings.len() != 1 {
            return None;
        }
        // Grouped expansions score their terms separately; their union
        // posting's aggregate bound is not proven conservative for that sum.
        if self.postings[0].has_grouped_terms() {
            return None;
        }
        let upper = self.postings[0].global_upper_bound(&self.scorer);
        (upper.is_finite() && upper >= 0.0).then_some(upper)
    }

    fn cost(&self) -> usize {
        self.postings[0].cost()
    }
}
