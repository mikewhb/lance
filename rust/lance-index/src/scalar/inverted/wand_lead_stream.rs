// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Lead-stream conjunction search: the shortest list's decompressed block is
//! the buffer, followers only `next(target)`.
//!
//! Community Auto still uses N-way bulk for balanced 2/3 (and modern 4+).
//! When `max_cost / min_cost >= AND_SKEW_RATIO`, bulk would decompress the
//! stopword in every window the rare term barely touches. Auto conjunctions
//! with three or more clauses that do not take bulk therefore take this
//! lead-stream, including unskewed Wikipedia block-128 4+. A skewed pair
//! stays on classic leapfrog (Lucene `ConjunctionDISI` — one follower makes
//! the buffer pure overhead).
//!
//! Inner score-first is not a dispatch table. A non-phrase leftover scores
//! a lead-block buffer of exact BM25 and applies followers only to
//! survivors when the second list is at least `AND_SCORE_FIRST_COST_RATIO`
//! times the lead, or once a competitive floor exists (`threshold > 0`).
//! The ratio still gates the heap-empty path; balanced leftover would
//! otherwise pay lead BM25 on every doc and still seek. Phrase
//! confirmation stays on the existing per-doc score-then-positions path.

use lance_core::Result;
use smallvec::SmallVec;

use super::super::builder::ScoredDoc;
use super::super::encoding::MAX_POSTING_BLOCK_SIZE;
use super::super::query::FtsSearchParams;
use super::super::scorer::Scorer;
use super::maxscore::bm25_tf_from_caches;
use super::{
    BLOCK_SIZE, CompetitiveFloorMode, DocCandidate, DocInfo, ExactBm25Addends, PostingIterator,
    PostingList, RawDocInfo, ScoreContribution, TERMINATED_DOC_ID, TopKCollector, Wand,
    WandDocuments, conservative_score_sum, exact_bm25_addend_slab,
    score_contributions_in_query_order, score_sum_cannot_compete, score_sum_upper_bound_factor,
};
use crate::metrics::MetricsCollector;

/// Cost ratio at which Auto leaves N-way bulk. Same 32× Wikipedia number
/// the analysis tree used for n≤3; not a term-count bucket.
pub(super) const AND_SKEW_RATIO: usize = 32;

/// Heap-empty gate: score the rare lead before seeking followers when the
/// second list is at least this many times longer. Same 3× shape as IU
/// promotion. After `threshold > 0` the block kernel does not consult this
/// ratio (Lucene `BlockMaxConjunction` after the heap fills).
const AND_SCORE_FIRST_COST_RATIO: usize = 3;

/// Heap-empty batches so Exclusive pruning can start (Lucene fills the
/// heap before scoring the rest of the lead block). Ignored when `limit`
/// cannot fill the heap. Leftover windows reuse this as the score-first
/// probe before deciding whether the rest of the window is intersect-first.
const HEAP_FILL_BATCH: usize = 16;

/// Switch a leftover window's remainder to intersect-first when the probe's
/// Exclusive skip rate is below this. Sits in the measured Wikipedia gap
/// (LA ~11% vs care / Hamlet / book-of-life ≥50%).
const AND_INTERSECT_FIRST_MAX_PROBE_SKIP: f32 = 0.25;

struct LeadStreamBlockOutcome {
    follower_exhausted: bool,
    lead_bm25: u32,
    floor_skip: u32,
}

fn leftover_rest_uses_intersect_first(lead_bm25: u32, floor_skip: u32) -> bool {
    lead_bm25 > 0 && (floor_skip as f32) / (lead_bm25 as f32) < AND_INTERSECT_FIRST_MAX_PROBE_SKIP
}

const FREQ_LUT_BUCKETS: usize = 64;

enum LeadFollowerSeek {
    Match,
    Leap(u64),
    Exhausted,
}

pub(super) fn conjunction_lists_are_skewed(min_cost: usize, max_cost: usize) -> bool {
    min_cost > 0 && max_cost / min_cost >= AND_SKEW_RATIO
}

