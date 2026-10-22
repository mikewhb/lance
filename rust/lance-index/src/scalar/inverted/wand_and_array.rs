// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Array-backed lead for skewed conjunctions.
//!
//! The lead clause's current block is copied into an array once per window,
//! then walked by index. Advancing the lead is an index bump, and a follower
//! leap skips the array by binary search, so the lead never pays a cursor seek
//! per candidate. Window-level pruning happens in [`ArrayLead::open`] via an
//! adaptive wide-bound check; inside an open window, once the followers have
//! matched, each candidate passes a frequency-bucket pre-filter and then the
//! exact doc-length prune.

use super::super::scorer::Scorer;
use super::and_kernel::{lead_freq_bound_lut, posting_block_up_to};
use super::{
    CompetitiveFloorMode, DocInfo, FREQ_LUT_BUCKETS, MAX_POSTING_BLOCK_SIZE, PostingIterator,
    PostingList, RawDocInfo, TERMINATED_DOC_ID, WB_LATCH_SKIP_COUNT, WB_REPROBE_INTERVAL,
    conservative_score_sum, score_sum_cannot_compete, score_sum_upper_bound_factor, skip_lead_docs,
};

/// Walk state of the array-backed lead. `lead[0]` of the driving conjunction
/// is the lead clause; the state is meaningful only while that conjunction is
/// walked through this route.
pub(super) struct ArrayLead {
    docs: Vec<u32>,
    freqs: Vec<u32>,
    pos: usize,
    first_index: usize,
    window_end: u64,
    pub(super) next_target: u64,
    // Adaptive gate for the window-level wide-bound check in `open`. Skip
    // rates are bimodal: a few queries skip most of their windows, almost all
    // others skip none. Re-probing every `WB_REPROBE_INTERVAL` windows keeps
    // zero-skip queries from paying the check on every window while still
    // catching isolated skippable ones.
    pub(super) windows_since_check: u32,
    pub(super) total_skips: u32,
    pub(super) always_check: bool,
    // Length-independent upper bounds of the lead clause's score contribution
    // per frequency bucket. Depends only on the lead clause and the scorer --
    // never on candidate or window state -- so it stays valid across windows
    // and is deliberately kept by `clear` (rebuilding it for an identical
    // query would be pure overhead).
    freq_bound: Option<[f32; FREQ_LUT_BUCKETS]>,
    #[cfg(test)]
    pub(super) stats: ArrayLeadStats,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct ArrayLeadStats {
    pub(super) lut_prepruned: usize,
    // Wide-bound checks entered (gate open), whole lead windows skipped by the
    // check, and follower leaps that jumped past the current lead window.
    pub(super) wide_bound_checks: usize,
    pub(super) windows_skipped: usize,
    pub(super) follower_window_jumps: usize,
    // Candidates parked by the deferred-park branch; proves a test actually
    // exercised that ordering.
    pub(super) deferred_park_hits: usize,
}

impl ArrayLead {
    pub(super) fn new() -> Self {
        Self {
            docs: Vec::new(),
            freqs: Vec::new(),
            pos: 0,
            first_index: 0,
            window_end: 0,
            next_target: 0,
            windows_since_check: WB_REPROBE_INTERVAL,
            total_skips: 0,
            always_check: false,
            freq_bound: None,
            #[cfg(test)]
            stats: ArrayLeadStats::default(),
        }
    }

    /// Forgets the previous walk's array and pending target.
    pub(super) fn clear(&mut self) {
        self.pos = 0;
        self.next_target = 0;
        self.docs.clear();
        self.freqs.clear();
    }

    /// Sizes the window buffers for one block.
    pub(super) fn reserve(&mut self) {
        self.docs.reserve(MAX_POSTING_BLOCK_SIZE);
        self.freqs.reserve(MAX_POSTING_BLOCK_SIZE);
    }

    pub(super) fn reset_gate(&mut self) {
        self.windows_since_check = WB_REPROBE_INTERVAL;
        self.total_skips = 0;
        self.always_check = false;
    }

