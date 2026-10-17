// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Boolean scoring for the `intersection_union` shape: K required term
//! clauses plus N optional ones (named after the Search Benchmark Game's
//! query of that shape).
//!
//! The IU batch profile: two walks that drive flat
//! MUST/SHOULD shapes from the scorer tree's `BlockBatch` providers.
//!
//! [`run_single_walk`] owns the K = 1 walk (plus the promoted sub-mode) and
//! [`run_multi_walk`] the K >= 2 seek-to-max walk; both consume block-wise
//! provider capabilities (`take_docs_one_block_upto`, `merge_optional_freqs`,
//! `seek_eq`, batch scoring) instead of driving the per-document protocol.
//!
//! The walk owns its loop for the whole shape: hits are handed straight to the
//! collector through `emit`, and the floor is re-read between blocks, so the
//! walk prunes with a value that is never older than the heap it prunes for.
//! Nothing has to be handed back mid-walk, and batch engagement stays one-way
//! per scorer.

use std::sync::LazyLock;

use lance_core::Result;

use super::super::compound::{BlockBatch, CompetitiveScore};
use super::super::encoding::MAX_POSTING_BLOCK_SIZE;
use super::{
    BulkAndMode, CompetitiveFloorMode, TERMINATED_DOC_ID, conservative_score_sum,
    score_sum_cannot_compete, score_sum_upper_bound_factor,
};

// ===== S0 block probe (zero behavior, env-gated) =====
//
// Counts what the batch walks actually do, so the block-merge decision rests on
// data instead of intuition. `LANCE_FTS_BLK_PROBE=1` prints one line per walk
// call; with the variable unset the counters are still maintained (a handful of
// integer adds per block) but nothing is printed.
//
// `blocks` = block takes the walk really performed.
// `est_blocks` = blocks a block-merge intersection would have to decompress:
// `sum(ceil(cost / MAX_POSTING_BLOCK_SIZE))` over every provider, using the raw
// df the trait exposes. If `seeks` is far below `est_blocks`, a merge would
// touch more blocks than the per-candidate seeks it replaces.
struct BlkProbe {
    shape: &'static str,
    blocks: usize,
    est_blocks: usize,
    candidates: usize,
    seeks: usize,
    emits: usize,
}

impl BlkProbe {
    fn new(shape: &'static str, est_blocks: usize) -> Self {
        Self {
            shape,
            blocks: 0,
            est_blocks,
            candidates: 0,
            seeks: 0,
            emits: 0,
        }
    }
}

impl Drop for BlkProbe {
    fn drop(&mut self) {
        if *BLK_PROBE {
            eprintln!(
                "[blkprobe] shape={} blocks={} est_blocks={} candidates={} seeks={} emits={}",
                self.shape, self.blocks, self.est_blocks, self.candidates, self.seeks, self.emits
            );
        }
    }
}

static BLK_PROBE: LazyLock<bool> =
    LazyLock::new(|| std::env::var("LANCE_FTS_BLK_PROBE").is_ok_and(|value| value.trim() == "1"));

fn est_block_count(costs: impl Iterator<Item = usize>) -> usize {
    costs
        .map(|cost| cost.div_ceil(MAX_POSTING_BLOCK_SIZE))
        .sum()
}

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

/// Promote only when the new lead is at least this many times cheaper than
/// MUST. A medium-df SHOULD that just barely becomes required is slower to
/// walk than finishing MUST.
pub const IU_TIGHT_LEAD_COST_RATIO: usize = 3;

pub fn iu_tight_lead_is_cheaper(must_cost: usize, lead_cost: usize) -> bool {
    lead_cost.saturating_mul(IU_TIGHT_LEAD_COST_RATIO) < must_cost
}

/// SHOULD indices that are each individually required once MUST-only scores
/// can no longer reach `floor`. `Some(empty)` means even MUST plus every
/// SHOULD clause cannot compete. `None` means keep walking MUST.
pub fn iu_tight_required_shoulds(
    must_ub: f32,
    should_ubs: &[f32],
    floor: f32,
) -> Option<Vec<usize>> {
    if !must_ub.is_finite() || must_ub >= floor {
        return None;
    }
    if should_ubs.iter().any(|upper| !upper.is_finite()) {
        return None;
    }
    let sum_shoulds = conservative_score_sum(should_ubs.iter().copied());
    if iu_cannot_compete(must_ub, sum_shoulds, floor) {
        return Some(Vec::new());
    }
    let required: Vec<usize> = should_ubs
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let others = conservative_score_sum(
                should_ubs
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| other != index)
                    .map(|(_, upper)| *upper),
            );
            iu_cannot_compete(must_ub, others, floor)
        })
        .map(|(index, _)| index)
        .collect();
    if required.is_empty() {
        None
    } else {
        Some(required)
    }
}

#[cfg(test)]
mod tests {
    use super::super::maxscore::exclusive_cannot_compete;
    use super::super::outward_f32_upper_bound;
    use super::{iu_cannot_compete, iu_tight_lead_is_cheaper, iu_tight_required_shoulds};

