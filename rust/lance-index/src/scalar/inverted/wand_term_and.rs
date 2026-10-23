// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! A conjunction of one-term postings evaluated a window at a time with the
//! bulk AND kernels and handed out one document at a time.

use std::sync::Arc;

use smallvec::SmallVec;

use super::super::scorer::{MemBM25Scorer, bm25_doc_weight_with_norm};
#[cfg(target_arch = "x86_64")]
use super::HAS_AVX2;
use super::and_array::ArrayLead;
use super::and_kernel::{
    AndWindowSlice, WindowList, and_block_bound, lead_freq_bound_lut, load_window_freqs,
    merge_window, posting_block_up_to, slice_and_window,
};
use super::{
    CompetitiveFloorMode, FREQ_LUT_BUCKETS, MAX_POSTING_BLOCK_SIZE, PostingIterator, PostingList,
    TERMINATED_DOC_ID, WandDocuments, conjunction_lists_are_skewed, conservative_score_sum,
    dequantize_doc_length, norm_addends, partial_score_limit, score_sum_upper_bound_factor,
};
use crate::metrics::MetricsCollector;

/// One or more one-term MUST postings intersected a window at a time.
///
/// Emits every document of the intersection that can still reach the
/// competitive floor, scored as the clauses' scores summed from zero in
/// clause order: the score of a `RequiredConjunctionScorer` over the same
/// one-term leaves.
/// Cached lead-score limit for one `(floor bits, follower-bound bits)` key.
type LeadLimitCache = ((u32, u64), Option<f32>, [bool; FREQ_LUT_BUCKETS]);

pub(in super::super) struct TermAndCursor<'a, D: WandDocuments> {
    /// Cheapest posting first: the window kernels prune on `lead[0]`.
    #[allow(clippy::vec_box)]
    lead: Vec<Box<PostingIterator>>,
    /// Indices into `lead` in clause order, the order scores are summed in.
    score_order: Vec<usize>,
    documents: &'a D,
    scorer: Arc<MemBM25Scorer>,
    norms: Option<(&'a [u8], Arc<[f32; 256]>)>,
    use_pairwise: bool,
    floor: f32,
    /// First document of the next window.
    target: u64,
    exhausted: bool,
    /// `(up_to, upper)` of the last block bound.
    bound: Option<(u64, f32)>,
    slice_prev: Vec<(usize, usize)>,
    batch_docs: Vec<u32>,
    batch_offs: Vec<u8>,
    cursor_scratch: Vec<usize>,
    intersection_scratch: Vec<u32>,
    freq_bound_lut: Option<[f32; FREQ_LUT_BUCKETS]>,
    /// `(key, lead score limit, frequency buckets that cannot beat it)` of the
    /// last pruned window, keyed by the floor and the followers' block bound.
    limit_cache: Option<LeadLimitCache>,
    /// Followers' block-max sum for the buffered window, so a later floor can
    /// reject a candidate from its lead score alone.
    window_others: Option<f64>,
    /// Array-backed lead, for skewed conjunctions of three or more clauses.
    array: Option<ArrayLead>,
    /// `(doc, norm addend, scoring length)` of the current window's
    /// candidates, scored and checked for visibility only when handed out.
    cands: Vec<(u64, Option<f32>, u32)>,
    /// `lead.len()` frequencies per candidate, in lead order.
    cand_freqs: Vec<u32>,
    next_cand: usize,
    current: Option<(u64, u64, f32)>,
    /// `(target, up_to, upper)` of the last `advance_shallow`.
    shallow: Option<(u64, u64, f32)>,
    comparisons: usize,
    metrics_recorded: bool,
    metrics: &'a dyn MetricsCollector,
}

