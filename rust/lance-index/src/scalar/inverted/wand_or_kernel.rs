// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Window union body for the bulk disjunction route.
//!
//! A disjunction leaf keeps one posting cursor per clause. [`fill_window`]
//! streams a doc range out of those cursors into a `WindowAccumulator`: each
//! clause adds its scores and frequencies to the slot of the doc that produced
//! them, and every cursor is left on the first doc beyond the range.
//!
//! The kernel is a pure sink over the accumulator. It never reads or updates
//! top-k, the threshold, or the heap; it never skips a doc, reorders clauses,
//! or decides which clause is essential. The caller owns the window bounds,
//! the accumulator lifetime, and every ranking decision.
//!
//! Accumulator precondition: the window's slots are already cleared, so `add`
//! only has to set the slot bit and add the contribution in. Callers clear a
//! slot when they drain it, which is what keeps a reused accumulator clean
//! from window to window.

use lance_core::Result;

use super::super::scorer::Scorer;
use super::{MAXSCORE_INNER_WINDOW, PostingIterator, WandDocuments, WindowAccumulator};

/// Accumulate one window of a disjunction leaf.
///
/// `postings` yields `(clause index, cursor)` pairs in the order the caller
/// wants the clauses folded into the accumulator. The index is the
/// accumulator's clause slot, so a caller streaming a suffix of a wider clause
/// set keeps its global numbering instead of renumbering from zero.
///
/// `window_min` and `up_to` are inclusive doc-id bounds. Slots are indexed by
/// `doc_id - window_min`, so `up_to - window_min` must stay below the
/// accumulator's window length.
pub(super) fn fill_window<'a, S, D, I>(
    postings: I,
    window_min: u64,
    up_to: u64,
    documents: &D,
    scorer: &S,
    norm_k: Option<(&[u8], &[f32; 256])>,
    acc: &mut WindowAccumulator,
) where
    S: Scorer + ?Sized,
    D: WandDocuments,
    I: IntoIterator<Item = (usize, &'a mut PostingIterator)>,
{
    debug_assert!(window_min <= up_to);
    debug_assert!((up_to - window_min) < MAXSCORE_INNER_WINDOW as u64);
    for (clause_idx, posting) in postings {
        posting.collect_window_scores(
            window_min, up_to, clause_idx, documents, scorer, norm_k, acc,
        );
    }
}

/// Reusable OR window kernel over a flat posting list.
///
/// This is the block-source kernel: it never touches the terminal heap,
/// threshold, or essential-clause policy. The caller owns the postings and the
/// accumulator, so a leaf keeps both across windows instead of rebuilding
/// them.
pub(super) struct OrUnionKernel<'a, 's, S: Scorer + ?Sized, D: WandDocuments> {
    postings: &'a mut [Box<PostingIterator>],
    /// Clause indices in query order: ascending `(position, token_id)`.
    score_order: &'a [usize],
    documents: &'a D,
    scorer: &'a S,
    acc: &'s mut WindowAccumulator,
}

impl<'a, 's, S: Scorer + ?Sized, D: WandDocuments> OrUnionKernel<'a, 's, S, D> {
    pub(super) fn new(
        postings: &'a mut [Box<PostingIterator>],
        score_order: &'a [usize],
        documents: &'a D,
        scorer: &'a S,
        acc: &'s mut WindowAccumulator,
    ) -> Self {
        Self {
            postings,
            score_order,
            documents,
            scorer,
            acc,
        }
    }

    pub(super) fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        let mut first: Option<u64> = None;
        for posting in self.postings.iter_mut() {
            if posting.doc().is_none_or(|doc| doc.doc_id() < target) {
                posting.next(target);
            }
            if let Some(doc) = posting.doc() {
                let doc_id = doc.doc_id();
                first = Some(first.map_or(doc_id, |current| current.min(doc_id)));
            }
        }
        Ok(first)
    }

    pub(super) fn block_end_at(&mut self, target: u64) -> Result<u64> {
        let mut up_to = u64::MAX;
        for posting in self.postings.iter_mut() {
            posting.shallow_next(target);
            up_to = up_to.min(super::and_kernel::posting_block_up_to(posting, target));
        }
        Ok(up_to)
    }

    pub(super) fn upper_bound(
        &mut self,
        window: &crate::scalar::inverted::block::Window,
    ) -> Result<crate::scalar::inverted::compound::ScoreBounds> {
        use crate::scalar::inverted::compound::ScoreBounds;
        let upper = super::conservative_score_sum(
            self.postings
                .iter()
                .map(|posting| posting.window_max_score(Some(window.up_to), self.scorer)),
        );
        ScoreBounds::try_new(0.0, upper)
    }

    pub(super) fn next_block(
        &mut self,
        target: u64,
        budget: usize,
        out: &mut crate::scalar::inverted::block::BlockBuf,
    ) -> Result<crate::scalar::inverted::block::BlockStatus> {
        use crate::scalar::inverted::block::{BlockStatus, Window};
        if budget == 0 {
            return Err(lance_core::Error::invalid_input(
                "FTS block request budget must be at least 1",
            ));
        }
        if self.postings.is_empty() {
            return Ok(BlockStatus::Exhausted);
        }
        // The accumulator has one slot per document of a fixed-length window,
        // so a wider budget cannot be honoured.
        let window_len = budget.min(MAXSCORE_INNER_WINDOW);
        // A block end is never below the target, so the window is non-empty.
        let up_to = self
            .block_end_at(target)?
            .min(target.saturating_add(window_len as u64 - 1));
        // The bound must be read before the scan below leaves the window.
        let upper = self.upper_bound(&Window {
            start: target,
            up_to,
        })?;
        fill_window(
            self.postings
                .iter_mut()
                .enumerate()
                .map(|(index, posting)| (index, posting.as_mut())),
            target,
            up_to,
            self.documents,
            self.scorer,
            None,
            self.acc,
        );
        out.clear();
        out.start = target;
        out.up_to = up_to;
        out.clause_count = self.postings.len();
        for slot in 0..=(up_to - target) as usize {
            if self.acc.words[slot >> 6] & (1u64 << (slot & 63)) == 0 {
                continue;
            }
            let doc = target + slot as u64;
            let Some(key) = self.documents.document_key_for_doc_id(doc as u32) else {
                self.acc.clear_slot(slot);
                continue;
            };
            out.docs.push(doc);
            out.scores.push(self.acc.scores[slot]);
            out.keys.push(key);
            out.matched.push(true);
            for clause in 0..self.postings.len() {
                out.freqs.push(self.acc.clause_freq(clause, slot));
            }
            self.acc.clear_slot(slot);
        }
        self.acc.words.fill(0);
        out.upper = upper;
        if !out.docs.is_empty() {
            return Ok(BlockStatus::Data);
        }
        // The scan left every clause on its first document beyond the window.
        // With none left, no later window can hold a document, and the block
        // boundary alone cannot say so: the last block reports no next-block
        // start, so its end is the caller's budget rather than the list end.
        if self.postings.iter().any(|posting| posting.doc().is_some()) {
            Ok(BlockStatus::Empty { up_to })
        } else {
            Ok(BlockStatus::Exhausted)
        }
    }

    pub(super) fn eval_at(
        &mut self,
        docs: &[u64],
        out: &mut crate::scalar::inverted::block::EvalOut,
    ) -> Result<()> {
        out.clear(docs.len());
        for (index, &doc) in docs.iter().enumerate() {
            let doc_length = self.documents.scoring_num_tokens(doc as u32);
            let mut total = 0.0_f32;
            let mut matched = false;
            for &clause in self.score_order {
                let posting = &mut self.postings[clause];
                if posting.doc().is_none_or(|current| current.doc_id() < doc) {
                    posting.next(doc);
                }
                let Some(current) = posting.doc() else {
                    continue;
                };
                if current.doc_id() != doc {
                    continue;
                }
                matched = true;
                total += posting.score(self.scorer, current.frequency(), doc_length);
            }
            out.scores[index] = total;
            out.matched[index] = matched;
        }
        out.exhausted = self.postings.iter().all(|posting| posting.doc().is_none());
        Ok(())
    }
}