fn and_score_first_for(num_lists: usize, lead_cost: usize, second_cost: usize) -> bool {
    num_lists >= 3 && second_cost >= lead_cost.saturating_mul(AND_SCORE_FIRST_COST_RATIO)
}

fn score_first_batch_len(remaining: usize, threshold: f32, heap_can_fill: bool) -> usize {
    if remaining == 0 {
        0
    } else if threshold > 0.0 || !heap_can_fill {
        remaining
    } else {
        HEAP_FILL_BATCH.min(remaining)
    }
}

fn clause_bm25<S: Scorer, D: WandDocuments>(
    posting: &PostingIterator,
    scorer: &S,
    documents: &D,
    freq: u32,
    doc: u32,
    norm_k: Option<(&[u8], &[f32; 256])>,
    exact_addends: Option<ExactBm25Addends<'_>>,
) -> f32 {
    bm25_tf_from_caches(posting.query_weight, freq, doc, norm_k, exact_addends)
        .unwrap_or_else(|| posting.score(scorer, freq, documents.scoring_num_tokens(doc)))
}

impl PostingIterator {
    /// Copy remaining docs in the current decompressed block at or before
    /// `window_max` without moving the cursor. Returns the absolute posting
    /// index of the first copied doc.
    fn peek_remaining_block_docs_upto(
        &mut self,
        window_max: u64,
        docs: &mut Vec<u32>,
        freqs: &mut Vec<u32>,
    ) -> Option<usize> {
        docs.clear();
        freqs.clear();
        let PostingList::Compressed(ref list) = self.list else {
            return None;
        };
        let cur = self.current_doc?;
        if cur.doc_id() > window_max {
            return None;
        }
        let shift = list.block_shift();
        let block_idx = self.index >> shift;
        let block_offset = self.index & list.block_mask();
        // SAFETY: `ensure_compressed_block_ptr` returns the iterator's
        // `UnsafeCell` compressed state. This iterator is uniquely borrowed
        // here; the copies below finish before any other alias is taken.
        let compressed = unsafe { &mut *self.ensure_compressed_block_ptr(list, block_idx) };
        for offset in block_offset..compressed.doc_ids.len() {
            let doc_id = compressed.doc_ids[offset];
            if u64::from(doc_id) > window_max {
                break;
            }
            docs.push(doc_id);
            freqs.push(compressed.freqs[offset]);
        }
        if docs.is_empty() {
            None
        } else {
            Some((block_idx << shift) + block_offset)
        }
    }
}

impl<'a, S: Scorer, D: WandDocuments> Wand<'a, S, D> {
    /// Seek `lead[from..to]` onto `doc`.
    fn seek_lead_followers(&mut self, from: usize, to: usize, doc: u32) -> LeadFollowerSeek {
        for posting in self.lead.iter_mut().take(to).skip(from) {
            if posting
                .doc()
                .is_none_or(|cur| cur.doc_id() < u64::from(doc))
            {
                posting.next(u64::from(doc));
            }
            match posting.doc() {
                None => return LeadFollowerSeek::Exhausted,
                Some(cur) if cur.doc_id() > u64::from(doc) => {
                    return LeadFollowerSeek::Leap(cur.doc_id());
                }
                Some(_) => {}
            }
        }
        LeadFollowerSeek::Match
    }

    /// After the two rarest lists sit on `doc`, skip remaining dense
    /// followers when even a conservative Exclusive upper cannot enter
    /// the heap. `rest_block_max` is `lead[2..]` over the current window.
    fn matched_pair_cannot_beat_floor(
        &self,
        lead_freq: u32,
        doc_length: u32,
        rest_block_max: f64,
        num_lists: usize,
    ) -> bool {
        if self.threshold <= 0.0 || num_lists < 3 {
            return false;
        }
        let Some(second) = self.lead.get(1).and_then(|posting| posting.doc()) else {
            return false;
        };
        score_sum_cannot_compete(
            self.lead[0].score(&self.scorer, lead_freq, doc_length),
            f64::from(self.lead[1].score(&self.scorer, second.frequency(), doc_length))
                + rest_block_max,
            self.threshold,
            score_sum_upper_bound_factor(num_lists),
            CompetitiveFloorMode::Exclusive,
        )
    }