impl<'a, D: WandDocuments> TermAndCursor<'a, D> {
    /// `postings` in clause order. `scorer` must give every posting the same
    /// document weight as that clause's own scorer.
    pub(in super::super) fn new(
        postings: Vec<PostingIterator>,
        documents: &'a D,
        scorer: Arc<MemBM25Scorer>,
        metrics: &'a dyn MetricsCollector,
    ) -> Self {
        let mut indexed = postings
            .into_iter()
            .map(Box::new)
            .enumerate()
            .collect::<Vec<_>>();
        indexed.sort_by_key(|(_, posting)| posting.cost());
        let mut score_order = vec![0; indexed.len()];
        for (lead_index, (clause, _)) in indexed.iter().enumerate() {
            score_order[*clause] = lead_index;
        }
        let lead = indexed
            .into_iter()
            .map(|(_, posting)| posting)
            .collect::<Vec<_>>();
        let use_pairwise = lead.len() >= 4
            && lead.iter().all(|posting| {
                matches!(&posting.list, PostingList::Compressed(list)
                    if list.block_size == MAX_POSTING_BLOCK_SIZE && list.impacts.is_some())
            });
        let array = (lead.len() >= 3 && conjunction_lists_are_skewed(&lead)).then(|| {
            #[cfg(test)]
            test_counters::arrayed_built();
            let mut array = ArrayLead::new();
            array.reserve();
            array
        });
        let mut target = 0;
        let mut exhausted = lead.is_empty();
        for posting in &lead {
            match posting.current_doc_id() {
                Some(doc) => target = target.max(doc),
                None => exhausted = true,
            }
        }
        let norms = documents.scoring_norms().zip(norm_addends(&*scorer));
        let num_lists = lead.len();
        Self {
            lead,
            score_order,
            documents,
            scorer,
            norms,
            use_pairwise,
            floor: f32::NEG_INFINITY,
            target,
            exhausted,
            bound: None,
            slice_prev: vec![(usize::MAX, 0); num_lists],
            batch_docs: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE),
            batch_offs: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE * num_lists),
            cursor_scratch: Vec::with_capacity(num_lists),
            intersection_scratch: Vec::new(),
            freq_bound_lut: None,
            limit_cache: None,
            window_others: None,
            array,
            cands: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE),
            cand_freqs: Vec::with_capacity(MAX_POSTING_BLOCK_SIZE * num_lists),
            next_cand: 0,
            current: None,
            shallow: None,
            comparisons: 0,
            metrics_recorded: false,
            metrics,
        }
    }

    pub(in super::super) fn doc(&self) -> Option<u64> {
        self.current.map(|(doc, _, _)| doc)
    }

    pub(in super::super) fn document_key(&self) -> Option<u64> {
        self.current.map(|(_, key, _)| key)
    }

    pub(in super::super) fn current_score(&self) -> Option<f32> {
        self.current.map(|(_, _, score)| score)
    }

    pub(in super::super) fn cost(&self) -> usize {
        self.lead
            .iter()
            .map(|posting| posting.cost())
            .min()
            .unwrap_or_default()
            .min(self.documents.visible_cost_upper_bound())
    }

    pub(in super::super) fn raise_floor(&mut self, floor: f32) {
        if floor > self.floor {
            self.floor = floor;
        }
    }

    pub(in super::super) fn global_score_upper_bound(&self) -> Option<f32> {
        let upper = conservative_score_sum(
            self.lead
                .iter()
                .map(|posting| posting.global_upper_bound(&*self.scorer)),
        );
        (upper.is_finite() && upper >= 0.0).then_some(upper)
    }

    pub(in super::super) fn next(&mut self) -> Option<u64> {
        loop {
            while self.next_cand < self.cands.len() {
                // A raised floor can rule out the whole buffered window. Those
                // candidates are not handed out, so they are not scored.
                if self.array.is_none()
                    && let Some((up_to, upper)) = self.bound
                    && upper < self.floor
                    && self.cands[self.next_cand].0 <= up_to
                {
                    while self.next_cand < self.cands.len() && self.cands[self.next_cand].0 <= up_to
                    {
                        self.next_cand += 1;
                    }
                    continue;
                }
                let index = self.next_cand;
                let (doc, norm_addend, doc_length) = self.cands[index];
                if self.lead_score_cannot_beat(index, norm_addend, doc_length) {
                    self.next_cand += 1;
                    continue;
                }
                self.next_cand += 1;
                let score = self.candidate_score(index, norm_addend, doc_length);
                if score < self.floor {
                    continue;
                }
                let Some(key) = self.documents.document_key_for_doc_id(doc as u32) else {
                    continue;
                };
                self.current = Some((doc, key, score));
                #[cfg(test)]
                test_counters::handed_candidate();
                return Some(doc);
            }
            if !self.refill() {
                self.current = None;
                self.record_metrics();
                return None;
            }
        }
    }

    /// Candidate `index`'s clause scores summed from zero in clause order.
    fn candidate_score(&self, index: usize, norm_addend: Option<f32>, doc_length: u32) -> f32 {
        #[cfg(test)]
        test_counters::scored_candidate();
        let num_lists = self.lead.len();
        let freqs = &self.cand_freqs[index * num_lists..(index + 1) * num_lists];
        let scorer: &MemBM25Scorer = &self.scorer;
        let mut score = 0.0_f32;
        for &clause in &self.score_order {
            let posting = &self.lead[clause];
            let freq = freqs[clause];
            score += match norm_addend {
                Some(addend) => posting.query_weight * bm25_doc_weight_with_norm(freq, addend),
                None => posting.score(scorer, freq, doc_length),
            };
        }
        score
    }

    /// The lead clause alone cannot reach `floor`, so the full sum is not scored.
    fn lead_score_cannot_beat(
        &mut self,
        index: usize,
        norm_addend: Option<f32>,
        doc_length: u32,
    ) -> bool {
        let Some(others) = self.window_others else {
            return false;
        };
        if self.floor <= 0.0 || self.lead.len() < 2 {
            return false;
        }
        let key = (self.floor.to_bits(), others.to_bits());
        if self.limit_cache.is_none_or(|(cached, _, _)| cached != key) {
            let limit = partial_score_limit(
                others,
                self.floor,
                score_sum_upper_bound_factor(self.lead.len()),
                CompetitiveFloorMode::Inclusive,
            );
            let mut cannot_beat = [false; FREQ_LUT_BUCKETS];
            if let Some(limit) = limit {
                let scorer: &MemBM25Scorer = &self.scorer;
                let lut = self
                    .freq_bound_lut
                    .get_or_insert_with(|| lead_freq_bound_lut(&self.lead[0], scorer));
                cannot_beat = std::array::from_fn(|frequency| lut[frequency] <= limit);
            }
            self.limit_cache = Some((key, limit, cannot_beat));
        }
        let Some(limit) = self.limit_cache.and_then(|(_, limit, _)| limit) else {
            return false;
        };
        let freq = self.cand_freqs[index * self.lead.len()];
        let lead_score = match norm_addend {
            Some(addend) => self.lead[0].query_weight * bm25_doc_weight_with_norm(freq, addend),
            None => self.lead[0].score(&self.scorer, freq, doc_length),
        };
        lead_score <= limit
    }

    pub(in super::super) fn advance(&mut self, target: u64) -> Option<u64> {
        if self.current.is_some_and(|(doc, _, _)| doc >= target) {
            return self.doc();
        }
        while self
            .cands
            .get(self.next_cand)
            .is_some_and(|&(doc, _, _)| doc < target)
        {
            self.next_cand += 1;
        }
        if self.next_cand >= self.cands.len() {
            match self.array.as_mut() {
                Some(array) => array.skip_to(target),
                None => self.target = self.target.max(target),
            }
        }
        self.next()
    }

    /// Bounds the conjunction from `target` without moving any block cursor
    /// the next window depends on.
    pub(in super::super) fn advance_shallow(&mut self, target: u64) -> u64 {
        // Compressed postings hold u32 doc ids: nothing lies past u32::MAX.
        if target > u64::from(u32::MAX) {
            self.shallow = Some((target, TERMINATED_DOC_ID, 0.0));
            return TERMINATED_DOC_ID;
        }
        let saved = self
            .lead
            .iter()
            .map(|posting| posting.block_idx)
            .collect::<SmallVec<[usize; 8]>>();
        // Block lookup only moves forward. An array-lead window can leave a
        // follower past `target`, so the bound probe searches every block and
        // the saved cursors are restored afterwards.
        let from_start = self.array.is_some();
        for posting in &mut self.lead {
            if from_start {
                posting.seek_block_from_start(target);
            } else {
                posting.shallow_next(target);
            }
        }
        let up_to = self
            .lead
            .iter()
            .map(|posting| posting_block_up_to(posting, target))
            .min()
            .unwrap_or(TERMINATED_DOC_ID);
        let upper = conservative_score_sum(
            self.lead
                .iter()
                .map(|posting| posting.block_max_score(&*self.scorer)),
        );
        for (posting, block_idx) in self.lead.iter_mut().zip(saved) {
            posting.block_idx = block_idx;
        }
        self.shallow = Some((target, up_to, upper));
        up_to
    }

    pub(in super::super) fn score_upper_bound(&self, up_to: u64) -> Option<f32> {
        let (target, shallow_up_to, upper) = self.shallow?;
        (target..=shallow_up_to).contains(&up_to).then_some(upper)
    }

    /// Fills the candidates of the next window that has any; false once exhausted.
    fn refill(&mut self) -> bool {
        self.cands.clear();
        self.cand_freqs.clear();
        self.next_cand = 0;
        self.window_others = None;
        if self.array.is_some() {
            return self.refill_arrayed();
        }
        // Window slices hold raw block pointers, which a `Send` scorer cannot
        // keep between calls.
        let mut wins = Vec::with_capacity(self.lead.len());
        while !self.exhausted {
            if self.target > u64::from(u32::MAX) {
                self.exhausted = true;
                break;
            }
            if self.floor > 0.0 {
                if self.bound.is_none_or(|(up_to, _)| self.target > up_to) {
                    let bound =
                        and_block_bound(&mut self.lead, &*self.scorer, self.target, self.floor);
                    self.bound = Some((bound.up_to, bound.max_score));
                }
                if let Some((up_to, upper)) = self.bound
                    && upper < self.floor
                {
                    #[cfg(test)]
                    test_counters::skipped_window();
                    if up_to == TERMINATED_DOC_ID {
                        self.exhausted = true;
                        break;
                    }
                    self.target = up_to + 1;
                    continue;
                }
            }
            let (win_end, jump_to) = match slice_and_window(
                &mut self.lead,
                self.target,
                &mut self.slice_prev,
                &mut wins,
            ) {
                AndWindowSlice::Exhausted => {
                    self.exhausted = true;
                    break;
                }
                AndWindowSlice::Empty { win_end, jump_to } => (win_end, jump_to),
                AndWindowSlice::Ready { win_end } => {
                    self.collect_window(&mut wins);
                    (win_end, 0)
                }
            };
            if win_end == TERMINATED_DOC_ID {
                self.exhausted = true;
            } else {
                self.target = (win_end + 1).max(jump_to);
            }
            if !self.cands.is_empty() {
                return true;
            }
        }
        !self.cands.is_empty()
    }

    /// Fills `hits` from the array lead, one lead window at a time.
    fn refill_arrayed(&mut self) -> bool {
        let Some(array) = self.array.as_mut() else {
            return false;
        };
        let documents = self.documents;
        let scorer: &MemBM25Scorer = &self.scorer;
        let prune = self.floor > 0.0;
        while !self.exhausted {
            let Some((lead_doc, doc_length)) = array.next_candidate(
                &mut self.lead,
                scorer,
                self.floor,
                CompetitiveFloorMode::Inclusive,
                prune,
                |doc| documents.doc_length(doc),
            ) else {
                self.exhausted = true;
                break;
            };
            self.comparisons += 1;
            #[cfg(test)]
            test_counters::collected_candidate();
            self.cands.push((lead_doc.doc_id(), None, doc_length));
            for posting in &self.lead {
                let doc = posting.doc();
                debug_assert!(doc.is_some(), "every clause sits on the candidate");
                self.cand_freqs.push(doc.map_or(0, |doc| doc.frequency()));
            }
            if array.window_drained() && !self.cands.is_empty() {
                return true;
            }
        }
        !self.cands.is_empty()
    }

    /// Intersects the sliced window and keeps its survivors in `hits`.
    fn collect_window(&mut self, wins: &mut [WindowList]) {
        let num_lists = self.lead.len();
        let scorer: &MemBM25Scorer = &self.scorer;
        let prune = self.floor > 0.0 && num_lists >= 2;
        let mut freq_cannot_beat = [false; FREQ_LUT_BUCKETS];
        let mut first_score_limit = None;
        let others = self.lead[1..]
            .iter()
            .map(|posting| f64::from(posting.block_max_score(scorer)))
            .sum::<f64>();
        if num_lists >= 2 {
            self.window_others = Some(others);
        }
        if prune {
            let key = (self.floor.to_bits(), others.to_bits());
            if self.limit_cache.is_none_or(|(cached, _, _)| cached != key) {
                let limit = partial_score_limit(
                    others,
                    self.floor,
                    score_sum_upper_bound_factor(num_lists),
                    CompetitiveFloorMode::Inclusive,
                );
                let mut cannot_beat = [false; FREQ_LUT_BUCKETS];
                if let Some(limit) = limit {
                    let lut = self
                        .freq_bound_lut
                        .get_or_insert_with(|| lead_freq_bound_lut(&self.lead[0], scorer));
                    cannot_beat = std::array::from_fn(|frequency| lut[frequency] <= limit);
                }
                self.limit_cache = Some((key, limit, cannot_beat));
            }
            if let Some((_, limit, cannot_beat)) = self.limit_cache {
                first_score_limit = limit;
                freq_cannot_beat = cannot_beat;
            }
            load_window_freqs(&self.lead[..1], &mut wins[..1]);
        }
        #[cfg(target_arch = "x86_64")]
        let use_avx2 = *HAS_AVX2;
        #[cfg(not(target_arch = "x86_64"))]
        let use_avx2 = false;
        self.batch_docs.clear();
        self.batch_offs.clear();
        merge_window(
            wins,
            num_lists,
            use_avx2,
            self.use_pairwise,
            prune,
            &freq_cannot_beat,
            &mut self.batch_docs,
            &mut self.batch_offs,
            &mut self.cursor_scratch,
            &mut self.intersection_scratch,
        );
        if self.batch_docs.is_empty() {
            return;
        }
        load_window_freqs(&self.lead, wins);
        for (index, &doc) in self.batch_docs.iter().enumerate() {
            let offs = &self.batch_offs[index * num_lists..(index + 1) * num_lists];
            let (norm_addend, doc_length) = match &self.norms {
                Some((norms, addends)) => {
                    let code = norms[doc as usize];
                    (Some(addends[code as usize]), dequantize_doc_length(code))
                }
                None => (None, self.documents.scoring_num_tokens(doc)),
            };
            if let Some(limit) = first_score_limit {
                // SAFETY: every window points at its block's frequencies and
                // the merge keeps offsets inside the slices.
                let freq = unsafe { *wins[0].freqs.add(offs[0] as usize) };
                let first_score = match norm_addend {
                    Some(addend) => {
                        self.lead[0].query_weight * bm25_doc_weight_with_norm(freq, addend)
                    }
                    None => self.lead[0].score(scorer, freq, doc_length),
                };
                if first_score <= limit {
                    #[cfg(test)]
                    test_counters::pruned_candidate();
                    continue;
                }
            }
            self.comparisons += 1;
            #[cfg(test)]
            test_counters::collected_candidate();
            self.cands.push((u64::from(doc), norm_addend, doc_length));
            for (win, &off) in wins.iter().zip(offs) {
                // SAFETY: every window points at its block's frequencies and
                // the merge keeps offsets inside the slices.
                self.cand_freqs
                    .push(unsafe { *win.freqs.add(off as usize) });
            }
        }
    }

    fn record_metrics(&mut self) {
        if !self.metrics_recorded {
            self.metrics.record_comparisons(self.comparisons);
            self.metrics_recorded = true;
        }
    }
}

