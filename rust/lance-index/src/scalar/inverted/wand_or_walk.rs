// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Windowed disjunction walk: one window per call.
//!
//! `Wand::maxscore_search` owns its whole walk, so the disjunction cursor
//! cannot reuse one window of it. This module lifts that window out of the
//! search: the state below keeps the window's bound, the clause split and the
//! per-window accumulator across calls, and every surviving candidate is
//! handed to a callback.
//!
//! Constraints this walk keeps:
//!
//! * A document whose exact score reaches the floor is always emitted, with the
//!   query-order score the per-document route reports. Pruning only happens
//!   under a positive floor and only when a clause bound proves the document
//!   cannot reach it.
//! * The non-essential prefix is never enumerated away. Its bound sits below
//!   the floor, so a document matched only by that prefix cannot compete.
//! * Nothing here owns a window slice or a raw pointer: the state has to stay
//!   `Send`, because the cursor is boxed and sent to a partition executor.

use super::and_walk::{BulkVisit, StepOutcome};
use super::*;

/// State a windowed disjunction keeps across windows.
///
/// `next_min` is the inclusive start of the next window. `up_to` is the
/// inclusive end of the window the clause split was computed for; clearing it
/// is how a raised floor forces the next call to recompute both.
pub(super) struct BulkOrState {
    pub(super) num_lists: usize,
    /// Candidates the walk compared so far, so a test can tell the windowed
    /// route really ran.
    pub(super) num_comparisons: usize,
    pub(super) done: bool,
    wand_factor: f32,
    next_min: u64,
    up_to: Option<u64>,
    /// Per-clause window bound, in clause order.
    bounds: Vec<f32>,
    /// Clause indices ascending by bound; the head is the non-essential prefix.
    order: Vec<usize>,
    /// Whether a clause belongs to the essential set of the open window.
    essential: Vec<bool>,
    /// Number of non-essential clauses at the head of `order`.
    essential_start: usize,
    /// Sum of the non-essential clauses' window bounds.
    non_essential_bound: f64,
    accumulator: WindowAccumulator,
    contributions: SmallVec<[ScoreContribution; 8]>,
    hit_freqs: SmallVec<[u32; 8]>,
}

impl BulkOrState {
    /// `num_lists` is the number of clauses the walk drives, fixed for the
    /// lifetime of the state: the clause split is indexed by clause position.
    pub(super) fn new(num_lists: usize, wand_factor: f32) -> Self {
        Self {
            num_lists,
            num_comparisons: 0,
            done: false,
            wand_factor,
            next_min: 0,
            up_to: None,
            bounds: vec![0.0; num_lists],
            order: (0..num_lists).collect(),
            essential: vec![false; num_lists],
            essential_start: 0,
            non_essential_bound: 0.0,
            accumulator: WindowAccumulator::new(num_lists),
            contributions: SmallVec::new(),
            hit_freqs: SmallVec::new(),
        }
    }

    /// Forget the open window's end and clause split. The next call recomputes
    /// both at `next_min`, which is the only point where a raised floor is
    /// allowed to change which clauses are probed.
    pub(super) fn invalidate_window(&mut self) {
        self.up_to = None;
    }

    /// Restart the walk at `target`, inclusive. Documents below it are the
    /// caller's business, so the open window and its bounds are dropped.
    pub(super) fn restart_at(&mut self, target: u64) {
        self.next_min = self.next_min.max(target);
        self.up_to = None;
    }

    /// Whether a window bound is still open. Test hook: a floor rise must close
    /// it, so the next window recomputes the bound and the clause split.
    #[cfg(test)]
    pub(super) fn window_is_open(&self) -> bool {
        self.up_to.is_some()
    }
}