    fn park_lead_at(&mut self, index: usize, doc: u32, freq: u32) {
        self.lead[0].index = index;
        if let PostingList::Compressed(ref list) = self.lead[0].list {
            self.lead[0].block_idx = index >> list.block_shift();
        }
        self.lead[0].current_doc = Some(DocInfo::Raw(RawDocInfo {
            doc_id: doc,
            frequency: freq,
        }));
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_lead_stream_hits(
        &mut self,
        docs: &[u32],
        freqs: &[u32],
        clause_scores: &[f32],
        num_lists: usize,
        candidates: &mut TopKCollector,
        num_comparisons: &mut usize,
        wand_factor: f32,
    ) -> Result<()> {
        for (i, &doc) in docs.iter().enumerate() {
            let Some(document_key) = self.documents.document_key_for_doc_id(doc) else {
                continue;
            };
            let base = i * num_lists;
            let contributions = self
                .lead
                .iter()
                .enumerate()
                .map(|(clause, posting)| {
                    (
                        (posting.position, posting.token_id),
                        clause_scores[base + clause],
                    )
                })
                .collect::<SmallVec<[ScoreContribution; 8]>>();
            let score = score_contributions_in_query_order(contributions);
            if candidates.rejects_score(score) {
                continue;
            }
            *num_comparisons += 1;
            let term_freqs = self
                .lead
                .iter()
                .enumerate()
                .map(|(clause, posting)| (posting.term_index(), freqs[base + clause]));
            if candidates.insert(
                ScoredDoc::new(document_key, score),
                self.documents.scoring_num_tokens(doc),
                u64::from(doc),
                term_freqs,
            )? && let Some(kth) = candidates.kth_score_if_full()
            {
                self.update_threshold(kth, wand_factor);
            }
        }
        Ok(())
    }

    /// Score a slice of the lead block: exact lead BM25, then compact followers
    /// onto survivors.
    #[allow(clippy::too_many_arguments)]
    fn and_lead_stream_score_first_block(
        &mut self,
        lead_docs: &[u32],
        lead_freqs: &[u32],
        others_bounds: &[f64],
        others_block_max: f64,
        freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
        num_lists: usize,
        docs: &mut Vec<u32>,
        partial: &mut Vec<f32>,
        freqs: &mut Vec<u32>,
        clause_scores: &mut Vec<f32>,
        candidates: &mut TopKCollector,
        num_comparisons: &mut usize,
        wand_factor: f32,
        norm_k: Option<(&[u8], &[f32; 256])>,
        exact_addends: Option<ExactBm25Addends<'_>>,
    ) -> Result<LeadStreamBlockOutcome> {
        #[cfg(test)]
        {
            self.lead_stream_score_first_blocks += 1;
        }
        let factor = score_sum_upper_bound_factor(num_lists);
        docs.clear();
        partial.clear();
        freqs.clear();
        clause_scores.clear();

        let mut lead_bm25 = 0u32;
        let mut floor_skip = 0u32;
        for (&doc, &freq) in lead_docs.iter().zip(lead_freqs.iter()) {
            let freq_bucket = (freq as usize).min(FREQ_LUT_BUCKETS - 1);
            if freq_cannot_beat[freq_bucket] {
                continue;
            }
            let lead_score = clause_bm25(
                &self.lead[0],
                &self.scorer,
                self.documents,
                freq,
                doc,
                norm_k,
                exact_addends,
            );
            lead_bm25 += 1;
            if self.threshold > 0.0
                && score_sum_cannot_compete(
                    lead_score,
                    others_block_max,
                    self.threshold,
                    factor,
                    CompetitiveFloorMode::Exclusive,
                )
            {
                floor_skip += 1;
                continue;
            }
            docs.push(doc);
            partial.push(lead_score);
            let start = freqs.len();
            freqs.resize(start + num_lists, 0);
            freqs[start] = freq;
            clause_scores.resize(start + num_lists, 0.0);
            clause_scores[start] = lead_score;
        }

        let mut follower_exhausted = false;
        for clause in 1..num_lists {
            if docs.is_empty() {
                return Ok(LeadStreamBlockOutcome {
                    follower_exhausted,
                    lead_bm25,
                    floor_skip,
                });
            }
            let remaining = others_bounds[(clause - 1)..].iter().copied().sum::<f64>();
            let posting = &mut self.lead[clause];
            let mut write = 0usize;
            let mut exhausted = false;
            for read in 0..docs.len() {
                if self.threshold > 0.0
                    && score_sum_cannot_compete(
                        partial[read],
                        remaining,
                        self.threshold,
                        factor,
                        CompetitiveFloorMode::Exclusive,
                    )
                {
                    continue;
                }
                let target = u64::from(docs[read]);
                if posting.doc().is_none_or(|cur| cur.doc_id() < target) {
                    posting.next(target);
                }
                match posting.doc() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(cur) if cur.doc_id() > target => continue,
                    Some(cur) => {
                        let contribution = clause_bm25(
                            posting,
                            &self.scorer,
                            self.documents,
                            cur.frequency(),
                            docs[read],
                            norm_k,
                            exact_addends,
                        );
                        docs[write] = docs[read];
                        partial[write] = partial[read] + contribution;
                        let src = read * num_lists;
                        let dst = write * num_lists;
                        if src != dst {
                            freqs.copy_within(src..src + num_lists, dst);
                            clause_scores.copy_within(src..src + num_lists, dst);
                        }
                        freqs[dst + clause] = cur.frequency();
                        clause_scores[dst + clause] = contribution;
                        write += 1;
                    }
                }
            }
            docs.truncate(write);
            partial.truncate(write);
            freqs.truncate(write * num_lists);
            clause_scores.truncate(write * num_lists);
            follower_exhausted |= exhausted;
        }

        self.insert_lead_stream_hits(
            docs,
            freqs,
            clause_scores,
            num_lists,
            candidates,
            num_comparisons,
            wand_factor,
        )?;
        Ok(LeadStreamBlockOutcome {
            follower_exhausted,
            lead_bm25,
            floor_skip,
        })
    }

