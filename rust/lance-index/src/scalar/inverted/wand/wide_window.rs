// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Wide windows for the bulk conjunction: one window per lead block.
//!
//! The narrow bulk window ends at the nearest block end across all clauses.
//! When the followers are much denser than the lead, their block boundaries
//! cut every lead block into many windows, and each one pays the window setup
//! again. A wide window spans the whole lead block instead, like Lucene's
//! `BlockMaxConjunctionBulkScorer`: the lead's documents are the candidates,
//! the followers filter them one clause at a time, and a follower block is
//! decoded only when a candidate that can still compete falls into it.

use arrow_array::Array;
use lance_core::Result;
use smallvec::SmallVec;

use crate::scalar::inverted::builder::ScoredDoc;
use crate::scalar::inverted::encoding::MAX_POSTING_BLOCK_SIZE;
use crate::scalar::inverted::index::dequantize_doc_length;
use crate::scalar::inverted::query::FtsSearchParams;
use crate::scalar::inverted::scorer::{Scorer, bm25_doc_weight_with_norm};
use crate::scalar::inverted::{DocInfo, PostingList, RawDocInfo};

use super::bounds::{next_up_f64, score_sum_cannot_compete, score_sum_upper_bound_factor};
use super::collector::TopKCollector;
use super::doc_source::WandDocuments;
use super::posting::{FREQ_LUT_BUCKETS, find_next_geq};
use super::{NormKCache, Wand};

/// Widest span of one wide window, the `MAX_WINDOW_SIZE` of Lucene's
/// `BlockMaxConjunctionBulkScorer`, so a very sparse lead block is split.
pub(super) const WIDE_MAX_SPAN: u64 = 1 << 16;

/// Follower densities at which a conjunction opens wide windows. They are
/// tuned with benchmarks, not derived, so a change needs a benchmark rerun.
const WIDE_GATE_K: u64 = 8;
const WIDE_GATE_K_WIDE: u64 = 13;

/// Whether a conjunction opens wide windows. Narrow windows end at follower
/// block ends, so one lead block is cut into about `followers_cost /
/// lead_cost` of them; the wide window pays off once that reaches
/// `block_size / K`. A narrow window decodes a block per clause, so four or
/// more clauses open wide windows at a lower follower density.
pub(super) fn wide_gate(
    lead_cost: u64,
    followers_cost: u64,
    block_size: u64,
    num_clauses: usize,
) -> bool {
    let k = if num_clauses >= 4 {
        WIDE_GATE_K_WIDE
    } else {
        WIDE_GATE_K
    };
    followers_cost.saturating_mul(k) >= block_size.saturating_mul(lead_cost)
}

const NO_POSITION_HINT: u64 = u64::MAX;

/// A posting's position delta start (high half) and its block's delta total
/// (low half), or `NO_POSITION_HINT` when either does not fit 32 bits.
fn position_hint(start: u64, total: u64) -> u64 {
    match (u32::try_from(start), u32::try_from(total)) {
        (Ok(start), Ok(total)) => (u64::from(start) << 32) | u64::from(total),
        _ => NO_POSITION_HINT,
    }
}

/// Per-query scratch of the wide window, allocated by its first window.
pub(super) struct WideState {
    docs: Vec<u32>,
    // Clause-major columns of `MAX_POSTING_BLOCK_SIZE` rows, one row per
    // candidate.
    freq: Vec<u32>,
    // Phrase queries only: the absolute posting index the position check
    // reads, and for followers the candidate's position delta start (high
    // half) and its block's delta total (low half), or `NO_POSITION_HINT`. By
    // the time positions are checked a follower's cursor has moved past most
    // candidate blocks, so the hint lets the check seek without decoding those
    // blocks again.
    pidx: Vec<u32>,
    phint: Vec<u64>,
    // Per follower: the (block, position) where its next probe starts.
    fcur: SmallVec<[(usize, usize); 8]>,
    // Per clause `frequency_bound_lut`, built once a floor exists.
    lut: Vec<f32>,
    lens: Vec<u32>,
    norms: Vec<u8>,
}