    /// Whether the open window has no entry left.
    pub(super) fn window_drained(&self) -> bool {
        self.pos >= self.docs.len()
    }

    /// Moves the walk to the first lead entry at or after `target`.
    pub(super) fn skip_to(&mut self, target: u64) {
        if self.pos < self.docs.len() && target <= self.window_end {
            self.pos += self.docs[self.pos..].partition_point(|&doc| u64::from(doc) < target);
            return;
        }
        self.pos = self.docs.len();
        self.next_target = if target > u64::from(u32::MAX) {
            TERMINATED_DOC_ID
        } else {
            self.next_target.max(target)
        };
    }

    /// Copies the lead clause's current block into the arrays and moves the
    /// walk to the window's first entry. Returns false when the lead is
    /// exhausted.
    pub(super) fn open<S: Scorer + ?Sized>(
        &mut self,
        lead: &mut [Box<PostingIterator>],
        scorer: &S,
        floor: f32,
    ) -> bool {
        loop {
            if self.next_target == TERMINATED_DOC_ID {
                return false;
            }
            lead[0].next(self.next_target);
            let Some(lead_doc_id) = lead[0].current_doc_id() else {
                return false;
            };
            let target = self.next_target.max(lead_doc_id);
            // The window is the lead clause's own block end: the rarest clause
            // sets the window, so one window serves many candidates.
            let win_end = posting_block_up_to(&lead[0], target);

            if floor > 0.0 && (self.always_check || self.windows_since_check >= WB_REPROBE_INTERVAL)
            {
                self.windows_since_check = 0;
                #[cfg(test)]
                {
                    self.stats.wide_bound_checks += 1;
                }
                for posting in lead.iter_mut() {
                    posting.shallow_next(target);
                }
                let wide_max = conservative_score_sum(lead.iter().map(|posting| {
                    posting
                        .block_max_score_up_to_with_stats(win_end, scorer)
                        .score
                }));
                if wide_max < floor {
                    self.total_skips = self.total_skips.saturating_add(1);
                    if self.total_skips >= WB_LATCH_SKIP_COUNT {
                        self.always_check = true;
                    }
                    #[cfg(test)]
                    {
                        self.stats.windows_skipped += 1;
                    }
                    if win_end == TERMINATED_DOC_ID {
                        return false;
                    }
                    self.next_target = next_target_after(win_end);
                    continue;
                }
            }
            self.windows_since_check = self.windows_since_check.saturating_add(1);

            let Some(first_index) =
                lead[0].peek_remaining_block_docs_up_to(win_end, &mut self.docs, &mut self.freqs)
            else {
                // Defensive: here `win_end = max(next block boundary, target)
                // >= lead[0]`'s current doc and the cursor's own entry is
                // always pushed into the arrays, so the peek cannot return
                // None. Kept as a guard for future call sites.
                if win_end == TERMINATED_DOC_ID {
                    return false;
                }
                self.next_target = next_target_after(win_end);
                continue;
            };

            self.first_index = first_index;
            self.pos = 0;
            self.window_end = win_end;
            self.next_target = next_target_after(win_end);
            return true;
        }
    }