    #[test]
    fn iu_and_exclusive_predicates_agree_except_at_the_floor() {
        // The IU walk and the generic WAND path share one kernel
        // (`score_sum_cannot_compete`) but wrap it with opposite floor modes: IU
        // keeps a document tying the floor (`Inclusive`), the generic path
        // rejects it (`Exclusive`). Pin that relationship so the two predicates
        // cannot silently drift — a drift would make the two engines disagree
        // about a single candidate. Factor 1.0 is `score_sum_upper_bound_factor(2)`;
        // `outward_f32_upper_bound` is the shared upper bound both compare on.
        for &p in &[0.0_f32, 1.0, 2.5, 3000.0, 16_777_216.0] {
            for &r in &[0.0_f32, 1.0, 3.0, 1500.0, 16_777_216.0] {
                for &f in &[-1.0_f32, 0.0, 1.0, 3000.0, 3000.0005, 1.0e7] {
                    let upper = outward_f32_upper_bound(f64::from(p) + f64::from(r));
                    let iu = iu_cannot_compete(p, r, f);
                    let ex = exclusive_cannot_compete(p, f64::from(r), f, 1.0);
                    if f > 0.0 {
                        assert!(
                            iu || !ex || upper == f,
                            "exclusive rejected a non-tie IU keeps: p={p} r={r} f={f}"
                        );
                        assert!(
                            !iu || ex,
                            "IU rejected where exclusive would not: p={p} r={r} f={f}"
                        );
                    } else {
                        assert!(
                            !iu && !ex,
                            "a non-positive floor must reject nothing: p={p} r={r} f={f}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn required_shoulds_stays_on_must_while_must_alone_can_compete() {
        assert_eq!(iu_tight_required_shoulds(5.0, &[10.0], 5.0), None);
        assert_eq!(iu_tight_required_shoulds(5.0, &[10.0], 4.0), None);
    }

    #[test]
    fn required_shoulds_promotes_a_should_that_is_individually_necessary() {
        assert_eq!(iu_tight_required_shoulds(1.0, &[10.0], 11.0), Some(vec![0]));
        assert_eq!(
            iu_tight_required_shoulds(1.0, &[10.0, 0.5], 11.0),
            Some(vec![0])
        );
    }

    #[test]
    fn required_shoulds_exits_when_the_combined_bound_cannot_compete() {
        assert_eq!(
            iu_tight_required_shoulds(1.0, &[2.0, 3.0], 10.0),
            Some(Vec::new())
        );
    }

    #[test]
    fn required_shoulds_does_not_promote_when_no_single_should_is_necessary() {
        // MUST + either SHOULD still reaches the floor, so neither list is
        // individually required.
        assert_eq!(iu_tight_required_shoulds(1.0, &[10.0, 10.0], 11.0), None);
    }

    #[test]
    fn required_shoulds_does_not_empty_on_f32_rounding_boundary() {
        // Naive f32 sum rounds 1.0 + 2^-24 down to 1.0, which is below
        // floor = 1 + 2^-23 and would terminate the MUST walk early.
        let must_ub = 2.0_f32.powi(-24);
        let should_ubs = [1.0, 2.0_f32.powi(-24)];
        let floor = 1.0 + 2.0_f32.powi(-23);
        assert_ne!(
            iu_tight_required_shoulds(must_ub, &should_ubs, floor),
            Some(Vec::new()),
            "conservative bounds must not declare the combined bound uncompetitive"
        );
    }

    #[test]
    fn lead_cost_ratio_is_relative_not_an_absolute_df_constant() {
        assert!(iu_tight_lead_is_cheaper(90_000, 10_000));
        assert!(!iu_tight_lead_is_cheaper(20_000, 10_000));
        assert!(!iu_tight_lead_is_cheaper(30_000, 10_000));
        assert!(iu_tight_lead_is_cheaper(31_000, 10_000));
        // Close costs must not hand the promoted walk to the optional list:
        // airport ~56k vs security ~77k is exactly the shape where walking the
        // SHOULD would be slower, so the switch stays relative, not absolute.
        assert!(!iu_tight_lead_is_cheaper(56_000, 77_000));
        assert!(!iu_tight_lead_is_cheaper(56_000, 56_000));
    }
}

// ===== The K = 1 and K >= 2 walks =====
//
// Floor semantics: both walks read the collector's shared floor live — at every
// block end (K = 1) and at every candidate (K >= 2). A rise produced by an
// emitted hit, or by another partition, is therefore honoured inside the same
// call; there is no refresh boundary to yield for. `emit` returning false
// (the collector's heap overflowed) ends the walk.
/// Promoted sub-mode: drive the cheaper lead clause block-wise and seek every
/// required clause per candidate.
struct PromotedWalk {
    lead_index: usize,
    required: Vec<usize>,
    start_doc: u64,
    lead_chunk: Vec<(u64, u32)>,
    lead_idx: usize,
}

impl PromotedWalk {
    fn new(lead_index: usize, required: Vec<usize>, start_doc: u64) -> Self {
        Self {
            lead_index,
            required,
            start_doc,
            lead_chunk: Vec::new(),
            lead_idx: 0,
        }
    }

    /// Runs to exhaustion. Returns `true` = exhausted, `false` = `emit` asked
    /// to stop because the collector can take no more.
    fn run<'s>(
        &mut self,
        must: &'s mut dyn BlockBatch,
        shoulds: &'s mut [&'s mut dyn BlockBatch],
        floor: &CompetitiveScore,
        emit: &mut dyn FnMut(u64, f32) -> Result<bool>,
    ) -> Result<bool> {
        loop {
            while self.lead_idx < self.lead_chunk.len() {
                let (doc, lead_freq) = self.lead_chunk[self.lead_idx];
                self.lead_idx += 1;
                if doc < self.start_doc {
                    continue;
                }
                // Forward seeks only: every cursor sits at or past the start
                // of its promoted region; a miss skips the candidate.
                let Some(must_freq) = must.seek_eq(doc) else {
                    continue;
                };
                let mut all_required = true;
                for &index in &self.required {
                    if index == self.lead_index {
                        continue;
                    }
                    if shoulds[index].seek_eq(doc).is_none() {
                        all_required = false;
                        break;
                    }
                }
                if !all_required {
                    continue;
                }
                let Some(key) = must.document_key_for_doc_id(doc as u32) else {
                    continue;
                };
                // Generic accumulation contract: optional contributions
                // accumulate from zero and the MUST term is added once at the
                // end. Folding MUST first (as this walk used to) changes the
                // f32 association and can drift by one ULP from the generic
                // ReqOpt scorer and the non-promoted batch path.
                let mut optional_sum = 0.0f32;
                for (index, should) in shoulds.iter_mut().enumerate() {
                    let freq = if index == self.lead_index {
                        Some(lead_freq)
                    } else {
                        should.seek_eq(doc)
                    };
                    if let Some(freq) = freq {
                        optional_sum += should.term_score(doc, freq);
                    }
                }
                let score = must.term_score(doc, must_freq) + optional_sum;
                if score >= floor.get() && !emit(key, score)? {
                    return Ok(false);
                }
            }
            shoulds[self.lead_index]
                .take_docs_one_block_upto(TERMINATED_DOC_ID, &mut self.lead_chunk);
            if self.lead_chunk.is_empty() {
                return Ok(true);
            }
            self.lead_idx = 0;
        }
    }
}

/// Runs the K = 1 IU walk to exhaustion: the single required provider drives
/// block takes, every SHOULD folds its scores over the block's candidates, and
/// each candidate at or above the live floor is handed to `emit`.
///
/// The walk owns its loop for the whole shape, so all of its state is local to
/// this call, and `floor` is the collector's own shared value: a rise produced
/// by an emitted hit is honoured at the very next block end.
///
/// Return value = "the candidate domain is exhausted": `true` when the
/// providers run out, `false` when `emit` returned `false` (the collector's
/// heap overflowed, so the caller retries the partition with resolved keys).
/// `run_multi_walk` returns `true` on that same stall — a known asymmetry that
/// is currently unobservable, since `ReqOptScorer::collect_top_k` is the only
/// consumer and an overflow retries the partition regardless.
pub fn run_single_walk<'s>(
    must_ub: f32,
    should_ubs: &[f32],
    must: &'s mut dyn BlockBatch,
    shoulds: &'s mut [&'s mut dyn BlockBatch],
    floor: &CompetitiveScore,
    emit: &mut dyn FnMut(u64, f32) -> Result<bool>,
) -> Result<bool> {
    let mut dry = vec![false; should_ubs.len()];
    let mut promotion_decision: Option<(f32, Option<Vec<usize>>)> = None;
    let mut chunk: Vec<(u64, u32)> = Vec::new();
    let mut candidates: Vec<u64> = Vec::new();
    let mut should_sums: Vec<f32> = Vec::new();
    let mut chunk_freqs: Vec<u32> = Vec::new();
    let mut must_scores: Vec<f32> = Vec::new();
    let mut keys: Vec<Option<u64>> = Vec::new();
    let mut chunk_idx = 0usize;
    let mut last_doc = 0u64;
    let mut chunk_started = false;
    let mut probe = BlkProbe::new(
        "single",
        est_block_count(
            std::iter::once(must.cost()).chain(shoulds.iter().map(|should| should.cost())),
        ),
    );
    loop {
        // 1) Emit the candidates of the block already in the state's
        // chunk. All per-candidate tables were batch-computed at block
        // take — this loop does no provider dispatch at all.
        while chunk_idx < chunk.len() {
            let idx = chunk_idx;
            let (doc, _freq) = chunk[idx];
            chunk_idx += 1;
            last_doc = doc;
            let Some(key) = keys[idx] else {
                continue;
            };
            let score = must_scores[idx] + should_sums[idx];
            if score >= floor.get() {
                probe.emits += 1;
                if !emit(key, score)? {
                    return Ok(false);
                }
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
                if promotion_decision
                    .as_ref()
                    .is_none_or(|(decided_floor, _)| *decided_floor != floor_now)
                {
                    promotion_decision = Some((
                        floor_now,
                        iu_tight_required_shoulds(must_ub, should_ubs, floor_now),
                    ));
                }
                if let Some(Some(required)) =
                    promotion_decision.as_ref().map(|(_, decision)| decision)
                    && !required.is_empty()
                {
                    let lead_cost = required
                        .iter()
                        .map(|&index| shoulds[index].cost())
                        .min()
                        .unwrap_or(usize::MAX);
                    if iu_tight_lead_is_cheaper(must.cost(), lead_cost) {
                        let lead = *required
                            .iter()
                            .min_by_key(|&&index| shoulds[index].cost())
                            .expect("checked non-empty");
                        let mut walk =
                            PromotedWalk::new(lead, required.clone(), last_doc.saturating_add(1));
                        if walk.run(must, shoulds, floor, emit)? {
                            return Ok(true);
                        }
                        return Ok(false);
                    }
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
        probe.blocks += 1;
        probe.candidates += chunk.len();
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

#[cfg(test)]
mod batch_proto_tests {
    use std::collections::HashSet;

    use super::{
        BlockBatch, CompetitiveScore, PromotedWalk, conservative_score_sum, run_multi_walk,
        run_single_walk,
    };

    /// Mock provider: in-memory posting (term_score = freq as f32), simulating
    /// block decoding `block` docs at a time.
    struct MockBatch {
        docs: Vec<(u64, u32)>,
        pos: usize,
        ub: f32,
        block: usize,
        /// Block takes, so a test can bound the scanned work.
        takes: usize,
        /// Per-candidate seeks, so a test can bound the scanned candidates.
        seeks: usize,
        hidden: HashSet<u32>,
    }

    impl MockBatch {
        fn new(docs: Vec<(u64, u32)>, ub: f32, block: usize) -> Self {
            Self {
                docs,
                pos: 0,
                ub,
                block,
                takes: 0,
                seeks: 0,
                hidden: HashSet::new(),
            }
        }
    }

    impl BlockBatch for MockBatch {
        fn document_key_for_doc_id(&self, doc_id: u32) -> Option<u64> {
            (!self.hidden.contains(&doc_id)).then_some(doc_id as u64)
        }

        fn take_docs_one_block_upto(&mut self, _upto: u64, out: &mut Vec<(u64, u32)>) {
            out.clear();
            self.takes += 1;
            let end = (self.pos + self.block).min(self.docs.len());
            out.extend_from_slice(&self.docs[self.pos..end]);
            self.pos = end;
        }

        fn merge_optional_freqs(&mut self, docs: &[u64], out_freqs: &mut [u32]) {
            out_freqs.fill(0);
            for (index, &doc) in docs.iter().enumerate() {
                if let Ok(hit) = self.docs.binary_search_by_key(&doc, |&(d, _)| d) {
                    out_freqs[index] = self.docs[hit].1;
                }
            }
        }

        fn current_doc_id(&self) -> Option<u64> {
            self.docs.get(self.pos).map(|&(d, _)| d)
        }

        fn seek_eq(&mut self, doc: u64) -> Option<u32> {
            self.seeks += 1;
            // Mirror the real provider contract: on a hit the cursor parks ON
            // the found document (current_doc_id == doc), so the alignment
            // tail's current_doc_id max never overshoots the candidate.
            if let Ok(hit) = self.docs.binary_search_by_key(&doc, |&(d, _)| d) {
                self.pos = hit;
                return Some(self.docs[hit].1);
            }
            None
        }

        fn global_upper_bound(&self) -> f32 {
            self.ub
        }

        fn cost(&self) -> usize {
            self.docs.len()
        }

        fn term_score(&self, _doc: u64, freq: u32) -> f32 {
            freq as f32
        }
    }

    /// Counterexample fixture: MUST docs 1..=20 each freq 1;
    /// SHOULD0 = {3,17} freq 10; SHOULD1 = {7} freq 10. Score = 1 + 10×hits.
    fn fixtures() -> (MockBatch, Vec<MockBatch>, Vec<(u64, f32)>) {
        let must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let should0 = MockBatch::new(vec![(3, 10), (17, 10)], 10.0, 4);
        let should1 = MockBatch::new(vec![(7, 10)], 10.0, 4);
        // Enumeration oracle (all visible): doc d scores 1 + 10×(SHOULD hits).
        let oracle: Vec<(u64, f32)> = (1..=20)
            .map(|d| {
                let s = [vec![3, 17], vec![7]]
                    .iter()
                    .filter(|docs| docs.contains(&d))
                    .count() as f32;
                (d as u64, 1.0 + 10.0 * s)
            })
            .collect();
        (must, vec![should0, should1], oracle)
    }

    /// A shared floor raised to `value`, mirroring what the collector exposes.
    /// The walk reads it live, so a rise is honoured at the next block end
    /// (K = 1) or the next candidate (K >= 2).
    fn competitive_score_at(value: f32) -> CompetitiveScore {
        let score = CompetitiveScore::default();
        score.raise(value);
        score
    }

    /// One-shot drive of the K = 1 walk, the way the collector drives it now:
    /// one call owns the loop, so delivery is complete when it returns.
    fn run_once(
        must_ub: f32,
        should_ubs: &[f32],
        must: &mut MockBatch,
        shoulds: &mut [MockBatch],
        floor: f32,
    ) -> (bool, Vec<(u64, f32)>) {
        let floor = competitive_score_at(floor);
        let mut providers: Vec<&mut dyn BlockBatch> = shoulds
            .iter_mut()
            .map(|should| should as &mut dyn BlockBatch)
            .collect();
        let mut hits = Vec::new();
        let exhausted = run_single_walk(
            must_ub,
            should_ubs,
            must,
            providers.as_mut_slice(),
            &floor,
            &mut |key, score| {
                hits.push((key, score));
                Ok(true)
            },
        )
        .expect("the K = 1 walk must not fail");
        (exhausted, hits)
    }

    fn run_all(
        must_ub: f32,
        should_ubs: &[f32],
        must: &mut MockBatch,
        shoulds: &mut [MockBatch],
        floor: f32,
    ) -> Vec<(u64, f32)> {
        let (exhausted, hits) = run_once(must_ub, should_ubs, must, shoulds, floor);
        assert!(exhausted, "a one-shot drive runs to exhaustion");
        hits
    }

    #[test]
    fn batch_one_call_delivers_every_above_floor_hit_once_in_order() {
        // Floor 11 ties the SHOULD hits (score 11) and sits above must_ub (1).
        // Keeping those ties is the inclusive-floor pin; delivering each once,
        // in order, is the one-call contract. A walk that stopped when the
        // floor passed must_ub would drop them.
        let (mut must, mut shoulds, oracle) = fixtures();
        let floor = 11.0;
        let out = run_all(1.0, &vec![10.0, 10.0], &mut must, &mut shoulds, floor);
        let expected: Vec<(u64, f32)> = oracle
            .iter()
            .copied()
            .filter(|(_, score)| *score >= floor)
            .collect();
        assert_eq!(
            out, expected,
            "one call must deliver every above-floor hit exactly once, including ties"
        );
        assert!(
            out.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "delivery stays in ascending doc order"
        );
    }

    #[test]
    fn batch_exhausts_and_respects_visibility() {
        let hidden: HashSet<u32> = HashSet::from([3, 17]);
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        must.hidden = hidden.clone();
        let mut shoulds = [MockBatch::new(vec![(3, 10), (17, 10)], 10.0, 4)];
        shoulds[0].hidden = hidden;
        let (exhausted, out) = run_once(1.0, &vec![10.0], &mut must, &mut shoulds, 5.0);
        assert!(exhausted);
        assert!(
            out.is_empty(),
            "the only hits (score 11) are invisible; nothing else clears floor=5.0"
        );
    }

    #[test]
    fn batch_promoted_walk_drives_the_cheaper_sparse_lead() {
        // Promoted sub-mode: once MUST-only (1.0) cannot reach floor (1.5) and
        // the sparse SHOULD is individually necessary, the walk drives the lead
        // (cost 3 < MUST 20) and seeks MUST per candidate. MUST-only docs never
        // clear the floor, so only the lead hits come out. One call owns the
        // loop and delivers every hit its lead walk produces (`run_all` asserts
        // the drive runs to exhaustion); the promoted decision is terminal.
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut shoulds = vec![MockBatch::new(vec![(3, 1), (17, 1), (25, 1)], 1.0, 2)];
        let out = run_all(1.0, &vec![1.0], &mut must, &mut shoulds, 1.5);
        assert_eq!(
            out,
            vec![(3, 2.0), (17, 2.0)],
            "doc 25 misses MUST and is excluded by the intersection"
        );
    }

    #[test]
    fn batch_promoted_scores_required_and_optional_clauses() {
        // Promoted walk with one necessary and one unnecessary clause: the
        // necessary clause is seek-checked first and must still be scored
        // (its frequency is reused, not re-sought), and the unnecessary
        // clause contributes in query order. Oracle = generic association
        // must + (s0 + s1).
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut shoulds = vec![
            MockBatch::new(vec![(3, 2)], 2.0, 4), // necessary (single clause cannot clear floor)
            MockBatch::new(vec![(3, 1)], 1.0, 4), // unnecessary but scored when present
        ];
        let out = run_all(1.0, &vec![2.0, 1.0], &mut must, &mut shoulds, 2.5);
        assert_eq!(
            out,
            vec![(3, 4.0)],
            "must(1) + (s0(2) + s1(1)) = 4.0; the lead clause's frequency must not be re-sought"
        );
    }

    #[test]
    fn batch_all_hidden_blocks_then_far_visible_hit() {
        // EXT scenario 7: every candidate of the MUST walk inside several
        // blocks is invisible; a valid optional hit sits far beyond them.
        // The walk must skip all hidden candidates (advancing last_doc),
        // keep the optional probes alive, and still deliver the far hit.
        let mut must = MockBatch::new((1..=200).map(|d| (d, 1)).collect(), 1.0, 32);
        must.hidden = (1..=199).collect();
        let mut shoulds = [MockBatch::new(vec![(200, 10)], 10.0, 32)];
        let (exhausted, out) = run_once(1.0, &vec![10.0], &mut must, &mut shoulds, 5.0);
        assert!(exhausted);
        assert_eq!(out, vec![(200, 11.0)], "the far visible hit must survive");
    }

    #[test]
    fn promoted_walk_adds_the_must_term_last() {
        // Regression (arithmetic isolated): the promoted walk must
        // accumulate optionals from zero and add MUST once. At 2^24 the two
        // associations differ by an ULP: optionals-first yields 2^24 + 2
        // (= floor, emitted); MUST-first rounds the 2^24 + 1 intermediate to
        // 2^24, so the score stays below the floor and the hit is lost.
        let mut must = MockBatch::new(vec![(1, 16_777_216)], 16_777_216.0, 4);
        let mut shoulds = [
            MockBatch::new(vec![(1, 1)], 1.0, 4),
            MockBatch::new(vec![(1, 1)], 1.0, 4),
        ];
        let mut walk = PromotedWalk::new(0, vec![0, 1], 0);
        let mut providers: Vec<&mut dyn BlockBatch> = shoulds
            .iter_mut()
            .map(|should| should as &mut dyn BlockBatch)
            .collect();
        let floor = competitive_score_at(16_777_218.0);
        let mut out = Vec::new();
        let exhausted = walk
            .run(
                &mut must,
                providers.as_mut_slice(),
                &floor,
                &mut |key, score| {
                    out.push((key, score));
                    Ok(true)
                },
            )
            .expect("the promoted walk must not fail");
        assert!(exhausted);
        assert_eq!(
            out,
            vec![(1, 16_777_218.0)],
            "optionals accumulate from zero, then MUST is added once"
        );
        assert_eq!(out[0].1.to_bits(), (16_777_216.0_f32 + 2.0).to_bits());
    }

    #[test]
    fn batch_promoted_trailing_lead_is_deterministic() {
        // The promoted lead is not the first optional, two optionals are
        // necessary, and the lead hits span blocks: two independent one-shot
        // runs must agree exactly, with the optionals still scored in query
        // order.
        let make = || {
            let must = MockBatch::new((1..=40).map(|d| (d, 1)).collect(), 1.0, 4);
            let shoulds = vec![
                // Not necessary: dropping it still leaves the other two ubs
                // above the floor.
                MockBatch::new(vec![(5, 1)], 1.0, 4),
                // Necessary and cheapest -> the promoted lead.
                MockBatch::new(vec![(3, 6), (9, 6), (15, 6)], 6.0, 4),
                // Necessary.
                MockBatch::new(vec![(3, 6), (9, 6), (15, 6)], 6.0, 4),
            ];
            (must, shoulds)
        };
        let floor = 13.0;

        let (mut must, mut shoulds) = make();
        let one_shot = run_all(1.0, &vec![1.0, 6.0, 6.0], &mut must, &mut shoulds, floor);

        let (mut must, mut shoulds) = make();
        let second = run_all(1.0, &vec![1.0, 6.0, 6.0], &mut must, &mut shoulds, floor);

        assert_eq!(second, one_shot, "the promoted walk is deterministic");
        for (left, right) in second.iter().zip(one_shot.iter()) {
            assert_eq!(
                left.1.to_bits(),
                right.1.to_bits(),
                "score bits must be stable"
            );
        }
        assert!(
            !one_shot.is_empty(),
            "the promoted walk (lead index 1) must have delivered the necessary hits"
        );
    }

    #[test]
    fn batch_honours_a_floor_rise_from_its_own_emits() {
        // Both sparse optionals hit early and the MUST bound is loose
        // (10 >= the real 1.0), so nothing but the raised floor can prove the
        // 60-block tail dead. The collector raises the floor on insert; the
        // walk reads that same shared value at the next block end, so the tail
        // is cut without any round trip to the driver.
        let mut must = MockBatch::new((1..=240).map(|d| (d, 1)).collect(), 10.0, 4);
        let mut shoulds = [
            MockBatch::new(vec![(3, 10)], 10.0, 4),
            MockBatch::new(vec![(3, 10)], 10.0, 4),
        ];
        let floor = competitive_score_at(1.5);
        let mut out = Vec::new();
        let exhausted = {
            let mut providers: Vec<&mut dyn BlockBatch> = shoulds
                .iter_mut()
                .map(|should| should as &mut dyn BlockBatch)
                .collect();
            run_single_walk(
                10.0,
                &[10.0, 10.0],
                &mut must,
                providers.as_mut_slice(),
                &floor,
                &mut |key, score| {
                    out.push((key, score));
                    floor.raise(score + 0.5); // what the collector does on insert
                    Ok(true)
                },
            )
            .expect("the K = 1 walk must not fail")
        };
        assert!(exhausted);
        assert_eq!(out, vec![(3, 21.0)], "the far tail must not produce hits");
        assert!(
            must.takes <= 3,
            "the walk must stop soon after the floor rises (took {} blocks)",
            must.takes
        );
    }

    #[test]
    fn multi_honours_a_floor_rise_from_its_own_emits() {
        // The multi walk reads the shared floor per candidate, so a rise the
        // collector performs on insert prunes the remaining sub-floor tail at
        // the very next candidate — no round trip to the driver is needed for
        // the walk to see the fresh value.
        let mut must0 = MockBatch::new((1..=400).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut must1 = MockBatch::new((1..=400).map(|d| (d, 1)).collect(), 1.0, 4);
        // The optional's only hit sits far beyond the required candidates, so
        // it never contributes and never runs dry.
        let mut optionals = [MockBatch::new(vec![(10_000, 1)], 1.0, 4)];
        // Loose required bound (sum 4.0 >= the real 2.0) so the pre-rise floor
        // can prove nothing; the raised floor (6.0 > 4.0 + 1.0) can.
        let floor = competitive_score_at(1.5);
        let mut hits = Vec::new();
        let exhausted = {
            let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
            let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
            run_multi_walk(
                &[2.0, 2.0],
                &[1.0],
                &mut requireds,
                &mut optionals_ref,
                &floor,
                &mut |key, score| {
                    hits.push((key, score));
                    floor.raise(6.0); // what the collector does on insert
                    Ok(true)
                },
            )
            .expect("the multi walk must not fail")
        };
        assert_eq!(hits, vec![(1, 2.0)], "every candidate scores 2.0");
        assert!(
            exhausted,
            "the raised floor proves the rest of the intersection dead"
        );
        let scanned = must0.seeks + must1.seeks;
        assert!(
            scanned < 20,
            "the raised floor must stop the tail at the next candidate (scanned {scanned})"
        );
    }

    #[test]
    fn batch_multi_seek_crosses_u32_domain_boundary() {
        // The multi walk aligns candidates near the u32 doc-id domain edge:
        // a sparse required hit at u32::MAX must survive alignment and
        // deliver, and the walk must exhaust cleanly past the domain.
        let top = u32::MAX as u64;
        let mut must0 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut must1 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut optionals = [MockBatch::new(vec![(top - 5, 4)], 4.0, 4)];
        let floor = competitive_score_at(3.0);
        let mut out = Vec::new();
        let exhausted = {
            let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
            let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
            run_multi_walk(
                &[1.0, 1.0],
                &[4.0],
                &mut requireds,
                &mut optionals_ref,
                &floor,
                &mut |key, score| {
                    out.push((key, score));
                    Ok(true)
                },
            )
            .expect("the multi walk must not fail")
        };
        assert!(exhausted, "the walk exhausts past the domain edge");
        assert_eq!(out, vec![(top - 5, 6.0)]);
    }

    #[test]
    fn multi_n1_promotion_recomputes_the_bound_from_raw_clause_bounds() {
        // Regression (M12): when the N == 1 promotion fires, the projected
        // required bound must be recomputed from the *raw* per-clause bounds
        // with `conservative_score_sum`, never by folding a raw f32 addend onto
        // the already-widened bound.
        //
        // The two forms agree on ordinary values, so the fixture must sit where
        // they cannot. Three clause bounds sum to 4500 in f64; the factor for
        // three values pushes `conservative_score_sum` a few ULP above 4500 (to
        // 4500.00244…), while a bare `+=` lands on exactly 4500. Put the floor
        // at the next f32 above 4500 (4500.000488…): the rounded-down bound
        // (4500) is strictly below it, and the inclusive rule rejects
        // strictly-below bounds, so it proves the rest dead and returns; the
        // sound bound sits above the floor, so it keeps scanning. Emission
        // cannot tell the two apart (any candidate's score is bounded by the old
        // bound, hence below the floor), so scanned work is the observable that
        // separates them.
        let next_above = |value: f32| f32::from_bits(value.to_bits() + 1);
        let mut must0 = MockBatch::new(vec![(1, 1500), (2, 1500)], 1500.0, 4);
        let mut must1 = MockBatch::new(vec![(1, 1500), (2, 1500)], 1500.0, 4);
        let mut optionals = [MockBatch::new(vec![(1, 1500), (2, 1500)], 1500.0, 4)];
        let floor = competitive_score_at(next_above(4500.0));
        let mut out = Vec::new();
        let exhausted = {
            let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
            let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
            run_multi_walk(
                &[1500.0, 1500.0],
                &[1500.0],
                &mut requireds,
                &mut optionals_ref,
                &floor,
                &mut |key, score| {
                    out.push((key, score));
                    Ok(true)
                },
            )
            .expect("the multi walk must not fail")
        };
        assert!(exhausted);
        assert!(
            out.is_empty(),
            "no candidate can reach a floor above the old bound"
        );
        // The promotion fires at the first aligned candidate (3000 < floor) and
        // re-evaluates it; the sound bound does not prove the rest dead, so the
        // walk keeps scanning (4 required seeks: candidates 0..3). The bare
        // `+=` bound is strictly below the floor, cuts at the re-evaluation, and
        // stops at 2.
        assert!(
            must0.seeks >= 4,
            "a sound promoted bound must not cut the walk after the promotion (seeks {})",
            must0.seeks
        );
        // Pin the arithmetic that makes the fixture discriminating (raw folds
        // in f64 with a widening factor > 1; a bare `+=` rounds to exactly
        // 4500), so the case cannot silently go vacuous.
        let recomputed = conservative_score_sum([1500.0_f32, 1500.0, 1500.0].into_iter());
        let folded = 3000.0_f32 + 1500.0;
        assert!(
            recomputed > folded && folded < next_above(4500.0) && recomputed >= next_above(4500.0),
            "fixture must straddle a floor between a bare += and the recomputed bound: \
             {recomputed} vs {folded}"
        );
    }
}

/// Runs the K >= 2 IU walk to exhaustion: the required providers align by
/// seek-to-max, the optionals are probed per candidate, and each candidate at
/// or above the live floor is handed to `emit`.
///
/// Same shape as [`run_single_walk`]: the loop lives here for the whole call,
/// the state lives in locals, and the floor is read per candidate, so a rise is
/// honoured immediately. The N == 1 promotion latch is a local too — the walk
/// cannot be re-entered, so it needs no cross-call representation.
///
/// Return value: like [`run_single_walk`] it is `true` when the required
/// intersection is exhausted, but unlike it this walk returns `true` on an
/// `emit` stall too (see the note on `run_single_walk`; the asymmetry is
/// currently unobservable).
pub fn run_multi_walk<'s>(
    required_ubs: &[f32],
    optional_ubs: &[f32],
    requireds: &'s mut Vec<&'s mut dyn BlockBatch>,
    optionals: &'s mut Vec<&'s mut dyn BlockBatch>,
    floor: &CompetitiveScore,
    emit: &mut dyn FnMut(u64, f32) -> Result<bool>,
) -> Result<bool> {
    let mut aligned = vec![false; required_ubs.len()];
    let mut freqs = vec![0u32; required_ubs.len()];
    let mut required_ub = conservative_score_sum(required_ubs.iter().copied());
    let mut optional_ub = conservative_score_sum(optional_ubs.iter().copied());
    let mut dry = vec![false; optional_ubs.len()];
    let mut promoted_optional = false;
    let mut promoted_freq = 0u32;
    let mut candidate = u64::MIN;
    let mut probe = BlkProbe::new(
        "multi",
        est_block_count(
            requireds
                .iter()
                .map(|posting| posting.cost())
                .chain(optionals.iter().map(|posting| posting.cost())),
        ),
    );
    debug_assert!(requireds.len() >= 2);
    loop {
        // Seek-to-max alignment across the required providers — plus
        // the promoted optional once it joined the required set.
        let mut max_doc = candidate;
        if promoted_optional {
            let optional = &mut optionals[0];
            // Always re-seek: a promoted clause is required from here on,
            // and the cursor only ever moves forward with the candidate.
            probe.seeks += 1;
            match optional.seek_eq(candidate) {
                Some(freq) => {
                    promoted_freq = freq;
                }
                None => match optional.current_doc_id() {
                    // The promoted optional ran out: with it being
                    // required, the intersection is empty from here.
                    None => {
                        return Ok(true);
                    }
                    Some(cur) => max_doc = max_doc.max(cur),
                },
            }
        }
        for (index, posting) in requireds.iter_mut().enumerate() {
            if !aligned[index] {
                probe.seeks += 1;
                match posting.seek_eq(candidate) {
                    Some(freq) => {
                        freqs[index] = freq;
                        aligned[index] = true;
                    }
                    None => {
                        let Some(cur) = posting.current_doc_id() else {
                            // A required list ran out: the intersection
                            // is empty from here on.
                            return Ok(true);
                        };
                        max_doc = max_doc.max(cur);
                    }
                }
            }
            max_doc = max_doc.max(posting.current_doc_id().unwrap_or(u64::MAX));
        }
        if max_doc > candidate {
            candidate = max_doc;
            aligned.iter_mut().for_each(|aligned| *aligned = false);
            continue;
        }

        probe.candidates += 1;
        let mut required_sum = 0.0_f32;
        for (index, posting) in requireds.iter().enumerate() {
            let Some(freq) = (aligned[index]).then(|| freqs[index]) else {
                return Ok(true);
            };
            required_sum += posting.term_score(candidate, freq);
        }
        if promoted_optional {
            // The promoted clause's contribution: after the N == 1
            // promotion it is a required clause — its aligned frequency
            // feeds required_sum exactly like the others.
            required_sum += optionals[0].term_score(candidate, promoted_freq);
        }
        let mut optional_sum = 0.0_f32;
        let mut newly_dry = false;
        for (index, posting) in optionals.iter_mut().enumerate() {
            if promoted_optional && index == 0 {
                // Promoted: its frequency is already in required_sum and
                // its hit is enforced by the alignment above.
                continue;
            }
            probe.seeks += 1;
            if let Some(freq) = posting.seek_eq(candidate) {
                optional_sum += posting.term_score(candidate, freq);
            } else if posting.current_doc_id().is_none() && !dry[index] {
                dry[index] = true;
                newly_dry = true;
            }
        }
        if newly_dry {
            optional_ub = live_upper_bound(&dry, |index| optionals[index].global_upper_bound());
        }
        let current_floor = floor.get();
        if current_floor.is_finite() {
            if !promoted_optional
                && optionals.len() == 1
                && iu_cannot_compete(required_ub, 0.0, current_floor)
            {
                // N == 1 promotion: MUST-only documents are provably dead.
                // The promoted clause stays in `optionals` at index 0 but
                // participates as required from here on, so the projected
                // required bound is recomputed from the original per-clause
                // bounds — never by adding a raw f32 onto the already-widened
                // `required_ub`, which could round down by an ULP and make the
                // post-promotion pruning tighter than the proof that fired.
                promoted_optional = true;
                required_ub = conservative_score_sum(
                    required_ubs
                        .iter()
                        .copied()
                        .chain(std::iter::once(optionals[0].global_upper_bound())),
                );
                optional_ub = 0.0;
                continue;
            }
            if iu_cannot_compete(required_ub, optional_ub, current_floor) {
                return Ok(true);
            }
        }
        let Some(key) = requireds[0].document_key_for_doc_id(candidate as u32) else {
            candidate += 1;
            aligned.iter_mut().for_each(|aligned| *aligned = false);
            continue;
        };
        // Only candidates that reach the live floor are emitted. Sub-floor
        // candidates are dropped right here rather than handed on: the floor
        // only rises, so a dropped candidate could never have competed, and
        // passing it on would only buy a heap insert the collector rejects (the
        // emit sink re-checks the same shared value anyway). Equality is kept —
        // a candidate tying the floor can still win the row-id tie-break.
        let score = required_sum + optional_sum;
        if score >= current_floor {
            probe.emits += 1;
            if !emit(key, score)? {
                // The collector can take no more (heap overflow). This run is
                // over: the caller retries the partition with resolved keys.
                return Ok(true);
            }
        }
        candidate += 1;
        aligned.iter_mut().for_each(|aligned| *aligned = false);
    }
}