impl WideState {
    pub(super) fn new(num_lists: usize, phrase: bool) -> Self {
        let cap = MAX_POSTING_BLOCK_SIZE;
        let phrase_rows = if phrase { cap * num_lists } else { 0 };
        Self {
            docs: Vec::with_capacity(cap),
            freq: vec![0; cap * num_lists],
            pidx: vec![0; phrase_rows],
            phint: vec![NO_POSITION_HINT; phrase_rows],
            fcur: std::iter::repeat_n((usize::MAX, 0), num_lists).collect(),
            lut: Vec::new(),
            lens: Vec::with_capacity(cap),
            norms: Vec::with_capacity(cap),
        }
    }
}

impl<'a, S: Scorer, D: WandDocuments> Wand<'a, S, D> {
    /// Intersect the wide window `[target, wide_end]` of lead block
    /// `lead_block`, then score and collect its matches with the narrow path's
    /// semantics. Returns the next target and whether the conjunction ended.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_wide_window(
        &mut self,
        st: &mut WideState,
        target: u64,
        wide_end: u64,
        lead_block: usize,
        score_order: &[usize],
        norm_k: &mut Option<NormKCache<'a>>,
        norm_k_initialized: &mut bool,
        candidates: &mut TopKCollector,
        num_comparisons: &mut usize,
        params: &FtsSearchParams,
    ) -> Result<(u64, bool)> {
        let num_lists = self.lead.len();
        let cap = MAX_POSTING_BLOCK_SIZE;
        let factor = score_sum_upper_bound_factor(num_lists);
        let mode = self.floor_mode;
        let target32 = target as u32;
        let wide_end32 = u32::try_from(wide_end).unwrap_or(u32::MAX);
        let phrase = params.phrase_slop.is_some();

        // A narrow window may have moved a follower's block cursor past the wide
        // cursor; then the block cursor is the better start. Both only move
        // forward.
        for j in 1..num_lists {
            let start = self.lead[j].block_idx;
            if st.fcur[j].0 == usize::MAX || start > st.fcur[j].0 {
                st.fcur[j] = (start, 0);
            }
        }

        // (a) The lead's documents in the window are the candidates. Without
        // one, block metadata alone gives the next target.
        let lead_lo;
        {
            let posting = &self.lead[0];
            let PostingList::Compressed(ref list) = posting.list else {
                unreachable!("bulk AND requires compressed postings");
            };
            let state = unsafe { &*posting.ensure_compressed_doc_ids_ptr(list, lead_block) };
            let docs = state.doc_ids.as_slice();
            let lo = docs.partition_point(|&doc| doc < target32);
            let hi = lo + docs[lo..].partition_point(|&doc| doc <= wide_end32);
            if lo == hi {
                if hi < docs.len() {
                    return Ok((u64::from(docs[hi]), false));
                }
                if lead_block + 1 < list.blocks.len() {
                    return Ok((u64::from(list.block_least_doc_id(lead_block + 1)), false));
                }
                return Ok((0, true));
            }
            st.docs.clear();
            st.docs.extend_from_slice(&docs[lo..hi]);
            debug_assert!(st.docs.len() <= cap);
            if phrase {
                let shift = list.block_shift();
                for (i, slot) in st.pidx[..st.docs.len()].iter_mut().enumerate() {
                    *slot = u32::try_from((lead_block << shift) | (lo + i)).unwrap_or(u32::MAX);
                }
            }
            lead_lo = lo;
        }
        let mut n = st.docs.len();

        // (b) Pruning uses the floor the narrow path uses and, like it, waits
        // for a positive floor. A window whose lead block bound plus the
        // followers' list-wide bounds cannot beat it retires before any
        // frequency is decoded.
        let floor = self.threshold;
        let mut prunable = floor > 0.0;
        let mut suffix = SmallVec::<[f64; 8]>::new();
        if prunable {
            suffix.resize(num_lists + 1, 0.0);
            for j in (0..num_lists).rev() {
                let posting = &self.lead[j];
                let bound = if j == 0 {
                    posting.block_bound_at(lead_block, &self.scorer)
                } else {
                    posting.global_upper_bound(&self.scorer)
                };
                if !(bound.is_finite() && bound > 0.0) {
                    prunable = false;
                    break;
                }
                suffix[j] = next_up_f64(f64::from(bound) + suffix[j + 1]);
            }
        }
        if prunable {
            if score_sum_cannot_compete(0.0, suffix[0], floor, factor, mode) {
                return Ok((wide_end + 1, wide_end >= u32::MAX as u64));
            }
            if st.lut.is_empty() {
                st.lut.reserve(num_lists * FREQ_LUT_BUCKETS);
                for posting in &self.lead {
                    st.lut.extend(posting.frequency_bound_lut(&self.scorer));
                }
            }
        }

        // (b') The lead column holds block offsets until the first reader needs
        // frequencies: the frequency prune below, or else scoring. Without a
        // floor, a window whose candidates all miss a follower never decodes the
        // lead's frequency block.
        for (i, slot) in st.freq[..n].iter_mut().enumerate() {
            *slot = (lead_lo + i) as u32;
        }
        if prunable {
            self.resolve_lead_freqs(lead_block, &mut st.freq[..n]);
        }

        // Position hints spare a phrase check a second decode of a follower
        // block that the follower's cursor has already left. Without a floor
        // every candidate that clears the followers gets that check; with one,
        // most fail on score first and the second decode stays rare.
        let hints = phrase && !prunable;

        // (c) Filter the candidates by one follower at a time. A follower block
        // is decoded only when a candidate that can still compete falls into
        // it, and survivors are compacted in place with all their columns.
        let mut max_next: u64 = 0;
        let mut query_done = false;
        let mut pruned = [false; MAX_POSTING_BLOCK_SIZE];
        for j in 1..num_lists {
            let posting = &self.lead[j];
            let PostingList::Compressed(ref list) = posting.list else {
                unreachable!("bulk AND requires compressed postings");
            };
            let num_blocks = list.blocks.len();
            let shift = list.block_shift();
            // Frequency one is the weakest a posting can be: when even that
            // competes, no candidate can be pruned against this follower block.
            let weakest = if prunable {
                (0..j)
                    .map(|c| f64::from(st.lut[c * FREQ_LUT_BUCKETS + 1]))
                    .sum::<f64>()
            } else {
                0.0
            };
            let (mut fb, mut fpos) = st.fcur[j];
            let mut i = 0usize;
            let mut out = 0usize;
            let mut max_next_j: u64 = 0;
            let mut last_decoded: Option<usize> = None;
            while i < n {
                let doc = st.docs[i];
                if fb + 1 < num_blocks && doc >= list.block_least_doc_id(fb + 1) {
                    fb = posting.block_idx_for_doc(list, fb, doc);
                    fpos = 0;
                }
                let segment_end_doc = if fb + 1 < num_blocks {
                    list.block_least_doc_id(fb + 1) - 1
                } else {
                    u32::MAX
                };
                let i_end = i + st.docs[i..n].partition_point(|&doc| doc <= segment_end_doc);
                // The cursor must skip the whole block, not just its last
                // survivor, so remember this before any compaction.
                let segment_last = st.docs[i_end - 1];
                pruned[i..i_end].fill(false);
                let mut keep = i_end - i;
                if prunable {
                    let rem = next_up_f64(
                        f64::from(posting.block_bound_at(fb, &self.scorer)) + suffix[j + 1],
                    );
                    if score_sum_cannot_compete(0.0, weakest + rem, floor, factor, mode) {
                        // The index addresses the candidate columns and the
                        // marks alike.
                        #[allow(clippy::needless_range_loop)]
                        for x in i..i_end {
                            let bound = (0..j)
                                .map(|c| {
                                    let freq =
                                        (st.freq[c * cap + x] as usize).min(FREQ_LUT_BUCKETS - 1);
                                    f64::from(st.lut[c * FREQ_LUT_BUCKETS + freq])
                                })
                                .sum::<f64>();
                            if score_sum_cannot_compete(0.0, bound + rem, floor, factor, mode) {
                                pruned[x] = true;
                                keep -= 1;
                            }
                        }
                    }
                }
                if keep == 0 {
                    // The bound covers the whole block, so it is neither decoded
                    // nor passed by the cursor.
                    #[cfg(test)]
                    {
                        self.and_window_stats.wide_blocks_skipped += 1;
                    }
                    i = i_end;
                    continue;
                }
                let (docs_b, len_b) = {
                    let state = unsafe { &*posting.ensure_compressed_doc_ids_ptr(list, fb) };
                    (state.doc_ids.as_ptr(), state.doc_ids.len())
                };
                last_decoded = Some(fb);
                // Frequencies stay packed until the first hit: a block that no
                // candidate lands in costs one doc id decode only.
                let mut freqs: Option<&[u32]> = None;
                // With hints: the block's position delta total, and the delta
                // sum up to `delta_pos`; hits arrive in posting order.
                let (mut block_deltas, mut delta_pos, mut delta_sum) = (0u64, 0usize, 0u64);
                #[allow(clippy::needless_range_loop)]
                for x in i..i_end {
                    if pruned[x] {
                        continue;
                    }
                    fpos = unsafe { find_next_geq(docs_b, fpos, len_b, st.docs[x]) };
                    if fpos < len_b && unsafe { *docs_b.add(fpos) } == st.docs[x] {
                        let freqs = match freqs {
                            Some(freqs) => freqs,
                            None => {
                                let state =
                                    unsafe { &*posting.ensure_compressed_block_ptr(list, fb) };
                                if hints {
                                    block_deltas =
                                        state.freqs.iter().map(|&freq| u64::from(freq)).sum();
                                }
                                *freqs.insert(state.freqs.as_slice())
                            }
                        };
                        if out != x {
                            st.docs[out] = st.docs[x];
                            for c in 0..j {
                                st.freq[c * cap + out] = st.freq[c * cap + x];
                            }
                            if phrase {
                                for c in 0..j {
                                    st.pidx[c * cap + out] = st.pidx[c * cap + x];
                                }
                            }
                            if hints {
                                for c in 1..j {
                                    st.phint[c * cap + out] = st.phint[c * cap + x];
                                }
                            }
                        }
                        st.freq[j * cap + out] = freqs[fpos];
                        if phrase {
                            st.pidx[j * cap + out] =
                                u32::try_from((fb << shift) | fpos).unwrap_or(u32::MAX);
                        }
                        if hints {
                            delta_sum += freqs[delta_pos..fpos]
                                .iter()
                                .map(|&freq| u64::from(freq))
                                .sum::<u64>();
                            delta_pos = fpos;
                            st.phint[j * cap + out] = position_hint(delta_sum, block_deltas);
                        }
                        out += 1;
                    }
                }
                fpos =
                    unsafe { find_next_geq(docs_b, fpos, len_b, segment_last.saturating_add(1)) };
                if fpos < len_b {
                    max_next_j = max_next_j.max(u64::from(unsafe { *docs_b.add(fpos) }));
                } else if fb + 1 == num_blocks {
                    // Nothing after this follower's last document can match, but
                    // the survivors still have to clear the remaining clauses.
                    query_done = true;
                    break;
                } else {
                    max_next_j = max_next_j.max(u64::from(list.block_least_doc_id(fb + 1)));
                    fb += 1;
                    fpos = 0;
                }
                i = i_end;
            }
            st.fcur[j] = (fb, fpos);
            // Only a decoded block moves the clause's block cursor, and only
            // forward: the block holding the next target is at or after it.
            if let Some(block) = last_decoded {
                self.lead[j].block_idx = self.lead[j].block_idx.max(block);
            }
            max_next = max_next.max(max_next_j);
            n = out;
            if n == 0 {
                break;
            }
        }

        // (d) Score and collect in doc order: gather lengths first, then sum in
        // query order with the narrow path's per-clause expression, so the f32
        // total is bit-identical.
        if n > 0 {
            if !prunable {
                self.resolve_lead_freqs(lead_block, &mut st.freq[..n]);
            }
            if !*norm_k_initialized {
                *norm_k = self.norm_k_cache();
                *norm_k_initialized = true;
            }
            let norm_k_ref = norm_k
                .as_ref()
                .map(|(norms, cache)| (*norms, cache.as_ref()));
            st.lens.clear();
            st.norms.clear();
            match norm_k_ref {
                Some((norms, _)) => st
                    .norms
                    .extend(st.docs[..n].iter().map(|&doc| norms[doc as usize])),
                None => match self.documents.scoring_norms() {
                    Some(norms) => st.lens.extend(
                        st.docs[..n]
                            .iter()
                            .map(|&doc| dequantize_doc_length(norms[doc as usize])),
                    ),
                    None => st.lens.extend(
                        st.docs[..n]
                            .iter()
                            .map(|&doc| self.documents.scoring_num_tokens(doc)),
                    ),
                },
            }
            for i in 0..n {
                let doc = st.docs[i];
                *num_comparisons += 1;
                let Some(document_key) = self.documents.document_key_for_doc_id(doc) else {
                    continue;
                };
                let (norm_addend, doc_length) = match norm_k_ref {
                    Some((_, cache)) => {
                        let code = st.norms[i];
                        (Some(cache[code as usize]), dequantize_doc_length(code))
                    }
                    None => (None, st.lens[i]),
                };
                // As in the narrow path, score before the position check only
                // when a floor can skip it; otherwise a candidate that fails
                // the check never pays for its score.
                let early_score = (self.threshold > 0.0).then(|| {
                    self.wide_candidate_score(st, i, score_order, norm_addend, doc_length)
                });
                if let Some(slop) = params.phrase_slop {
                    if early_score
                        .is_some_and(|score| self.exclusive_score_cannot_beat_floor(score))
                    {
                        continue;
                    }
                    // The position check needs every clause parked on this
                    // candidate.
                    for (c, posting) in self.lead.iter_mut().enumerate() {
                        let index = st.pidx[c * cap + i] as usize;
                        let frequency = st.freq[c * cap + i];
                        posting.index = index;
                        posting.current_doc = Some(DocInfo::Raw(RawDocInfo {
                            doc_id: doc,
                            frequency,
                        }));
                        // The lead block stays decoded for the whole window.
                        if hints && c > 0 && st.phint[c * cap + i] != NO_POSITION_HINT {
                            let hint = st.phint[c * cap + i];
                            let start = (hint >> 32) as usize;
                            posting.set_position_hint(
                                index,
                                start..start + frequency as usize,
                                (hint & u64::from(u32::MAX)) as usize,
                            );
                        }
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
                    self.wide_candidate_score(st, i, score_order, norm_addend, doc_length)
                });
                if candidates.insert(
                    ScoredDoc::new(document_key, score),
                    doc_length,
                    u64::from(doc),
                    (0..num_lists).map(|c| (self.lead[c].term_index(), st.freq[c * cap + i])),
                )? && let Some(kth) = candidates.kth_score_if_full()
                {
                    self.update_threshold(kth, params.wand_factor);
                }
            }
        }

        // (e) The next target clears the window end and every follower's next
        // document: no conjunction can start below the latter.
        let next_target = (wide_end + 1).max(max_next);
        debug_assert!(next_target > target);
        Ok((next_target, query_done || wide_end >= u32::MAX as u64))
    }

    /// The score of candidate row `row`, summed in query order with the narrow
    /// path's per-clause expression, so the f32 total is bit-identical.
    #[inline(always)]
    fn wide_candidate_score(
        &self,
        st: &WideState,
        row: usize,
        score_order: &[usize],
        norm_addend: Option<f32>,
        doc_length: u32,
    ) -> f32 {
        let mut score = 0.0_f32;
        for &c in score_order {
            let freq = st.freq[c * MAX_POSTING_BLOCK_SIZE + row];
            score += match norm_addend {
                Some(addend) => self.lead[c].query_weight * bm25_doc_weight_with_norm(freq, addend),
                None => self.lead[c].score(&self.scorer, freq, doc_length),
            };
        }
        score
    }

    /// Replace each lead block offset in `freqs` with that posting's frequency.
    fn resolve_lead_freqs(&mut self, lead_block: usize, freqs: &mut [u32]) {
        let posting = &self.lead[0];
        let PostingList::Compressed(ref list) = posting.list else {
            unreachable!("bulk AND requires compressed postings");
        };
        let state = unsafe { &*posting.ensure_compressed_block_ptr(list, lead_block) };
        for slot in freqs.iter_mut() {
            *slot = state.freqs[*slot as usize];
        }
        #[cfg(test)]
        {
            self.and_window_stats.wide_lead_freq_decodes += 1;
        }
    }
}
