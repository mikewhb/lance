//! Lead-block region bulk AND.
//!
//! Region upper bounds are group-granular (§5.2.1). Tight pruning stays on the
//! per-block bound, which only touches blocks that still have candidates.

use arrow_array::Array;
use lance_core::{Error, Result};

use super::intersection::intersect_indices;
use super::{
    DocCandidate, FtsSearchParams, MetricsCollector, PostingIterator, PostingList, ScoredDoc,
    Scorer, TopKCollector, Wand, WandDocuments, bm25_doc_weight_with_norm, dequantize_doc_length,
    find_next_geq, next_up_f64, score_sum_cannot_compete, score_sum_upper_bound_factor,
};
#[cfg(test)]
use super::{ExactPolicy, RegionBound, RegionIntersect};
use super::{IMPACT_LEVEL1_BLOCKS, MAX_POSTING_BLOCK_SIZE};

const REGION_MAX_SPAN: u64 = 1 << 16;
/// Edge groups this small use per-block bounds. A structural cutoff, not a
/// tuned threshold: both choices are safe upper bounds.
const EDGE_BLOCKS: usize = 4;
/// Comparison-model crossover. S3 seed; S6 replaces it from the microbench.
const MERGE_RATIO: usize = 8;
const FREQ_LUT_BUCKETS: usize = 64;
/// Nanosecond seeds. Only the decision to run exact partial scores moves;
/// S6 overwrites them. They are not fitted on the 943-query set.
const C_DEC_NS: f64 = 150.0;
const C_DEC_FREQ_NS: f64 = 40.0;
const C_GEQ_NS: f64 = 5.0;
const C_NORM_NS: f64 = 15.0;
const C_BM25_NS: f64 = 3.0;

macro_rules! tally {
    ($wand:ident.$field:ident += $n:expr) => {
        #[cfg(test)]
        {
            $wand.$field += $n;
        }
    };
}

struct Rows {
    n: usize,
    cap: usize,
    k: usize,
    docs: Vec<u32>,
    freq: Vec<u32>,
    norm: Vec<u8>,
    doc_len: Vec<u32>,
    norm_ok: Vec<bool>,
    lut_ub: Vec<f64>,
    scored_lut: Vec<u8>,
    partial: Vec<f32>,
    scored_exact: Vec<u8>,
}

impl Rows {
    fn new(k: usize) -> Self {
        let cap = MAX_POSTING_BLOCK_SIZE;
        Self {
            n: 0,
            cap,
            k,
            docs: vec![0; cap],
            freq: vec![0; k * cap],
            norm: vec![0; cap],
            doc_len: vec![0; cap],
            norm_ok: vec![false; cap],
            lut_ub: vec![0.0; cap],
            scored_lut: vec![0; cap],
            partial: vec![0.0; cap],
            scored_exact: vec![0; cap],
        }
    }

    fn freq(&self, clause: usize, index: usize) -> u32 {
        self.freq[clause * self.cap + index]
    }

    fn freq_mut(&mut self, clause: usize, index: usize) -> &mut u32 {
        &mut self.freq[clause * self.cap + index]
    }

    fn copy_row(&mut self, src: usize, dst: usize, clauses: usize) {
        if src == dst {
            return;
        }
        self.docs[dst] = self.docs[src];
        for clause in 0..clauses {
            let copied = self.freq(clause, src);
            *self.freq_mut(clause, dst) = copied;
        }
        self.norm[dst] = self.norm[src];
        self.doc_len[dst] = self.doc_len[src];
        self.norm_ok[dst] = self.norm_ok[src];
        self.lut_ub[dst] = self.lut_ub[src];
        self.scored_lut[dst] = self.scored_lut[src];
        self.partial[dst] = self.partial[src];
        self.scored_exact[dst] = self.scored_exact[src];
    }
}

impl<'a, S: Scorer, D: WandDocuments> Wand<'a, S, D> {
    pub(super) fn and_region_search(
        &mut self,
        params: &FtsSearchParams,
        metrics: &dyn MetricsCollector,
    ) -> Result<Vec<DocCandidate<D::Candidate>>> {
        let limit = params.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return Ok(vec![]);
        }
        // Phrase positions stay on the main kernel until the position-state
        // path is switched on. The route already requires `phrase_slop` none.
        debug_assert!(params.phrase_slop.is_none());
        self.invalidate_score_first_and_window();
        self.score_first_and_enabled = false;