    /// Compact followers onto lead ids first, then BM25 only the survivors.
    /// Must not Exclusive-prune with a zero lead score: mixed leftover windows
    /// have `others < T < others+lead` and would drop real hits.
    #[allow(clippy::too_many_arguments)]
    fn and_lead_stream_intersect_first_block(
        &mut self,
        lead_docs: &[u32],
        lead_freqs: &[u32],
        freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
        num_lists: usize,
        docs: &mut Vec<u32>,
        freqs: &mut Vec<u32>,
        clause_scores: &mut Vec<f32>,
        candidates: &mut TopKCollector,
        num_comparisons: &mut usize,
        wand_factor: f32,
        norm_k: Option<(&[u8], &[f32; 256])>,
        exact_addends: Option<ExactBm25Addends<'_>>,
    ) -> Result<bool> {
        #[cfg(test)]
        {
            self.lead_stream_intersect_first_blocks += 1;
        }
        docs.clear();
        freqs.clear();
        clause_scores.clear();

        for (&doc, &freq) in lead_docs.iter().zip(lead_freqs.iter()) {
            let freq_bucket = (freq as usize).min(FREQ_LUT_BUCKETS - 1);
            if freq_cannot_beat[freq_bucket] {
                continue;
            }
            docs.push(doc);
            let start = freqs.len();
            freqs.resize(start + num_lists, 0);
            freqs[start] = freq;
        }

        let mut follower_exhausted = false;
        for clause in 1..num_lists {
            if docs.is_empty() {
                return Ok(follower_exhausted);
            }
            let posting = &mut self.lead[clause];
            let mut write = 0usize;
            let mut exhausted = false;
            for read in 0..docs.len() {
                let target = u64::from(docs[read]);
                if posting.doc().is_none_or(|cur| cur.doc_id() < target) {
                    posting.next(target);
                }
                match posting.doc() {
                    None => {
                        exhausted = true;
                        break;
                    }
                    Some(cur) if cur.doc_id() > target => continue,
                    Some(cur) => {
                        docs[write] = docs[read];
                        let src = read * num_lists;
                        let dst = write * num_lists;
                        if src != dst {
                            freqs.copy_within(src..src + num_lists, dst);
                        }
                        freqs[dst + clause] = cur.frequency();
                        write += 1;
                    }
                }
            }
            docs.truncate(write);
            freqs.truncate(write * num_lists);
            follower_exhausted |= exhausted;
        }

        clause_scores.resize(docs.len() * num_lists, 0.0);
        for (i, &doc) in docs.iter().enumerate() {
            let base = i * num_lists;
            for clause in 0..num_lists {
                clause_scores[base + clause] = clause_bm25(
                    &self.lead[clause],
                    &self.scorer,
                    self.documents,
                    freqs[base + clause],
                    doc,
                    norm_k,
                    exact_addends,
                );
            }
        }

        self.insert_lead_stream_hits(
            docs,
            freqs,
            clause_scores,
            num_lists,
            candidates,
            num_comparisons,
            wand_factor,
        )?;
        Ok(follower_exhausted)
    }

