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
/// This is the C1b/C1c kernel: it never touches the terminal heap, threshold,
/// or essential-clause policy. The caller owns the posting ownership and the
/// candidate block.
#[allow(dead_code)]
pub(super) struct OrUnionKernel<'a, S: Scorer + ?Sized, D: WandDocuments> {
    postings: Vec<Box<PostingIterator>>,
    score_order: Vec<usize>,
    documents: &'a D,
    scorer: &'a S,
}

#[allow(dead_code)]
impl<'a, S: Scorer + ?Sized, D: WandDocuments> OrUnionKernel<'a, S, D> {
    pub(super) fn new(
        postings: Vec<Box<PostingIterator>>,
        score_order: Vec<usize>,
        documents: &'a D,
        scorer: &'a S,
    ) -> Self {
        Self {
            postings,
            score_order,
            documents,
            scorer,
        }
    }

    pub(super) fn caps(&self) -> crate::scalar::inverted::block::Capacities {
        use crate::scalar::inverted::PostingList;
        use crate::scalar::inverted::block::Capacities;
        let compressed = self.postings.iter().all(|posting| posting.is_compressed());
        let grouped = self
            .postings
            .iter()
            .any(|posting| posting.has_grouped_terms());
        let block_size = self
            .postings
            .first()
            .and_then(|posting| match &posting.list {
                PostingList::Compressed(list) => Some(list.block_size),
                PostingList::Plain(_) => None,
            })
            .unwrap_or(0);
        Capacities {
            compressed,
            grouped,
            phrase: false,
            scores_non_negative: true,
            block_size,
            window_bounds: compressed && matches!(block_size, 128 | 256),
        }
    }

    pub(super) fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        let mut first: Option<u64> = None;
        for posting in &mut self.postings {
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
        for posting in &mut self.postings {
            posting.shallow_next(target);
            up_to = up_to.min(posting.block_end_doc());
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
        use crate::scalar::inverted::block::BlockStatus;
        if budget == 0 {
            return Err(lance_core::Error::invalid_input(
                "UBE block budget must be at least 1",
            ));
        }
        let up_to = self
            .block_end_at(target)?
            .min(target.saturating_add(budget as u64 - 1));
        if up_to < target {
            return Ok(BlockStatus::Empty { up_to: target });
        }
        let mut acc = WindowAccumulator::new(self.postings.len());
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
            &mut acc,
        );
        out.clear();
        out.start = target;
        out.up_to = up_to;
        out.clause_count = self.postings.len();
        for slot in 0..=(up_to - target) as usize {
            if acc.words[slot >> 6] & (1u64 << (slot & 63)) == 0 {
                continue;
            }
            let doc = target + slot as u64;
            let Some(key) = self.documents.document_key_for_doc_id(doc as u32) else {
                acc.clear_slot(slot);
                continue;
            };
            out.docs.push(doc);
            out.scores.push(acc.scores[slot]);
            out.keys.push(key);
            out.matched.push(true);
            for clause in 0..self.postings.len() {
                out.freqs.push(acc.clause_freq(clause, slot));
            }
            acc.clear_slot(slot);
        }
        acc.words.fill(0);
        out.upper = self.upper_bound(&crate::scalar::inverted::block::Window {
            start: target,
            up_to,
        })?;
        if out.docs.is_empty() {
            Ok(BlockStatus::Empty { up_to })
        } else {
            Ok(BlockStatus::Data)
        }
    }

    pub(super) fn eval_at(
        &mut self,
        docs: &[u64],
        out: &mut crate::scalar::inverted::block::EvalOut,
    ) -> Result<()> {
        out.clear(docs.len());
        for (index, &doc) in docs.iter().enumerate() {
            let mut total = 0.0_f32;
            let mut matched = false;
            for &clause in &self.score_order {
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
                let doc_length = self.documents.scoring_num_tokens(doc as u32);
                total += posting.score(self.scorer, current.frequency(), doc_length);
            }
            out.scores[index] = total;
            out.matched[index] = matched;
        }
        out.exhausted = self.postings.iter().all(|posting| posting.doc().is_none());
        Ok(())
    }
}
