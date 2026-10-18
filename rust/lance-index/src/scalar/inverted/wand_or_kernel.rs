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