impl<'a, S: Scorer, D: WandDocuments> Wand<'a, S, D> {
    /// Walk one window of a disjunction and hand every surviving candidate to
    /// `visit`.
    ///
    /// The window is clamped to the clauses' current blocks, so one bound per
    /// clause covers the whole window. Clauses whose combined bound cannot
    /// reach the floor are never probed per candidate; the rest are streamed
    /// into the accumulator, and only candidates the accumulator could not
    /// reject reach the non-essential clauses.
    ///
    /// The clauses must all sit in `lead`, in query order; the cursor's route
    /// build puts them there and no other route may drive this state.
    pub(super) fn bulk_or_step<V: BulkVisit>(
        &mut self,
        state: &mut BulkOrState,
        visit: &mut V,
    ) -> Result<StepOutcome> {
        if state.done {
            return Ok(StepOutcome::Done);
        }
        if state.up_to.is_none() {
            let Some(up_to) = self.open_or_window(state) else {
                state.done = true;
                return Ok(StepOutcome::Done);
            };
            state.up_to = Some(up_to);
        }
        let up_to = state.up_to.expect("the window was just opened");
        if state.essential_start == state.num_lists {
            // No clause combination in this window reaches the floor.
            state.next_min = up_to.saturating_add(1);
            state.invalidate_window();
            return Ok(StepOutcome::Continue);
        }

        let inner_min = state.next_min;
        let inner_max = up_to.min(inner_min.saturating_add(MAXSCORE_INNER_WINDOW as u64 - 1));
        state.next_min = inner_max.saturating_add(1);
        // Scores are computed from `query_weight * Scorer::doc_weight`, exactly
        // as the per-document route computes them, so the two routes cannot
        // disagree by rounding. The norm addend cache is deliberately unused.
        let norm_k: Option<(&[u8], &[f32; 256])> = None;
        for slot in state.essential_start..state.order.len() {
            let index = state.order[slot];
            self.lead[index].collect_window_scores(
                inner_min,
                inner_max,
                index,
                self.documents,
                &self.scorer,
                norm_k,
                &mut state.accumulator,
            );
        }

        let floor = self.threshold;
        let floor_mode = self.floor_mode;
        let factor = score_sum_upper_bound_factor(state.num_lists);
        for word_index in 0..state.accumulator.words.len() {
            let mut word = state.accumulator.words[word_index];
            if word == 0 {
                continue;
            }
            state.accumulator.words[word_index] = 0;
            while word != 0 {
                let bit = word.trailing_zeros() as usize;
                word &= word - 1;
                let slot = (word_index << 6) | bit;
                let doc = inner_min + slot as u64;
                state.num_comparisons += 1;
                let essential_score = state.accumulator.scores[slot];
                // A candidate the essential set cannot lift to the floor, even
                // with the whole non-essential prefix, is dropped before a
                // single non-essential clause is probed.
                if floor > 0.0
                    && score_sum_cannot_compete(
                        essential_score,
                        state.non_essential_bound,
                        floor,
                        factor,
                        floor_mode,
                    )
                {
                    state.accumulator.clear_slot(slot);
                    continue;
                }
                let Some(document_key) = self.documents.document_key_for_doc_id(doc as u32) else {
                    state.accumulator.clear_slot(slot);
                    continue;
                };
                let doc_length = self.documents.scoring_num_tokens(doc as u32);
                state.contributions.clear();
                state.hit_freqs.clear();
                // Query order: the accumulator holds the essential clauses'
                // frequencies, the non-essential ones are probed here, and both
                // move forward only.
                for index in 0..state.num_lists {
                    let freq = if state.essential[index] {
                        state.accumulator.clause_freq(index, slot)
                    } else {
                        let posting = &mut self.lead[index];
                        if posting.doc().is_some_and(|current| current.doc_id() < doc) {
                            posting.next(doc);
                        }
                        posting
                            .doc()
                            .filter(|current| current.doc_id() == doc)
                            .map_or(0, |current| current.frequency())
                    };
                    if freq == 0 {
                        continue;
                    }
                    let posting = &self.lead[index];
                    state.contributions.push((
                        (posting.position, posting.token_id),
                        posting.score(&self.scorer, freq, doc_length),
                    ));
                    state.hit_freqs.push(freq);
                }
                let score = score_contributions_in_query_order(state.contributions.clone());
                state.accumulator.clear_slot(slot);
                if floor > 0.0 && score_sum_cannot_compete(score, 0.0, floor, factor, floor_mode) {
                    continue;
                }
                if let Some(kth) = visit.visit(
                    doc as u32,
                    document_key,
                    score,
                    doc_length,
                    &state.hit_freqs,
                )? {
                    self.update_threshold(kth, state.wand_factor);
                    // The end and the split were computed under the old floor.
                    state.invalidate_window();
                }
            }
        }

        if inner_max >= up_to {
            state.invalidate_window();
        }
        Ok(StepOutcome::Continue)
    }