    /// Lead-stream conjunction: the shortest list's decompressed block is the
    /// buffer, followers only `next(target)`. Scoring, phrase confirm, and heap
    /// semantics match the classic loop.
    pub(super) fn and_lead_stream_search(
        &mut self,
        params: &FtsSearchParams,
        metrics: &dyn MetricsCollector,
    ) -> Result<Vec<DocCandidate<D::Candidate>>> {
        let limit = params.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return Ok(vec![]);
        }
        let phrase_slop = params.phrase_slop;
        let num_lists = self.lead.len();
        let mut freq_bound_lut = [f32::INFINITY; FREQ_LUT_BUCKETS];
        for (freq, slot) in freq_bound_lut
            .iter_mut()
            .enumerate()
            .take(FREQ_LUT_BUCKETS - 1)
        {
            *slot = self.lead[0].score(&self.scorer, freq as u32, 0);
        }
        freq_bound_lut[FREQ_LUT_BUCKETS - 1] =
            self.lead[0].frequency_clamp_upper_bound(&self.scorer);

        let score_first = and_score_first_for(
            num_lists,
            self.lead[0].cost(),
            self.lead.get(1).map(|posting| posting.cost()).unwrap_or(0),
        );
        let heap_can_fill = limit != usize::MAX;
        // Parent Boolean / sibling partitions may already have published a
        // floor. Raise once so cache prep sees kernel reachability for the
        // first window; the loop raises again to pick up later updates.
        self.raise_to_shared_floor(params.wand_factor);
        let kernel_reachable =
            phrase_slop.is_none() && (score_first || heap_can_fill || self.threshold > 0.0);
        let documents = self.documents;
        let norm_k = if kernel_reachable {
            self.norm_k_cache()
        } else {
            None
        };
        let norm_k_ref = norm_k
            .as_ref()
            .map(|(norms, cache)| (*norms, cache.as_ref()));
        let exact_addends = if kernel_reachable && norm_k_ref.is_none() {
            exact_bm25_addend_slab(&self.scorer, documents)
        } else {
            None
        };

        let mut candidates = TopKCollector::new(limit, std::cmp::min(limit, BLOCK_SIZE * 10));
        let mut num_comparisons: usize = 0;
        let mut lead_docs: Vec<u32> = Vec::with_capacity(MAX_POSTING_BLOCK_SIZE);
        let mut lead_freqs: Vec<u32> = Vec::with_capacity(MAX_POSTING_BLOCK_SIZE);
        let mut score_docs: Vec<u32> = Vec::new();
        let mut score_partial: Vec<f32> = Vec::new();
        let mut score_freqs: Vec<u32> = Vec::new();
        let mut score_clause: Vec<f32> = Vec::new();
        if kernel_reachable {
            score_docs.reserve(MAX_POSTING_BLOCK_SIZE);
            score_partial.reserve(MAX_POSTING_BLOCK_SIZE);
            score_freqs.reserve(MAX_POSTING_BLOCK_SIZE.saturating_mul(num_lists));
            score_clause.reserve(MAX_POSTING_BLOCK_SIZE.saturating_mul(num_lists));
        }