impl<D: WandDocuments> Drop for TermAndCursor<'_, D> {
    fn drop(&mut self) {
        self.record_metrics();
        #[cfg(test)]
        if let Some(array) = &self.array {
            test_counters::array_pruned(array.stats.windows_skipped + array.stats.lut_prepruned);
        }
    }
}

#[cfg(test)]
pub(in crate::scalar::inverted) mod test_counters {
    use std::cell::Cell;

    thread_local! {
        static SKIPPED_WINDOWS: Cell<usize> = const { Cell::new(0) };
        static PRUNED_CANDIDATES: Cell<usize> = const { Cell::new(0) };
        static ARRAYED_BUILT: Cell<usize> = const { Cell::new(0) };
        static ARRAY_PRUNED: Cell<usize> = const { Cell::new(0) };
        static COLLECTED: Cell<usize> = const { Cell::new(0) };
        static SCORED: Cell<usize> = const { Cell::new(0) };
        static HANDED: Cell<usize> = const { Cell::new(0) };
    }

    /// `(windows skipped by their bound, candidates pruned by the lead score)`
    /// on this thread since the previous call.
    pub(in crate::scalar::inverted) fn take() -> (usize, usize) {
        (
            SKIPPED_WINDOWS.with(|count| count.replace(0)),
            PRUNED_CANDIDATES.with(|count| count.replace(0)),
        )
    }