        let k = self.lead.len();
        let mut score_order = (0..k).collect::<Vec<_>>();
        score_order.sort_unstable_by_key(|&index| {
            let posting = &self.lead[index];
            (posting.position, posting.token_id)
        });
        let factor = score_sum_upper_bound_factor(k);
        let mut rows = Rows::new(k);
        let mut lut: Vec<[f32; FREQ_LUT_BUCKETS]> = Vec::new();
        let mut fcur = (0..k)
            .map(|j| (self.lead[j].block_idx, 0usize))
            .collect::<Vec<_>>();
        let mut candidates = TopKCollector::new(limit, std::cmp::min(limit, 256));
        let mut num_comparisons = 0usize;
        let norm_k = self.norm_k_cache();
        let norms_only = if norm_k.is_none() {
            self.documents.scoring_norms()
        } else {
            None
        };

        let mut target = 0u64;
        for posting in &self.lead {
            match posting.current_doc_id() {
                Some(doc) => target = target.max(doc),
                None => return Ok(vec![]),
            }
        }

        let mut query_done = false;
        while !query_done {
            tally!(self.regions += 1);
            self.raise_to_shared_floor(params.wand_factor);
            let has_floor = self.threshold > 0.0;
            let mut max_follower_doc = 0u64;

            for posting in &mut self.lead {
                let PostingList::Compressed(list) = &posting.list else {
                    return Err(Error::internal(
                        "region AND requires compressed postings".to_string(),
                    ));
                };
                let target32 = u32::try_from(target).unwrap_or(u32::MAX);
                posting.block_idx = posting.block_idx_for_doc(list, posting.block_idx, target32);
            }

            let (lead_block, region_end) = {
                let posting = &self.lead[0];
                let PostingList::Compressed(list) = &posting.list else {
                    return Err(Error::internal(
                        "region AND requires compressed postings".to_string(),
                    ));
                };
                let block = posting.block_idx;
                let block_end = if block + 1 >= list.blocks.len() {
                    u64::from(u32::MAX)
                } else {
                    u64::from(list.block_least_doc_id(block + 1)).saturating_sub(1)
                };
                let end = block_end.min(target.saturating_add(REGION_MAX_SPAN - 1));
                (block, end.max(target))
            };
            let region_end32 = u32::try_from(region_end).unwrap_or(u32::MAX);
            let target32 = u32::try_from(target).unwrap_or(u32::MAX);

            let (lo, hi, lead_len) = {
                let posting = &self.lead[0];
                let PostingList::Compressed(list) = &posting.list else {
                    return Err(Error::internal(
                        "region AND requires compressed postings".to_string(),
                    ));
                };
                let state = unsafe { &*posting.ensure_compressed_doc_ids_ptr(list, lead_block) };
                let doc_ids = state.doc_ids.as_slice();
                let lo = doc_ids.partition_point(|&doc| doc < target32);
                let hi = lo + doc_ids[lo..].partition_point(|&doc| doc <= region_end32);
                (lo, hi, doc_ids.len())
            };
            if lo == hi {
                if hi < lead_len {
                    let posting = &self.lead[0];
                    let PostingList::Compressed(list) = &posting.list else {
                        break;
                    };
                    let state =
                        unsafe { &*posting.ensure_compressed_doc_ids_ptr(list, lead_block) };
                    target = u64::from(state.doc_ids[hi]);
                    continue;
                }
                let exhausted = {
                    let PostingList::Compressed(list) = &self.lead[0].list else {
                        break;
                    };
                    lead_block + 1 >= list.blocks.len()
                };
                if exhausted {
                    break;
                }
                let PostingList::Compressed(list) = &self.lead[0].list else {
                    break;
                };
                target = u64::from(list.block_least_doc_id(lead_block + 1));
                continue;
            }

            let mut bounds_ok = false;
            let mut suffix = vec![0.0f64; k + 1];
            if has_floor {
                if lut.is_empty() {
                    lut = (0..k)
                        .map(|clause| clause_lut(&self.lead[clause], &self.scorer))
                        .collect();
                }
                bounds_ok = true;
                let mut upper = vec![0.0f32; k];
                upper[0] = block_bound_at(&self.lead[0], &self.scorer, lead_block);
                #[allow(clippy::needless_range_loop)]
                for clause in 1..k {
                    upper[clause] = if self.region_bound_is_deque() {
                        self.lead[clause]
                            .block_max_score_up_to_with_stats(region_end, &self.scorer)
                            .score
                    } else {
                        self.region_bound(clause, region_end32)
                    };
                }
                if upper
                    .iter()
                    .any(|bound| !bound.is_finite() || *bound <= 0.0)
                {
                    bounds_ok = false;
                }
                if bounds_ok {
                    for clause in (0..k).rev() {
                        suffix[clause] = next_up_f64(f64::from(upper[clause]) + suffix[clause + 1]);
                    }
                    if score_sum_cannot_compete(
                        0.0,
                        suffix[0],
                        self.threshold,
                        factor,
                        self.floor_mode,
                    ) {
                        tally!(self.regions_skipped += 1);
                        if region_end >= u64::from(u32::MAX) {
                            break;
                        }
                        let next = region_end + 1;
                        debug_assert!(next > target);
                        target = next;
                        continue;
                    }
                }
            }

            {
                let posting = &self.lead[0];
                let PostingList::Compressed(list) = &posting.list else {
                    break;
                };
                let state = unsafe { &*posting.ensure_compressed_block_ptr(list, lead_block) };
                rows.n = hi - lo;
                rows.docs[..rows.n].copy_from_slice(&state.doc_ids[lo..hi]);
                for (offset, index) in (0..rows.n).enumerate() {
                    *rows.freq_mut(0, index) = state.freqs[lo + offset];
                    rows.norm_ok[index] = false;
                    rows.scored_lut[index] = 0;
                    rows.scored_exact[index] = 0;
                    rows.lut_ub[index] = 0.0;
                    rows.partial[index] = 0.0;
                }
            }

            for clause in 1..k {
                if rows.n == 0 {
                    break;
                }
                let done = self.intersect_follower(
                    clause,
                    &mut rows,
                    &mut fcur,
                    &lut,
                    &suffix,
                    bounds_ok && has_floor,
                    factor,
                    norm_k.as_ref(),
                    norms_only,
                    &mut max_follower_doc,
                )?;
                // Exhausting one follower ends the query after every clause
                // has filtered this region. It must not skip the clauses that
                // have not run yet.
                if done {
                    query_done = true;
                }
                if rows.n == 0 {
                    break;
                }
            }

            if rows.n > 0 {
                self.collect_region(
                    &mut rows,
                    &score_order,
                    norm_k.as_ref(),
                    norms_only,
                    &mut candidates,
                    &mut num_comparisons,
                    params.wand_factor,
                )?;
            }

            if query_done || region_end >= u64::from(u32::MAX) {
                break;
            }
            let next = (region_end + 1).max(max_follower_doc);
            debug_assert!(next > target);
            target = next;
        }