        let mut target: u64 = 0;
        for posting in &self.lead {
            match posting.doc() {
                Some(doc) => target = target.max(doc.doc_id()),
                None => return Ok(vec![]),
            }
        }

        'window: loop {
            self.raise_to_shared_floor(params.wand_factor);
            self.lead[0].next(target);
            let Some(lead_doc) = self.lead[0].doc() else {
                break;
            };
            target = target.max(lead_doc.doc_id());
            let win_end = Self::posting_block_up_to(&self.lead[0], target);

            if self.threshold > 0.0 {
                for posting in &mut self.lead {
                    posting.shallow_next(target);
                }
                let wide_max = conservative_score_sum(self.lead.iter().map(|posting| {
                    posting
                        .block_max_score_up_to_with_stats(win_end, &self.scorer)
                        .score
                }));
                if wide_max < self.threshold {
                    if win_end == TERMINATED_DOC_ID {
                        break;
                    }
                    target = win_end + 1;
                    continue;
                }
            }

            let Some(first_index) = self.lead[0].peek_remaining_block_docs_upto(
                win_end,
                &mut lead_docs,
                &mut lead_freqs,
            ) else {
                if win_end == TERMINATED_DOC_ID {
                    break;
                }
                target = win_end + 1;
                continue;
            };

            let others_bounds: SmallVec<[f64; 8]> = self.lead[1..]
                .iter()
                .map(|posting| {
                    f64::from(
                        posting
                            .block_max_score_up_to_with_stats(win_end, &self.scorer)
                            .score,
                    )
                })
                .collect();
            let others_block_max = others_bounds.iter().copied().sum::<f64>();
            let rest_block_max = others_bounds.iter().skip(1).copied().sum::<f64>();
            let freq_cannot_beat = if self.threshold > 0.0 && num_lists >= 2 {
                std::array::from_fn(|frequency| {
                    score_sum_cannot_compete(
                        freq_bound_lut[frequency],
                        others_block_max,
                        self.threshold,
                        score_sum_upper_bound_factor(num_lists),
                        CompetitiveFloorMode::Exclusive,
                    )
                })
            } else {
                [false; FREQ_LUT_BUCKETS]
            };