    pub(super) fn skipped_window() {
        SKIPPED_WINDOWS.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn pruned_candidate() {
        PRUNED_CANDIDATES.with(|count| count.set(count.get() + 1));
    }

    /// `(array-led conjunctions built, lead windows and candidates their array
    /// lead pruned)` on this thread since the previous call.
    pub(in crate::scalar::inverted) fn take_arrayed() -> (usize, usize) {
        (
            ARRAYED_BUILT.with(|count| count.replace(0)),
            ARRAY_PRUNED.with(|count| count.replace(0)),
        )
    }

    pub(super) fn arrayed_built() {
        ARRAYED_BUILT.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn array_pruned(pruned: usize) {
        ARRAY_PRUNED.with(|count| count.set(count.get() + pruned));
    }

    /// `(candidates collected, candidates fully scored, candidates handed out)`
    /// on this thread since the previous call.
    pub(in crate::scalar::inverted) fn take_lazy() -> (usize, usize, usize) {
        (
            COLLECTED.with(|count| count.replace(0)),
            SCORED.with(|count| count.replace(0)),
            HANDED.with(|count| count.replace(0)),
        )
    }

    pub(super) fn collected_candidate() {
        COLLECTED.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn scored_candidate() {
        SCORED.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn handed_candidate() {
        HANDED.with(|count| count.set(count.get() + 1));
    }
}
