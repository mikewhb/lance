// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Bulk conjunction state that can be advanced one window at a time.
//!
//! `Wand::and_bulk_search` used to own the whole walk in one function, so the
//! per-document cursor could not reuse a single window of that walk. The state
//! in this module moves out of that function; the terminal still drives it,
//! and later the conjunction cursor drives the same state.
//!
//! The window slices (`WindowList`) hold raw pointers, so they must stay on the
//! stepping call's stack: keeping them here would make the state, and anything
//! that embeds it, `!Send`.

use smallvec::SmallVec;

use super::*;

/// Candidate callback: document, key, score, length, per-clause frequencies.
pub(super) type BulkVisit<'v> = dyn FnMut(u32, u64, f32, u32, &[u32]) -> Result<Option<f32>> + 'v;

/// One step's outcome: another window may be available, or the walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepOutcome {
    Continue,
    Done,
}

/// Everything `and_bulk_search` used to keep in locals across windows.
///
/// `target` is the exclusive lower bound of the next window; `done` is set when
/// a window reached the terminated document id, so the caller can stop without
/// another slice.
pub(super) struct BulkAndState<'a> {
    pub(super) num_lists: usize,
    pub(super) wand_factor: f32,
    pub(super) score_order: Vec<usize>,
    pub(super) use_pairwise_intersection: bool,
    pub(super) phrase_slop: Option<u32>,
    pub(super) batch_docs: Vec<u32>,
    pub(super) batch_offs: Vec<u8>,
    pub(super) batch_lens: Vec<u32>,
    pub(super) batch_norms: Vec<u8>,
    pub(super) cursor_scratch: Vec<usize>,
    pub(super) intersection_scratch: Vec<u32>,
    pub(super) freq_cannot_beat: [bool; FREQ_LUT_BUCKETS],
    pub(super) freq_cannot_beat_key: Option<(u32, u64)>,
    pub(super) freq_bound_lut: Option<[f32; FREQ_LUT_BUCKETS]>,
    pub(super) slice_prev: SmallVec<[(usize, usize); 8]>,
    pub(super) norm_k: Option<NormKCache<'a>>,
    pub(super) norm_k_initialized: bool,
    pub(super) first_score_limit: Option<f32>,
    pub(super) first_score_limit_key: Option<(u32, u64)>,
    pub(super) target: u64,
    pub(super) num_comparisons: usize,
    pub(super) done: bool,
    pub(super) floor_mode: CompetitiveFloorMode,
}

impl<'a> BulkAndState<'a> {
    /// Capture the state the one-shot walk used to build at entry.
    ///
    /// `wand` must already be positioned at the first candidate of every
    /// clause; `and_bulk_search` does that before it starts.
    pub(super) fn new<S: Scorer, D: WandDocuments>(
        wand: &Wand<'a, S, D>,
        params: &FtsSearchParams,
        floor_mode: CompetitiveFloorMode,
    ) -> Self {
        let num_lists = wand.lead.len();
        let use_pairwise_intersection = num_lists >= 4
            && wand.lead.iter().all(|posting| {
                matches!(&posting.list, PostingList::Compressed(list)
                    if list.block_size == MAX_POSTING_BLOCK_SIZE && list.impacts.is_some())
            });
        let mut score_order = (0..num_lists).collect::<Vec<_>>();
        score_order.sort_unstable_by_key(|&index| {
            let posting = &wand.lead[index];
            (posting.position, posting.token_id)
        });
        let mut target = 0_u64;
        let mut done = num_lists == 0;
        for posting in &wand.lead {
            match posting.current_doc_id() {
                Some(doc) => target = target.max(doc),
                None => done = true,
            }
        }
        Self {
            num_lists,
            wand_factor: params.wand_factor,
            score_order,
            use_pairwise_intersection,
            phrase_slop: params.phrase_slop,
            batch_docs: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE),
            batch_offs: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE * num_lists.max(1)),
            batch_lens: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE),
            batch_norms: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE),
            cursor_scratch: Vec::with_capacity(num_lists),
            intersection_scratch: Vec::new(),
            freq_cannot_beat: [false; FREQ_LUT_BUCKETS],
            freq_cannot_beat_key: None,
            freq_bound_lut: None,
            slice_prev: std::iter::repeat_n((usize::MAX, 0), num_lists).collect(),
            norm_k: None,
            norm_k_initialized: false,
            first_score_limit: None,
            first_score_limit_key: None,
            target,
            num_comparisons: 0,
            done,
            floor_mode,
        }
    }
}

