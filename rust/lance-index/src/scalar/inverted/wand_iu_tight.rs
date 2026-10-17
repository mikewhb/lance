// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Boolean scoring for the `intersection_union` shape: K required term
//! clauses plus N optional ones (named after the Search Benchmark Game's
//! query of that shape).
//!
//! The IU batch profile: one walk that drives the K = 1 flat MUST/SHOULD shape
//! from the scorer tree's `BlockBatch` providers.
//!
//! [`run_single_walk`] takes one required block at a time, folds the SHOULDs
//! over it, and emits. K >= 2 is left to the per-document tree.
//!
//! The walk owns its loop for the whole shape: hits are handed straight to the
//! collector through `emit`, and the floor is re-read between blocks, so the
//! walk prunes with a value that is never older than the heap it prunes for.
//! Nothing has to be handed back mid-walk, and batch engagement stays one-way
//! per scorer.

use lance_core::Result;

use super::super::compound::{BlockBatch, CompetitiveScore};
use super::{
    BulkAndMode, CompetitiveFloorMode, TERMINATED_DOC_ID, conservative_score_sum,
    score_sum_cannot_compete, score_sum_upper_bound_factor,
};

/// Whether `partial + remaining` can no longer reach `floor`. Wraps the
/// generic path's predicate so the inclusive floor semantics live in one
/// audited place; a document scoring exactly at the floor still competes,
/// which is the contract the batch walks are tested against.
///
/// The **widening happens at the call sites**: every caller passes a
/// `conservative_score_sum`-widened `remaining` (the sum of the still-live
/// clause bounds), so this function itself only adds two operands and correctly
/// uses `score_sum_upper_bound_factor(2)` (== 1.0). Adding a raw `f32` onto an
/// already-widened bound is what would round down by an ULP; do not do that.
/// The consumer scores the same values by sequential `f32` accumulation, so
/// with `n >= 3` clauses the accumulation order alone can exceed the f64 sum by
/// `(n-1) * f32::EPSILON` relative error — which is why the caller's
/// `conservative_score_sum` matters, not this factor.
fn iu_cannot_compete(partial: f32, remaining: f32, floor: f32) -> bool {
    score_sum_cannot_compete(
        partial,
        f64::from(remaining),
        floor,
        score_sum_upper_bound_factor(2),
        CompetitiveFloorMode::Inclusive,
    )
}

/// Sum of the upper bounds of the cursors that have not run dry. The closure
/// is consulted only for live cursors, so a dry cursor's bound is neither
/// recomputed nor counted.
fn live_upper_bound(dry: &[bool], upper_bound: impl Fn(usize) -> f32) -> f32 {
    conservative_score_sum((0..dry.len()).filter(|&index| !dry[index]).map(upper_bound))
}

/// Env gate for the batch profile, following the `LANCE_FTS_BULK_AND`
/// convention: `auto` (default) and `on`/`1` both enable it, and `off`/`0`
/// routes every IU-shaped Boolean back to the per-document scorer tree. This
/// is the A/B and rollout switch; it does not change results, only which
/// engine computes them.
static IU_TIGHT_MODE: std::sync::LazyLock<BulkAndMode> = std::sync::LazyLock::new(|| {
    super::mode_from_env_value("LANCE_FTS_IU_TIGHT", std::env::var("LANCE_FTS_IU_TIGHT"))
});

/// Whether the IU fast paths may run in this process.
pub fn iu_tight_enabled() -> bool {
    *IU_TIGHT_MODE != BulkAndMode::Off
}

pub fn run_single_walk<'s>(
    must_ub: f32,
    should_ubs: &[f32],
    must: &'s mut dyn BlockBatch,
    shoulds: &'s mut [&'s mut dyn BlockBatch],
    floor: &CompetitiveScore,
    emit: &mut dyn FnMut(u64, f32) -> Result<bool>,
) -> Result<bool> {
    let mut dry = vec![false; should_ubs.len()];
    let mut chunk: Vec<(u64, u32)> = Vec::new();
    let mut candidates: Vec<u64> = Vec::new();
    let mut should_sums: Vec<f32> = Vec::new();
    let mut chunk_freqs: Vec<u32> = Vec::new();
    let mut must_scores: Vec<f32> = Vec::new();
    let mut keys: Vec<Option<u64>> = Vec::new();
    let mut chunk_idx = 0usize;
    let mut chunk_started = false;
    loop {
        // 1) Emit the candidates of the block already in the state's
        // chunk. All per-candidate tables were batch-computed at block
        // take — this loop does no provider dispatch at all.
        while chunk_idx < chunk.len() {
            let idx = chunk_idx;
            let (_doc, _freq) = chunk[idx];
            chunk_idx += 1;
            let Some(key) = keys[idx] else {
                continue;
            };
            let score = must_scores[idx] + should_sums[idx];
            if score >= floor.get() && !emit(key, score)? {
                return Ok(false);
            }
        }
        // 2) Block fully consumed: block-end checks (only after really
        // consuming a block). The floor is read here, at the block
        // boundary, so a hit emitted earlier in this call has already
        // raised the value these gates prune with.
        if chunk_started {
            let floor_now = floor.get();
            if floor_now.is_finite() {
                let live_optional_ub = live_upper_bound(&dry, |index| should_ubs[index]);
                if iu_cannot_compete(must_ub, live_optional_ub, floor_now) {
                    return Ok(true);
                }
            }
        }
        // 3) Take the next block and rebuild the merge state. All
        // per-candidate provider exits (must scores, visibility keys,
        // per-SHOULD scoring) collapse to one dispatch per block.
        must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut chunk);
        if chunk.is_empty() {
            return Ok(true);
        }
        candidates.clear();
        candidates.extend(chunk.iter().map(|(doc, _)| *doc));
        chunk_freqs.clear();
        chunk_freqs.extend(chunk.iter().map(|(_, f)| *f));
        // must_scores carries no independent zero semantics — every
        // entry is rewritten by term_scores_batch below — so a plain
        // resize (no clear) avoids a per-block memset.
        must_scores.resize(candidates.len(), 0.0);
        must.term_scores_batch(&candidates, &chunk_freqs, &mut must_scores);
        must.document_keys_batch(&candidates, &mut keys);
        // `should_sums` accumulates (`fold_optional_scores` adds into every
        // hit slot and *leaves misses untouched*), so unlike `must_scores` it
        // must be zeroed before each block: the resize above only grows the
        // vector, it does not reset the slots the fold will revisit.
        should_sums.clear();
        should_sums.resize(chunk.len(), 0.0);
        for (index, should) in shoulds.iter_mut().enumerate() {
            // One fused dispatch per SHOULD: merge + score + accumulate
            // in the provider, in the same accumulation order as the
            // separated passes.
            should.fold_optional_scores(&candidates, &mut should_sums);
            if should.current_doc_id().is_none() {
                dry[index] = true;
            }
        }
        chunk_idx = 0;
        chunk_started = true;
    }
}

// ===== Walk harness: path walkthrough (takeover / promotion latch /
// exhaustion). Drives the walks with mock providers to verify deliver-once
// ordering, visibility, the promotion sub-mode, and soundness under a floor
// that rises mid-walk.