            // Window-boundary switch only: the ratio still gates a zero floor,
            // a live floor admits the block kernel without consulting 3×.
            let use_block = phrase_slop.is_none() && (score_first || self.threshold > 0.0);
            if use_block {
                let mut offset = 0usize;
                let mut exhausted = false;
                if score_first {
                    while offset < lead_docs.len() {
                        let take = score_first_batch_len(
                            lead_docs.len() - offset,
                            self.threshold,
                            heap_can_fill,
                        );
                        let end = offset + take;
                        let outcome = self.and_lead_stream_score_first_block(
                            &lead_docs[offset..end],
                            &lead_freqs[offset..end],
                            &others_bounds,
                            others_block_max,
                            &freq_cannot_beat,
                            num_lists,
                            &mut score_docs,
                            &mut score_partial,
                            &mut score_freqs,
                            &mut score_clause,
                            &mut candidates,
                            &mut num_comparisons,
                            params.wand_factor,
                            norm_k_ref,
                            exact_addends,
                        )?;
                        if outcome.follower_exhausted {
                            exhausted = true;
                            break;
                        }
                        offset = end;
                    }
                } else {
                    let probe_end = offset.saturating_add(HEAP_FILL_BATCH).min(lead_docs.len());
                    let outcome = self.and_lead_stream_score_first_block(
                        &lead_docs[offset..probe_end],
                        &lead_freqs[offset..probe_end],
                        &others_bounds,
                        others_block_max,
                        &freq_cannot_beat,
                        num_lists,
                        &mut score_docs,
                        &mut score_partial,
                        &mut score_freqs,
                        &mut score_clause,
                        &mut candidates,
                        &mut num_comparisons,
                        params.wand_factor,
                        norm_k_ref,
                        exact_addends,
                    )?;
                    if outcome.follower_exhausted {
                        exhausted = true;
                    } else {
                        offset = probe_end;
                        let use_intersect = leftover_rest_uses_intersect_first(
                            outcome.lead_bm25,
                            outcome.floor_skip,
                        );
                        while offset < lead_docs.len() {
                            let end = lead_docs.len();
                            if use_intersect {
                                if self.and_lead_stream_intersect_first_block(
                                    &lead_docs[offset..end],
                                    &lead_freqs[offset..end],
                                    &freq_cannot_beat,
                                    num_lists,
                                    &mut score_docs,
                                    &mut score_freqs,
                                    &mut score_clause,
                                    &mut candidates,
                                    &mut num_comparisons,
                                    params.wand_factor,
                                    norm_k_ref,
                                    exact_addends,
                                )? {
                                    exhausted = true;
                                }
                            } else {
                                let rest = self.and_lead_stream_score_first_block(
                                    &lead_docs[offset..end],
                                    &lead_freqs[offset..end],
                                    &others_bounds,
                                    others_block_max,
                                    &freq_cannot_beat,
                                    num_lists,
                                    &mut score_docs,
                                    &mut score_partial,
                                    &mut score_freqs,
                                    &mut score_clause,
                                    &mut candidates,
                                    &mut num_comparisons,
                                    params.wand_factor,
                                    norm_k_ref,
                                    exact_addends,
                                )?;
                                if rest.follower_exhausted {
                                    exhausted = true;
                                }
                            }
                            offset = end;
                        }
                    }
                }
                if exhausted || win_end == TERMINATED_DOC_ID {
                    break;
                }
                // Lucene BlockMaxConjunctionBulkScorer: after the current
                // window, leap lead to max(windowEnd+1, max other current
                // doc). Dense windows: max other ≤ win_end, this is
                // win_end+1. The kernel still finishes this window first
                // (same as Lucene); mid-window leap is the doc-at-a-time
                // path above.
                target = win_end + 1;
                if let Some(other) = self
                    .lead
                    .iter()
                    .skip(1)
                    .filter_map(|posting| posting.current_doc_id())
                    .max()
                {
                    target = target.max(other);
                }
                #[cfg(test)]
                {
                    if target > win_end + 1 {
                        self.lead_stream_block_leaps += 1;
                    }
                }
                continue;
            }

            let mut pos = 0;
            while pos < lead_docs.len() {
                let doc = lead_docs[pos];
                let freq = lead_freqs[pos];
                let freq_bucket = (freq as usize).min(FREQ_LUT_BUCKETS - 1);
                if freq_cannot_beat[freq_bucket] {
                    pos += 1;
                    continue;
                }

                self.park_lead_at(first_index + pos, doc, freq);
                let Some(parked) = self.lead[0].doc() else {
                    pos += 1;
                    continue;
                };
                let Some(document_key) = self.documents.document_key(&parked) else {
                    pos += 1;
                    continue;
                };
                let doc_length = self.documents.doc_length(&parked);

                if score_first
                    && self.threshold > 0.0
                    && score_sum_cannot_compete(
                        self.lead[0].score(&self.scorer, freq, doc_length),
                        others_block_max,
                        self.threshold,
                        score_sum_upper_bound_factor(num_lists),
                        CompetitiveFloorMode::Exclusive,
                    )
                {
                    pos += 1;
                    continue;
                }

                let split_followers = num_lists >= 3 && self.threshold > 0.0;
                let first_followers = if split_followers { 2 } else { num_lists };
                match self.seek_lead_followers(1, first_followers, doc) {
                    LeadFollowerSeek::Exhausted => break 'window,
                    LeadFollowerSeek::Leap(next) => {
                        pos = skip_lead_docs(&lead_docs, pos, next);
                        if next > win_end {
                            target = next;
                            continue 'window;
                        }
                        continue;
                    }
                    LeadFollowerSeek::Match => {}
                }

                if self.matched_pair_cannot_beat_floor(freq, doc_length, rest_block_max, num_lists)
                {
                    pos += 1;
                    continue;
                }

                if split_followers {
                    match self.seek_lead_followers(2, num_lists, doc) {
                        LeadFollowerSeek::Exhausted => break 'window,
                        LeadFollowerSeek::Leap(next) => {
                            pos = skip_lead_docs(&lead_docs, pos, next);
                            if next > win_end {
                                target = next;
                                continue 'window;
                            }
                            continue;
                        }
                        LeadFollowerSeek::Match => {}
                    }
                }

                let score = self.score_in_query_order(doc_length);
                num_comparisons += 1;
                if let Some(slop) = phrase_slop {
                    if self.exclusive_score_cannot_beat_floor(score) {
                        pos += 1;
                        continue;
                    }
                    if !self.check_positions(slop as i32)? {
                        pos += 1;
                        continue;
                    }
                }

                if candidates.insert(
                    ScoredDoc::new(document_key, score),
                    doc_length,
                    u64::from(doc),
                    self.iter_term_freqs(),
                )? && let Some(kth) = candidates.kth_score_if_full()
                {
                    self.update_threshold(kth, params.wand_factor);
                }
                pos += 1;
            }