    /// Open the window that starts at the earliest live clause document.
    ///
    /// Returns its inclusive end, or `None` once no clause can produce another
    /// document. The clause split is recomputed here, so a raised floor only
    /// has to clear the window to get fresh bounds.
    fn open_or_window(&mut self, state: &mut BulkOrState) -> Option<u64> {
        #[cfg(test)]
        cursor_counters::or_window_refresh();
        let mut first_doc = TERMINATED_DOC_ID;
        for posting in &self.lead {
            if let Some(doc) = posting.doc() {
                first_doc = first_doc.min(u64::from(doc.doc_id()));
            }
        }
        if first_doc == TERMINATED_DOC_ID {
            return None;
        }
        let window_min = state.next_min.max(first_doc);
        if window_min > u32::MAX as u64 {
            // No posting holds a document past the doc-id domain.
            return None;
        }
        state.next_min = window_min;

        // The window ends at the first clause block edge, so every clause bound
        // is anchored inside the window it has to cover. The anchor search
        // starts from the first block: a shallow probe may have left the block
        // cursor ahead of this clause's document, and a forward-only search
        // would then bound the wrong block.
        let mut up_to = TERMINATED_DOC_ID;
        for posting in self.lead.iter_mut() {
            if posting.doc().is_none() {
                continue;
            }
            posting.seek_block_from_start(window_min);
            up_to = up_to.min(posting.block_end_doc());
        }

        for (index, posting) in self.lead.iter().enumerate() {
            state.bounds[index] = match posting.doc() {
                Some(doc) if u64::from(doc.doc_id()) <= up_to => {
                    posting
                        .block_max_score_up_to_with_stats(up_to, &self.scorer)
                        .score
                }
                // An exhausted clause, and one whose cursor already passed the
                // window, contributes nothing inside it.
                _ => 0.0,
            };
        }

        if self.threshold <= 0.0 {
            state.essential.fill(true);
            state.essential_start = 0;
            state.non_essential_bound = 0.0;
            return Some(up_to);
        }

        let bounds = &state.bounds;
        let num_lists = state.num_lists;
        state.order.clear();
        state.order.extend(0..num_lists);
        state
            .order
            .sort_unstable_by(|&left, &right| bounds[left].total_cmp(&bounds[right]));
        let mut prefix = 0.0_f64;
        let mut essential_start = num_lists;
        for (slot, &index) in state.order.iter().enumerate() {
            let next = prefix + f64::from(bounds[index]);
            // Adding this clause could reach the floor, so from here on the
            // clauses must be streamed. `>=` and not `>`: an inclusive floor
            // still wants the document whose score ties it.
            if next * score_sum_upper_bound_factor(slot + 1) >= f64::from(self.threshold) {
                essential_start = slot;
                break;
            }
            prefix = next;
        }
        state.essential.fill(false);
        for slot in essential_start..num_lists {
            state.essential[state.order[slot]] = true;
        }
        state.essential_start = essential_start;
        state.non_essential_bound = prefix;
        Some(up_to)
    }
}