        metrics.record_comparisons(num_comparisons);
        candidates.into_candidates(|key| self.documents.candidate_from_key(key))
    }

    fn region_bound_is_deque(&self) -> bool {
        #[cfg(test)]
        {
            self.force_region_bound == Some(RegionBound::Deque)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    pub(super) fn region_bound(&mut self, clause: usize, hi: u32) -> f32 {
        tally!(self.region_bound_lookups += 1);
        let (b0, b1, has_impacts) = {
            let posting = &self.lead[clause];
            let PostingList::Compressed(list) = &posting.list else {
                return f32::INFINITY;
            };
            let b0 = posting.block_idx;
            let b1 = posting.block_idx_for_doc(list, b0, hi);
            (b0, b1, list.impacts.is_some())
        };
        if b1 < b0 {
            return f32::INFINITY;
        }
        if has_impacts {
            let g0 = b0 / IMPACT_LEVEL1_BLOCKS;
            let g1 = b1 / IMPACT_LEVEL1_BLOCKS;
            let mut acc = 0.0f32;
            #[cfg(test)]
            let mut level0_reads = 0usize;
            #[cfg(test)]
            let mut level1_reads = 0usize;
            for group in g0..=g1 {
                let start = b0.max(group * IMPACT_LEVEL1_BLOCKS);
                let end = b1.min(group * IMPACT_LEVEL1_BLOCKS + IMPACT_LEVEL1_BLOCKS - 1);
                if end < start {
                    continue;
                }
                if end - start < EDGE_BLOCKS {
                    for block in start..=end {
                        let bound = block_bound_at(&self.lead[clause], &self.scorer, block);
                        #[cfg(test)]
                        {
                            level0_reads += 1;
                        }
                        if !bound.is_finite() {
                            tally!(self.region_bound_level0 += level0_reads);
                            tally!(self.region_bound_level1 += level1_reads);
                            return f32::INFINITY;
                        }
                        acc = acc.max(bound);
                    }
                } else {
                    let missing = {
                        let PostingList::Compressed(list) = &self.lead[clause].list else {
                            return f32::INFINITY;
                        };
                        list.impacts
                            .as_ref()
                            .is_none_or(|impacts| impacts.level1_doc_up_to(group).is_none())
                    };
                    if missing {
                        tally!(self.region_bound_level0 += level0_reads);
                        tally!(self.region_bound_level1 += level1_reads);
                        return f32::INFINITY;
                    }
                    let bound = {
                        let posting = &self.lead[clause];
                        let PostingList::Compressed(list) = &posting.list else {
                            return f32::INFINITY;
                        };
                        let Some(impacts) = list.impacts.as_ref() else {
                            return f32::INFINITY;
                        };
                        let state = unsafe { &mut *posting.compressed_state_ptr() };
                        impacts.level1_score_cached(
                            group,
                            posting.query_weight,
                            &self.scorer,
                            &mut state.block_max_window.impact_score_cache,
                        )
                    };
                    #[cfg(test)]
                    {
                        level1_reads += 1;
                    }
                    acc = acc.max(bound);
                }
            }
            tally!(self.region_bound_level0 += level0_reads);
            tally!(self.region_bound_level1 += level1_reads);
            acc
        } else if b1 - b0 < EDGE_BLOCKS * 2 {
            let acc = {
                let PostingList::Compressed(list) = &self.lead[clause].list else {
                    return f32::INFINITY;
                };
                let mut acc = 0.0f32;
                for block in b0..=b1 {
                    acc = acc.max(list.block_max_score(block));
                }
                acc
            };
            tally!(self.region_bound_level0 += b1 - b0 + 1);
            acc
        } else {
            self.lead[clause].frequency_clamp_upper_bound(&self.scorer)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn intersect_follower(
        &mut self,
        clause: usize,
        rows: &mut Rows,
        fcur: &mut [(usize, usize)],
        lut: &[[f32; FREQ_LUT_BUCKETS]],
        suffix: &[f64],
        prune: bool,
        factor: f64,
        norm_k: Option<&super::NormKCache<'a>>,
        norms_only: Option<&'a [u8]>,
        max_follower_doc: &mut u64,
    ) -> Result<bool> {
        let (mut fb, mut fpos) = fcur[clause];
        let mut last_examined = None;
        let mut out = 0usize;
        let mut index = 0usize;
        let mut query_done = false;
        while index < rows.n {
            let doc = rows.docs[index];
            let (num_blocks, advanced) = {
                let posting = &self.lead[clause];
                let PostingList::Compressed(list) = &posting.list else {
                    return Err(Error::internal(
                        "region AND requires compressed postings".to_string(),
                    ));
                };
                if fb + 1 < list.blocks.len() && doc >= list.block_least_doc_id(fb + 1) {
                    fb = posting.block_idx_for_doc(list, fb, doc);
                    fpos = 0;
                }
                (list.blocks.len(), fb)
            };
            fb = advanced;
            let block_end = {
                let PostingList::Compressed(list) = &self.lead[clause].list else {
                    return Err(Error::internal(
                        "region AND requires compressed postings".to_string(),
                    ));
                };
                if fb + 1 >= num_blocks {
                    u32::MAX
                } else {
                    list.block_least_doc_id(fb + 1).saturating_sub(1)
                }
            };
            let seg_end = index + rows.docs[index..rows.n].partition_point(|&doc| doc <= block_end);
            debug_assert!(seg_end > index);
            let seg_last_doc = rows.docs[seg_end - 1];
            let mut seg_keep = seg_end;
            if prune {
                seg_keep = self.prune_segment(
                    clause, rows, index, seg_end, fb, lut, suffix, factor, norm_k, norms_only,
                );
            }
            if seg_keep == index {
                tally!(self.segments_pruned += 1);
                index = seg_end;
                continue;
            }

            let posting = &self.lead[clause];
            let PostingList::Compressed(list) = &posting.list else {
                return Err(Error::internal(
                    "region AND requires compressed postings".to_string(),
                ));
            };
            let state = unsafe { &*posting.ensure_compressed_doc_ids_ptr(list, fb) };
            let len_b = state.doc_ids.len();
            let docs_ptr = state.doc_ids.as_ptr();
            last_examined = Some(fb);
            tally!(self.follower_doc_blocks_decoded += 1);
            let block_docs = unsafe { std::slice::from_raw_parts(docs_ptr, len_b) };
            let n_b = seg_keep - index;
            let m_b = len_b.saturating_sub(fpos);
            let use_merge = {
                #[cfg(test)]
                {
                    match self.force_intersect {
                        Some(RegionIntersect::Merge) => true,
                        Some(RegionIntersect::Gallop) => false,
                        None => n_b * MERGE_RATIO >= m_b,
                    }
                }
                #[cfg(not(test))]
                {
                    n_b * MERGE_RATIO >= m_b
                }
            };
            let mut hits = Vec::with_capacity(n_b);
            if use_merge {
                tally!(self.merge_segments += 1);
                let mut pairs = Vec::new();
                intersect_indices(
                    &rows.docs[index..seg_keep],
                    &block_docs[fpos..len_b],
                    &mut pairs,
                );
                for (left, right) in pairs {
                    hits.push((index + left as usize, fpos + right as usize));
                }
            } else {
                tally!(self.gallop_segments += 1);
                let mut pos = fpos;
                for candidate in index..seg_keep {
                    let doc = rows.docs[candidate];
                    pos = unsafe { find_next_geq(docs_ptr, pos, len_b, doc) };
                    if pos >= len_b {
                        break;
                    }
                    if unsafe { *docs_ptr.add(pos) } == doc {
                        hits.push((candidate, pos));
                    }
                }
            }
            let mut wrote_freq = false;
            let mut freq_ptr = std::ptr::null();
            for (source, off) in hits {
                if !wrote_freq {
                    let state = unsafe { &*posting.ensure_compressed_block_ptr(list, fb) };
                    freq_ptr = state.freqs.as_ptr();
                    wrote_freq = true;
                }
                rows.copy_row(source, out, clause);
                *rows.freq_mut(clause, out) = unsafe { *freq_ptr.add(off) };
                out += 1;
            }
            let scan_from = fpos;
            fpos = if seg_last_doc == u32::MAX {
                len_b
            } else {
                unsafe { find_next_geq(docs_ptr, scan_from, len_b, seg_last_doc + 1) }
            };
            if fpos < len_b {
                *max_follower_doc = (*max_follower_doc).max(u64::from(block_docs[fpos]));
            } else if fb + 1 >= num_blocks {
                query_done = true;
                break;
            } else {
                *max_follower_doc =
                    (*max_follower_doc).max(u64::from(list.block_least_doc_id(fb + 1)));
                fb += 1;
                fpos = 0;
            }
            index = seg_end;
        }
        rows.n = out;
        fcur[clause] = (fb, fpos);
        if let Some(examined) = last_examined {
            self.lead[clause].block_idx = self.lead[clause].block_idx.max(examined);
        }
        Ok(query_done)
    }

    #[allow(clippy::too_many_arguments)]
    fn prune_segment(
        &mut self,
        clause: usize,
        rows: &mut Rows,
        start: usize,
        end: usize,
        block: usize,
        lut: &[[f32; FREQ_LUT_BUCKETS]],
        suffix: &[f64],
        factor: f64,
        norm_k: Option<&super::NormKCache<'a>>,
        norms_only: Option<&'a [u8]>,
    ) -> usize {
        let block_bound = block_bound_at(&self.lead[clause], &self.scorer, block);
        let rem = next_up_f64(f64::from(block_bound) + suffix[clause + 1]);
        let min_prefix = lut
            .iter()
            .take(clause)
            .map(|row| f64::from(row[1]))
            .sum::<f64>();
        let skip_lut = !score_sum_cannot_compete(
            0.0,
            min_prefix + rem,
            self.threshold,
            factor,
            self.floor_mode,
        );
        let mut write = start;
        if !skip_lut {
            for src in start..end {
                ensure_lut(rows, lut, src, clause);
                let reject = score_sum_cannot_compete(
                    0.0,
                    rows.lut_ub[src] + rem,
                    self.threshold,
                    factor,
                    self.floor_mode,
                );
                if !reject {
                    rows.copy_row(src, write, clause);
                    write += 1;
                }
            }
        } else {
            write = end;
        }
        let survivors = write - start;
        if survivors == 0 {
            return start;
        }
        let decoded = {
            let state = unsafe { &*self.lead[clause].compressed_state_ptr() };
            state.block_idx == block && !state.doc_ids.is_empty()
        };
        let run_exact = !decoded && self.exact_policy_runs(rows, start, write, clause);
        if !run_exact {
            return write;
        }
        let mut exact_write = start;
        for src in start..write {
            self.ensure_norm(rows, src, norm_k, norms_only);
            ensure_partial(rows, &self.lead, &self.scorer, norm_k, src, clause);
            let reject = score_sum_cannot_compete(
                rows.partial[src],
                rem,
                self.threshold,
                factor,
                self.floor_mode,
            );
            if !reject {
                rows.copy_row(src, exact_write, clause);
                exact_write += 1;
            }
        }
        exact_write
    }

    fn exact_policy_runs(&self, rows: &Rows, start: usize, end: usize, clause: usize) -> bool {
        #[cfg(test)]
        match self.force_exact {
            Some(ExactPolicy::Always) => return true,
            Some(ExactPolicy::Never) => return false,
            Some(ExactPolicy::Auto) | None => {}
        }
        let mut cost = 0.0f64;
        for index in start..end {
            if !rows.norm_ok[index] {
                cost += C_NORM_NS;
            }
            let pending = clause.saturating_sub(rows.scored_exact[index] as usize);
            cost += pending as f64 * C_BM25_NS;
        }
        let saved = C_DEC_NS + C_DEC_FREQ_NS + (end - start) as f64 * C_GEQ_NS;
        cost <= saved / 2.0
    }

    fn ensure_norm(
        &self,
        rows: &mut Rows,
        index: usize,
        norm_k: Option<&super::NormKCache<'a>>,
        norms_only: Option<&'a [u8]>,
    ) {
        if rows.norm_ok[index] {
            return;
        }
        let doc = rows.docs[index];
        if let Some((norms, _)) = norm_k {
            rows.norm[index] = norms[doc as usize];
        } else if let Some(norms) = norms_only {
            rows.doc_len[index] = dequantize_doc_length(norms[doc as usize]);
        } else {
            rows.doc_len[index] = self.documents.scoring_num_tokens(doc);
        }
        rows.norm_ok[index] = true;
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_region(
        &mut self,
        rows: &mut Rows,
        score_order: &[usize],
        norm_k: Option<&super::NormKCache<'a>>,
        norms_only: Option<&'a [u8]>,
        candidates: &mut TopKCollector,
        num_comparisons: &mut usize,
        wand_factor: f32,
    ) -> Result<()> {
        // Gather only the candidates whose norm was not already touched by
        // an exact segment prune.
        let mut pending = Vec::new();
        for index in 0..rows.n {
            if !rows.norm_ok[index] {
                pending.push(index);
            }
        }
        for index in pending {
            self.ensure_norm(rows, index, norm_k, norms_only);
        }
        for index in 0..rows.n {
            *num_comparisons += 1;
            let Some(key) = self.documents.document_key_for_doc_id(rows.docs[index]) else {
                continue;
            };
            let (addend, doc_len) = row_length(rows, index, norm_k);
            let mut score = 0.0f32;
            for &clause in score_order {
                score += clause_contrib(
                    &self.lead[clause],
                    &self.scorer,
                    rows.freq(clause, index),
                    addend,
                    doc_len,
                );
            }
            if self.threshold > 0.0 && self.exclusive_score_cannot_beat_floor(score) {
                continue;
            }
            let freqs = (0..rows.k)
                .map(|clause| (self.lead[clause].term_index(), rows.freq(clause, index)));
            if candidates.insert(
                ScoredDoc::new(key, score),
                doc_len,
                u64::from(rows.docs[index]),
                freqs,
            )? && let Some(kth) = candidates.kth_score_if_full()
            {
                self.update_threshold(kth, wand_factor);
            }
        }
        Ok(())
    }
}

fn clause_lut<S: Scorer + ?Sized>(
    posting: &PostingIterator,
    scorer: &S,
) -> [f32; FREQ_LUT_BUCKETS] {
    let mut lut = [0.0f32; FREQ_LUT_BUCKETS];
    for (freq, slot) in lut.iter_mut().enumerate().take(FREQ_LUT_BUCKETS - 1) {
        *slot = posting.score(scorer, freq as u32, 0);
    }
    lut[FREQ_LUT_BUCKETS - 1] = posting.frequency_clamp_upper_bound(scorer);
    lut
}

/// Block upper bound copied from `PostingIterator::block_max_score` and
/// `impact_level0`, addressed by `block` instead of `self.block_idx`.
///
/// A missing or malformed level0 entry returns `+∞`. `level0_score_cached`
/// returns 0 past the end, which is a lower bound and must not prune.
/// Grouped terms, 256-document blocks, and scorer-wide ceilings are excluded
/// by the region route.
pub(super) fn block_bound_at<S: Scorer + ?Sized>(
    posting: &PostingIterator,
    scorer: &S,
    block: usize,
) -> f32 {
    let PostingList::Compressed(list) = &posting.list else {
        return f32::INFINITY;
    };
    debug_assert!(!posting.has_grouped_terms());
    debug_assert!(list.block_size != MAX_POSTING_BLOCK_SIZE);
    debug_assert!(!posting.use_scorer_upper_bound);
    if let Some(impacts) = list.impacts.as_ref() {
        if impacts.level0_doc_up_to(block).is_none() {
            return f32::INFINITY;
        }
        let state = unsafe { &mut *posting.compressed_state_ptr() };
        return impacts.level0_score_cached(
            block,
            posting.query_weight,
            scorer,
            &mut state.block_max_window.impact_score_cache,
        );
    }
    list.block_max_score(block)
}

fn ensure_lut(rows: &mut Rows, lut: &[[f32; FREQ_LUT_BUCKETS]], index: usize, up_to: usize) {
    let mut clause = rows.scored_lut[index] as usize;
    let mut acc = rows.lut_ub[index];
    while clause < up_to {
        let freq = rows.freq(clause, index) as usize;
        let bucket = freq.min(FREQ_LUT_BUCKETS - 1);
        acc += f64::from(lut[clause][bucket]);
        clause += 1;
    }
    rows.lut_ub[index] = acc;
    rows.scored_lut[index] = clause as u8;
}

fn ensure_partial<S: Scorer + ?Sized>(
    rows: &mut Rows,
    lead: &[Box<PostingIterator>],
    scorer: &S,
    norm_k: Option<&super::NormKCache<'_>>,
    index: usize,
    up_to: usize,
) {
    let (addend, doc_len) = row_length(rows, index, norm_k);
    let mut clause = rows.scored_exact[index] as usize;
    let mut acc = rows.partial[index];
    while clause < up_to {
        acc += clause_contrib(
            lead[clause].as_ref(),
            scorer,
            rows.freq(clause, index),
            addend,
            doc_len,
        );
        clause += 1;
    }
    rows.partial[index] = acc;
    rows.scored_exact[index] = clause as u8;
}

fn row_length(
    rows: &Rows,
    index: usize,
    norm_k: Option<&super::NormKCache<'_>>,
) -> (Option<f32>, u32) {
    if let Some((_, addends)) = norm_k {
        let code = rows.norm[index];
        (Some(addends[code as usize]), dequantize_doc_length(code))
    } else {
        (None, rows.doc_len[index])
    }
}

/// One clause of `window_bm25_score`: norm-cache addend, otherwise `score`.
fn clause_contrib<S: Scorer + ?Sized>(
    posting: &PostingIterator,
    scorer: &S,
    freq: u32,
    addend: Option<f32>,
    doc_len: u32,
) -> f32 {
    match addend {
        Some(addend) => posting.query_weight * bm25_doc_weight_with_norm(freq, addend),
        None => posting.score(scorer, freq, doc_len),
    }
}