            if win_end == TERMINATED_DOC_ID {
                break;
            }
            target = win_end + 1;
        }

        metrics.record_comparisons(num_comparisons);
        candidates.into_candidates(|key| self.documents.candidate_from_key(key))
    }
}

fn skip_lead_docs(lead_docs: &[u32], pos: usize, next: u64) -> usize {
    let next32 = u32::try_from(next).unwrap_or(u32::MAX);
    pos + lead_docs[pos + 1..].partition_point(|&id| id < next32) + 1
}

#[cfg(test)]
mod tests {
    use super::{
        AND_INTERSECT_FIRST_MAX_PROBE_SKIP, AND_SKEW_RATIO, HEAP_FILL_BATCH, and_score_first_for,
        conjunction_lists_are_skewed, leftover_rest_uses_intersect_first, score_first_batch_len,
        skip_lead_docs,
    };

    #[test]
    fn leftover_intersect_first_switch_uses_probe_skip_rate() {
        assert!(!leftover_rest_uses_intersect_first(0, 0));
        assert!(!leftover_rest_uses_intersect_first(16, 4));
        assert!(leftover_rest_uses_intersect_first(16, 3));
        assert!(!leftover_rest_uses_intersect_first(
            16,
            (16.0 * AND_INTERSECT_FIRST_MAX_PROBE_SKIP) as u32
        ));
    }

    #[test]
    fn skew_gate_uses_integer_max_over_min() {
        assert!(!conjunction_lists_are_skewed(0, 1_000_000));
        assert!(!conjunction_lists_are_skewed(16, 16 * (AND_SKEW_RATIO - 1)));
        assert!(conjunction_lists_are_skewed(16, 16 * AND_SKEW_RATIO));
        assert!(conjunction_lists_are_skewed(1, AND_SKEW_RATIO));
    }

    #[test]
    fn score_first_needs_three_clauses_and_a_3x_second() {
        assert!(!and_score_first_for(2, 100, 10_000));
        assert!(!and_score_first_for(3, 100, 299));
        assert!(and_score_first_for(3, 100, 300));
        assert!(and_score_first_for(6, 10, 30));
    }

    #[test]
    fn skip_lead_docs_advances_to_the_first_id_at_or_after_next() {
        let docs = [10, 20, 30, 40];
        assert_eq!(skip_lead_docs(&docs, 0, 30), 2);
        assert_eq!(skip_lead_docs(&docs, 0, 25), 2);
        assert_eq!(skip_lead_docs(&docs, 1, 50), 4);
    }

    #[test]
    fn score_first_batch_len_fills_then_consumes_the_block() {
        assert_eq!(score_first_batch_len(128, 0.0, true), HEAP_FILL_BATCH);
        assert_eq!(score_first_batch_len(10, 0.0, true), 10);
        assert_eq!(score_first_batch_len(128, 1.0, true), 128);
        assert_eq!(score_first_batch_len(128, 0.0, false), 128);
        assert_eq!(score_first_batch_len(0, 0.0, true), 0);
    }
}