    /// The next document every clause of `lead` contains, with `lead[0]`
    /// parked on it, and that document's scoring length.
    ///
    /// With `prune`, a candidate is skipped when its lead contribution plus
    /// the followers' block bounds cannot reach `floor` under `floor_mode`:
    /// first by the frequency-bucket bound (no doc-length lookup), then by
    /// the exact lead score.
    pub(super) fn next_candidate<S: Scorer + ?Sized>(
        &mut self,
        lead: &mut [Box<PostingIterator>],
        scorer: &S,
        floor: f32,
        floor_mode: CompetitiveFloorMode,
        prune: bool,
        doc_length: impl Fn(&DocInfo) -> u32,
    ) -> Option<(DocInfo, u32)> {
        let num_terms = lead.len();
        let is_vectorized_search_enabled = matches!(num_terms, 3..=5);

        'advance_head: loop {
            while self.pos >= self.docs.len() {
                if !self.open(lead, scorer, floor) {
                    return None;
                }
            }
            let doc = self.docs[self.pos];
            let freq = self.freqs[self.pos];
            let lead_index = self.first_index + self.pos;
            // The lead cursor is NOT parked here: the write is deferred until
            // every follower has matched, so a candidate the followers reject
            // never pays for it. It must still happen *before* the exact prune
            // below, which reads the lead posting's cursor.
            for posting in lead[1..].iter_mut() {
                if posting
                    .current_doc_id()
                    .is_none_or(|cur| cur < u64::from(doc))
                {
                    posting.next_doc_id(u64::from(doc), is_vectorized_search_enabled);
                }
                match posting.current_doc_id() {
                    None => return None,
                    Some(next) if next > u64::from(doc) => {
                        self.pos = skip_lead_docs(&self.docs, self.pos, next);
                        if next > self.window_end {
                            self.next_target = next;
                            self.pos = self.docs.len();
                            #[cfg(test)]
                            {
                                self.stats.follower_window_jumps += 1;
                            }
                        }
                        continue 'advance_head;
                    }
                    Some(_) => {}
                }
            }

            // Every follower has matched, so this candidate will not be
            // leapfrogged. Park it now, before the prune: the exact prune
            // reads the lead posting's cursor.
            park_lead_at(&mut lead[0], lead_index, doc, freq);
            #[cfg(test)]
            {
                self.stats.deferred_park_hits += 1;
            }
            let lead_doc = lead[0].doc()?;
            if prune {
                // One follower-bound pass serves both prune stages: the
                // followers do not move between them.
                let remaining_upper_bound = lead[1..]
                    .iter()
                    .map(|posting| f64::from(posting.block_max_score(scorer)))
                    .sum::<f64>();
                let lut = self
                    .freq_bound
                    .get_or_insert_with(|| lead_freq_bound_lut(&lead[0], scorer));
                let lut_bound = lut[(freq as usize).min(FREQ_LUT_BUCKETS - 1)];
                if lut_bound.is_finite()
                    && score_sum_cannot_compete(
                        lut_bound,
                        remaining_upper_bound,
                        floor,
                        score_sum_upper_bound_factor(num_terms),
                        floor_mode,
                    )
                {
                    #[cfg(test)]
                    {
                        self.stats.lut_prepruned += 1;
                    }
                    self.pos += 1;
                    continue;
                }
                let length = doc_length(&lead_doc);
                if score_sum_cannot_compete(
                    lead[0].score(scorer, lead_doc.frequency(), length),
                    remaining_upper_bound,
                    floor,
                    score_sum_upper_bound_factor(num_terms),
                    floor_mode,
                ) {
                    self.pos += 1;
                    continue;
                }
                self.pos += 1;
                return Some((lead_doc, length));
            }
            let length = doc_length(&lead_doc);
            self.pos += 1;
            return Some((lead_doc, length));
        }
    }
}

/// The target of the window after one ending at `win_end`: `win_end + 1`
/// clamped into the doc-id domain. A `win_end` of `u32::MAX` (or the
/// `u64::MAX` sentinel) must terminate the walk instead of wrapping around to
/// doc 0.
fn next_target_after(win_end: u64) -> u64 {
    if win_end >= u32::MAX as u64 {
        TERMINATED_DOC_ID
    } else {
        win_end + 1
    }
}

/// Parks the lead clause's cursor on an array entry without searching for it.
fn park_lead_at(lead: &mut PostingIterator, index: usize, doc: u32, freq: u32) {
    lead.index = index;
    if let PostingList::Compressed(ref list) = lead.list {
        lead.block_idx = index >> list.block_shift();
    }
    lead.current_doc = Some(DocInfo::Raw(RawDocInfo {
        doc_id: doc,
        frequency: freq,
    }));
}