impl<'a, S: Scorer, D: WandDocuments> Wand<'a, S, D> {
    /// Advance the conjunction walk by at most one window.
    ///
    /// Every candidate that survives the window's pruning is handed to
    /// `visit` with its key, score, length and per-clause frequencies in lead
    /// order. `visit` returns the new kth score when the top-k heap filled;
    /// the threshold is raised before the walk continues, exactly as the
    /// one-shot loop did.
    pub(super) fn bulk_and_step(
        &mut self,
        state: &mut BulkAndState<'a>,
        visit: &mut BulkVisit<'_>,
    ) -> Result<StepOutcome> {
        if state.done {
            return Ok(StepOutcome::Done);
        }
        let num_lists = state.num_lists;
        #[cfg(test)]
        {
            self.and_window_stats.windows_sliced += 1;
        }
        let window_started_with_floor = self.threshold > 0.0;
        if window_started_with_floor {
            if num_lists >= 2 && state.freq_bound_lut.is_none() {
                state.freq_bound_lut = Some(self.build_lead_freq_bound_lut());
            }
            let advanced = self.and_advance_target(state.target);
            if advanced == TERMINATED_DOC_ID {
                state.done = true;
                return Ok(StepOutcome::Done);
            }
            state.target = advanced;
        }

        let mut wins: SmallVec<[WindowList; 8]> = SmallVec::new();
        let (win_end, jump_to, sliced) = match and_kernel::slice_and_window(
            &mut self.lead,
            state.target,
            &mut state.slice_prev,
            &mut wins,
        ) {
            and_kernel::AndWindowSlice::Exhausted => {
                state.done = true;
                return Ok(StepOutcome::Done);
            }
            and_kernel::AndWindowSlice::Ready { win_end } => (win_end, 0, true),
            and_kernel::AndWindowSlice::Empty { win_end, jump_to } => (win_end, jump_to, false),
        };

        if sliced {
            let mut others_block_max = if window_started_with_floor {
                Some(
                    self.lead[1..]
                        .iter()
                        .map(|posting| f64::from(posting.block_max_score(&self.scorer)))
                        .sum::<f64>(),
                )
            } else {
                None
            };

            state.batch_docs.clear();
            state.batch_offs.clear();
            if window_started_with_floor && num_lists >= 2 {
                and_kernel::load_window_freqs(&self.lead[..1], &mut wins[..1]);
                let others = others_block_max.expect("positive floor should initialize bounds");
                let key = (self.threshold.to_bits(), others.to_bits());
                if state.first_score_limit_key != Some(key) {
                    state.first_score_limit = partial_score_limit(
                        others,
                        self.threshold,
                        score_sum_upper_bound_factor(num_lists),
                        state.floor_mode,
                    );
                    state.first_score_limit_key = Some(key);
                }
                if state.freq_cannot_beat_key != Some(key) {
                    let lut = state
                        .freq_bound_lut
                        .as_ref()
                        .expect("positive threshold should initialize the frequency bound LUT");
                    state.freq_cannot_beat = match state.first_score_limit {
                        Some(limit) => std::array::from_fn(|frequency| lut[frequency] <= limit),
                        None => [false; FREQ_LUT_BUCKETS],
                    };
                    state.freq_cannot_beat_key = Some(key);
                }
            } else if state.freq_cannot_beat_key.is_some() {
                state.freq_cannot_beat = [false; FREQ_LUT_BUCKETS];
                state.freq_cannot_beat_key = None;
            }
            #[cfg(test)]
            if state.freq_cannot_beat.iter().any(|&pruned| pruned) {
                self.and_window_stats.windows_frequency_pruned += 1;
            }
            #[cfg(target_arch = "x86_64")]
            let use_avx2 = *HAS_AVX2;
            #[cfg(not(target_arch = "x86_64"))]
            let use_avx2 = false;
            #[cfg(test)]
            if num_lists >= 4 && !window_started_with_floor && state.use_pairwise_intersection {
                self.and_window_stats.pairwise_intersections += 1;
            }
            and_kernel::merge_window(
                wins.as_slice(),
                num_lists,
                use_avx2,
                state.use_pairwise_intersection,
                window_started_with_floor,
                &state.freq_cannot_beat,
                &mut state.batch_docs,
                &mut state.batch_offs,
                &mut state.cursor_scratch,
                &mut state.intersection_scratch,
            );

            if !state.batch_docs.is_empty() {
                and_kernel::load_window_freqs(&self.lead, &mut wins);
                if !state.norm_k_initialized {
                    state.norm_k = self.norm_k_cache();
                    state.norm_k_initialized = true;
                }
                state.batch_lens.clear();
                state.batch_norms.clear();
                let norm_k_ref = state
                    .norm_k
                    .as_ref()
                    .map(|(norms, cache)| (*norms, cache.as_ref()));
                match norm_k_ref {
                    Some((norms, _)) => {
                        for &doc in state.batch_docs.iter() {
                            state.batch_norms.push(norms[doc as usize]);
                        }
                    }
                    None => match self.documents.scoring_norms() {
                        Some(norms) => {
                            for &doc in state.batch_docs.iter() {
                                state
                                    .batch_lens
                                    .push(dequantize_doc_length(norms[doc as usize]));
                            }
                        }
                        None => {
                            for &doc in state.batch_docs.iter() {
                                state
                                    .batch_lens
                                    .push(self.documents.scoring_num_tokens(doc));
                            }
                        }
                    },
                }

                let mut hit_freqs: Vec<u32> = Vec::with_capacity(num_lists);
                for (index, &doc) in state.batch_docs.iter().enumerate() {
                    let (norm_addend, doc_length) = match norm_k_ref {
                        Some((_, cache)) => {
                            let code = state.batch_norms[index];
                            (Some(cache[code as usize]), dequantize_doc_length(code))
                        }
                        None => (None, state.batch_lens[index]),
                    };
                    let offs = &state.batch_offs[index * num_lists..(index + 1) * num_lists];
                    if self.threshold > 0.0 && num_lists >= 2 && others_block_max.is_none() {
                        others_block_max = Some(
                            self.lead[1..]
                                .iter()
                                .map(|posting| f64::from(posting.block_max_score(&self.scorer)))
                                .sum::<f64>(),
                        );
                    }
                    if self.threshold > 0.0
                        && num_lists >= 2
                        && let Some(others_block_max) = others_block_max
                    {
                        let first_freq = unsafe { *wins[0].freqs.add(offs[0] as usize) };
                        let first_score = match norm_addend {
                            Some(addend) => {
                                self.lead[0].query_weight
                                    * bm25_doc_weight_with_norm(first_freq, addend)
                            }
                            None => self.lead[0].score(&self.scorer, first_freq, doc_length),
                        };
                        let key = (self.threshold.to_bits(), others_block_max.to_bits());
                        if state.first_score_limit_key != Some(key) {
                            state.first_score_limit = partial_score_limit(
                                others_block_max,
                                self.threshold,
                                score_sum_upper_bound_factor(num_lists),
                                state.floor_mode,
                            );
                            state.first_score_limit_key = Some(key);
                        }
                        if state
                            .first_score_limit
                            .is_some_and(|limit| first_score <= limit)
                        {
                            continue;
                        }
                    }
                    #[cfg(test)]
                    {
                        self.and_window_stats.candidates_returned += 1;
                    }
                    state.num_comparisons += 1;

                    let Some(document_key) = self.documents.document_key_for_doc_id(doc) else {
                        continue;
                    };

                    let early_score = (self.threshold > 0.0).then(|| {
                        window_bm25_score(
                            &self.lead,
                            wins.as_slice(),
                            offs,
                            &state.score_order,
                            &self.scorer,
                            norm_addend,
                            doc_length,
                        )
                    });
                    if let Some(slop) = state.phrase_slop {
                        if early_score
                            .is_some_and(|score| self.exclusive_score_cannot_beat_floor(score))
                        {
                            continue;
                        }
                        for ((win, posting), &off) in
                            wins.iter().zip(self.lead.iter_mut()).zip(offs.iter())
                        {
                            posting.index = win.block_start + off as usize;
                            posting.current_doc = Some(DocInfo::Raw(RawDocInfo {
                                doc_id: doc,
                                frequency: unsafe { *win.freqs.add(off as usize) },
                            }));
                        }
                        let matched = if slop == 0 {
                            self.check_exact_positions_bulk()?
                        } else {
                            self.check_positions(slop as i32)?
                        };
                        if !matched {
                            continue;
                        }
                    }
                    let score = early_score.unwrap_or_else(|| {
                        window_bm25_score(
                            &self.lead,
                            wins.as_slice(),
                            offs,
                            &state.score_order,
                            &self.scorer,
                            norm_addend,
                            doc_length,
                        )
                    });

                    hit_freqs.clear();
                    for (win, &off) in wins.iter().zip(offs.iter()) {
                        hit_freqs.push(unsafe { *win.freqs.add(off as usize) });
                    }
                    if let Some(kth) = visit(doc, document_key, score, doc_length, &hit_freqs)? {
                        self.update_threshold(kth, state.wand_factor);
                    }
                }
            }
        }

        if win_end == TERMINATED_DOC_ID {
            state.done = true;
            return Ok(StepOutcome::Done);
        }
        state.target = (win_end + 1).max(jump_to);
        Ok(StepOutcome::Continue)
    }
}
