// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Boolean scoring for the `intersection_union` shape: K required term
//! clauses plus N optional ones (named after the Search Benchmark Game's
//! query of that shape).
//!
//! The IU batch profile: a suspendable, pure-data state machine that drives
//! flat MUST/SHOULD shapes from the scorer tree's `BlockBatch` providers.
//!
//! `IuBatchState` owns the K = 1 walk (plus the promoted sub-mode) and
//! `IuMultiBatchState` the K >= 2 seek-to-max walk; both consume block-wise
//! provider capabilities (`take_docs_one_block_upto`, `merge_optional_freqs`,
//! `seek_eq`, batch scoring) instead of driving the per-document protocol.
//! The walk yields as soon as a call produced a hit so the collection driver
//! can refresh its competitive floor, and it never hands provider ownership
//! back: batch engagement is one-way per scorer.

use super::super::compound::BlockBatch;
use super::{
    BulkAndMode, CompetitiveFloorMode, TERMINATED_DOC_ID, conservative_score_sum,
    score_sum_cannot_compete, score_sum_upper_bound_factor,
};

/// Whether `partial + remaining` can no longer reach `floor`. Wraps the
/// generic path's predicate so the widening and the inclusive floor semantics
/// live in one audited place; a document scoring exactly at the floor still
/// competes, which is the contract the batch walks are tested against. Bounds
/// are widened by `conservative_score_sum` so f32 rounding cannot shrink them
/// below the true total and trigger premature IU termination: the consumer
/// scores the same values by sequential `f32` accumulation, so with
/// `n >= 3` clauses the accumulation order alone can exceed the f64 sum by
/// `(n-1) * f32::EPSILON` relative error, and a bound that tight would drop a
/// document tying the floor.
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
static IU_TIGHT_MODE: std::sync::LazyLock<BulkAndMode> =
    std::sync::LazyLock::new(|| match std::env::var("LANCE_FTS_IU_TIGHT") {
        Ok(value) => BulkAndMode::parse(&value).unwrap_or_else(|| {
            log::warn!(
                "Invalid LANCE_FTS_IU_TIGHT value {value:?}; expected auto, on/1, or off/0; \
                 falling back to auto"
            );
            BulkAndMode::Auto
        }),
        Err(std::env::VarError::NotPresent) => BulkAndMode::Auto,
        Err(std::env::VarError::NotUnicode(value)) => {
            log::warn!(
                "Invalid non-Unicode LANCE_FTS_IU_TIGHT value {value:?}; expected auto, on/1, \
                 or off/0; falling back to auto"
            );
            BulkAndMode::Auto
        }
    });

/// Whether the IU fast paths may run in this process.
pub fn iu_tight_enabled() -> bool {
    *IU_TIGHT_MODE != BulkAndMode::Off
}

/// Promote only when the new lead is at least this many times cheaper than
/// MUST. A medium-df SHOULD that just barely becomes required is slower to
/// walk than finishing MUST.
pub const IU_TIGHT_LEAD_COST_RATIO: usize = 3;

/// Candidates one multi-walk call may scan before handing control back even
/// without producing a hit. The driver only refreshes its competitive floor
/// between calls (and the floor is shared across partitions), so an unbounded
/// scan of a sub-floor tail would ignore a floor that rose elsewhere. One
/// block's worth of candidates matches the granularity the K = 1 walk already
/// has through its block takes.
pub(crate) const MULTI_CANDIDATE_BUDGET: usize = 128;

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
    use super::{iu_tight_lead_is_cheaper, iu_tight_required_shoulds};

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

// ===== K=1 pausable batch state machine =====
//
// Pure-data breakpoints (borrow contract): this struct holds no borrows into
// its children — providers (`BlockBatch`) are re-acquired by the host on
// every collect_confirmed_window call; resumption always resumes from the
// stored chunk/chunk-index breakpoint. Never use a forward posting seek as
// recovery: the remaining chunk candidates would be silently skipped.
//
// Floor semantics: each `next_batch` call's entry `floor` is the emit
// decision value for that segment, inclusive; the block-end dry-budget /
// promotion decisions use the same value. The floor is only refreshed
// between calls, so the yield boundary is the floor refresh point.
/// Promoted sub-mode: drive the cheaper lead clause block-wise and seek every
/// required clause per candidate. Pure data — providers are re-acquired per
/// call, and resumption always resumes from the stored lead chunk/index.
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

    /// Runs to exhaustion or pause. Returns true = exhausted.
    fn run<'s>(
        &mut self,
        must: &'s mut dyn BlockBatch,
        shoulds: &'s mut [&'s mut dyn BlockBatch],
        floor: f32,
        out: &mut Vec<(u64, f32)>,
        max_hits: usize,
    ) -> bool {
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
                if score >= floor {
                    out.push((key, score));
                    if out.len() >= max_hits {
                        must.sync_after_batch();
                        for should in shoulds.iter_mut() {
                            should.sync_after_batch();
                        }
                        return false;
                    }
                }
            }
            shoulds[self.lead_index]
                .take_docs_one_block_upto(TERMINATED_DOC_ID, &mut self.lead_chunk);
            if self.lead_chunk.is_empty() {
                must.sync_after_batch();
                for should in shoulds.iter_mut() {
                    should.sync_after_batch();
                }
                return true;
            }
            self.lead_idx = 0;
        }
    }
}

pub(crate) struct IuBatchState {
    must_ub: f32,
    should_ubs: Vec<f32>,
    /// SHOULD cursor exhaustion flags (irreversible; once set, the cursor's
    /// bound leaves the live budget for good).
    dry: Vec<bool>,
    /// Promotion decision cached by floor (floor is monotonic ⇒ the decision
    /// is never revoked).
    promotion_decision: Option<(f32, Option<Vec<usize>>)>,
    /// Candidates of the current MUST block ((doc, freq), ascending).
    chunk: Vec<(u64, u32)>,
    /// Doc id view of the chunk (merge_optional_freqs input).
    candidates: Vec<u64>,
    /// Per-candidate Σ SHOULD scores (computed at block take; preserved
    /// across pauses).
    should_sums: Vec<f32>,
    /// Frequency view of the chunk (batch must-scoring input).
    chunk_freqs: Vec<u32>,
    /// Per-candidate MUST scores, batch-computed at block take (one dispatch
    /// per block; preserved across pauses).
    must_scores: Vec<f32>,
    /// Per-candidate visibility keys, batch-computed at block take (None =
    /// invisible).
    keys: Vec<Option<u64>>,
    /// In-chunk candidate index (pause breakpoint; >= chunk.len() means this
    /// block is fully consumed).
    chunk_idx: usize,
    /// Most recently processed candidate doc id (including invisible skipped
    /// ones — the promotion boundary depends on it).
    last_doc: u64,
    /// Whether at least one block has been taken (distinguishes the first
    /// call from a block-end check).
    chunk_started: bool,
    /// Promoted sub-mode state; the MUST walk is dead once this is `Some`
    /// (the walk is one-way, and the promoted walk owns everything).
    promoted: Option<PromotedWalk>,
    exhausted: bool,
}

impl IuBatchState {
    pub(crate) fn new(must_ub: f32, should_ubs: Vec<f32>) -> Self {
        Self {
            must_ub,
            dry: vec![false; should_ubs.len()],
            should_ubs,
            promotion_decision: None,
            chunk: Vec::new(),
            candidates: Vec::new(),
            should_sums: Vec::new(),
            chunk_freqs: Vec::new(),
            must_scores: Vec::new(),
            keys: Vec::new(),
            chunk_idx: 0,
            last_doc: 0,
            chunk_started: false,
            promoted: None,
            exhausted: false,
        }
    }

    /// Test-only: the walk's terminal flag is otherwise observed through
    /// `next_batch`'s return value.
    #[cfg(test)]
    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Advances one segment: consumes the current chunk (resuming at the
    /// breakpoint) or takes the next block, delivering at most `max_hits`
    /// `(document_key, score)` pairs into `out` (score >= floor, inclusive;
    /// a candidate tying the floor still competes). Returns true = exhausted;
    /// every non-exhausted return strictly advances the buffer offset.
    ///
    /// `out` contract: the caller passes a fresh (empty) window per call;
    /// the walk yields as soon as this call produced a hit so the driver can
    /// refresh its competitive floor (the driver inserts produced hits only
    /// after the call returns, so a long call would re-check promotion and
    /// cannot-compete against an obsolete floor).
    ///
    /// At a consumed block end: sync_after_batch (invalidate provider derived
    /// caches) → dry-budget termination → promotion decision (the per-should
    /// necessity proof; a fired proof with a cheaper lead hands the walk to
    /// the promoted sub-mode — once engaged the batch runs to exhaustion and
    /// the MUST walk never interleaves with it again).
    pub(crate) fn next_batch<'s>(
        &mut self,
        must: &'s mut dyn BlockBatch,
        shoulds: &'s mut [&'s mut dyn BlockBatch],
        floor: f32,
        out: &mut Vec<(u64, f32)>,
        max_hits: usize,
    ) -> bool {
        debug_assert!(max_hits > 0, "max_hits must be positive");
        if self.exhausted {
            return true;
        }
        if let Some(mut walk) = self.promoted.take() {
            let exhausted = walk.run(must, shoulds, floor, out, max_hits);
            if exhausted {
                self.exhausted = true;
                return true;
            }
            self.promoted = Some(walk);
            return false;
        }
        let out_start_len = out.len();
        loop {
            // 1) In-chunk resumption (pause-breakpoint recovery lands here:
            // chunk / should_sums / must_scores / keys already live in the
            // state machine; the providers are untouched). All per-candidate
            // tables were batch-computed at block take — the emit loop does
            // no provider dispatch at all.
            while self.chunk_idx < self.chunk.len() {
                let idx = self.chunk_idx;
                let (doc, _freq) = self.chunk[idx];
                self.chunk_idx += 1;
                self.last_doc = doc;
                let Some(key) = self.keys[idx] else {
                    continue;
                };
                let score = self.must_scores[idx] + self.should_sums[idx];
                if score >= floor {
                    out.push((key, score));
                    if out.len() >= max_hits {
                        for provider in std::iter::once(&mut *must).chain(shoulds.iter_mut().map(|s| &mut **s)) {
                            provider.sync_after_batch();
                        }
                        return false;
                    }
                }
            }
            // 1b) Floor-freshness yield: the driver refreshes its competitive
            // floor only between drains (produced hits are inserted into the
            // collector after this call returns), so scanning on would make
            // the block-end decisions below re-check promotion and
            // cannot-compete against a floor that is already stale. Once this
            // call produced a hit (the only way the driver's floor can rise)
            // hand control back; the buffer strictly advanced, so the driver
            // re-issues with the fresh floor.
            if out.len() > out_start_len {
                for provider in std::iter::once(&mut *must).chain(shoulds.iter_mut().map(|s| &mut **s)) {
                    provider.sync_after_batch();
                }
                return false;
            }
            // 2) Block fully consumed: block-end checks (only after really
            // consuming a block).
            if self.chunk_started {
                if floor.is_finite() {
                    let live_optional_ub =
                        live_upper_bound(&self.dry, |index| self.should_ubs[index]);
                    if iu_cannot_compete(self.must_ub, live_optional_ub, floor) {
                        self.exhausted = true;
                        for provider in std::iter::once(&mut *must).chain(shoulds.iter_mut().map(|s| &mut **s)) {
                            provider.sync_after_batch();
                        }
                        return true;
                    }
                    if self
                        .promotion_decision
                        .as_ref()
                        .is_none_or(|(decided_floor, _)| *decided_floor != floor)
                    {
                        self.promotion_decision = Some((
                            floor,
                            iu_tight_required_shoulds(self.must_ub, &self.should_ubs, floor),
                        ));
                    }
                    if let Some(Some(required)) =
                        self.promotion_decision.as_ref().map(|(_, decision)| decision)
                    {
                        if required.is_empty() {
                            self.exhausted = true;
                            for provider in std::iter::once(&mut *must)
                                .chain(shoulds.iter_mut().map(|s| &mut **s))
                            {
                                provider.sync_after_batch();
                            }
                            return true;
                        }
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
                            self.promoted = Some(PromotedWalk::new(
                                lead,
                                required.clone(),
                                self.last_doc.saturating_add(1),
                            ));
                            let mut walk = self.promoted.take().expect("just set");
                            let exhausted = walk.run(must, shoulds, floor, out, max_hits);
                            if exhausted {
                                self.exhausted = true;
                                return true;
                            }
                            self.promoted = Some(walk);
                            return false;
                        }
                    }
                }
                for provider in std::iter::once(&mut *must).chain(shoulds.iter_mut().map(|s| &mut **s)) {
                    provider.sync_after_batch();
                }
            }
            // 3) Take the next block and rebuild the merge state. All
            // per-candidate provider exits (must scores, visibility keys,
            // per-SHOULD scoring) collapse to one dispatch per block.
            must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut self.chunk);
            if self.chunk.is_empty() {
                self.exhausted = true;
                return true;
            }
            self.candidates.clear();
            self.candidates
                .extend(self.chunk.iter().map(|(doc, _)| *doc));
            self.chunk_freqs.clear();
            self.chunk_freqs.extend(self.chunk.iter().map(|(_, f)| *f));
            // must_scores carries no independent zero semantics — every
            // entry is rewritten by term_scores_batch below — so a plain
            // resize (no clear) avoids a per-block memset.
            self.must_scores.resize(self.candidates.len(), 0.0);
            must.term_scores_batch(&self.candidates, &self.chunk_freqs, &mut self.must_scores);
            must.document_keys_batch(&self.candidates, &mut self.keys);
            self.should_sums.clear();
            self.should_sums.resize(self.chunk.len(), 0.0);
            for (index, should) in shoulds.iter_mut().enumerate() {
                // One fused dispatch per SHOULD: merge + score + accumulate
                // in the provider, in the same accumulation order as the
                // separated passes.
                should.fold_optional_scores(&self.candidates, &mut self.should_sums);
                if should.current_doc_id().is_none() {
                    self.dry[index] = true;
                }
            }
            self.chunk_idx = 0;
            self.chunk_started = true;
        }
    }
}

// ===== Protocol harness: path walkthrough (first takeover / pause and
// resume / latch no-op / exhaustion). Drives IuBatchState with mock providers
// to verify the borrow design (FSM = pure data, Send), incremental vs
// one-shot equivalence, and soundness under a dynamic floor.

#[cfg(test)]
mod batch_proto_tests {
    use std::collections::HashSet;

    use super::super::super::DocInfo;
    use super::super::{FlatDocuments, WandDocuments};
    use super::{BlockBatch, IuBatchState, IuMultiBatchState, PromotedWalk};

    /// Minimal documents with configurable visibility / num_tokens.
    struct MockDocs {
        hidden: HashSet<u32>,
    }

    impl MockDocs {
        fn key(&self, doc_id: u32) -> Option<u64> {
            (!self.hidden.contains(&doc_id)).then_some(doc_id as u64)
        }
    }

    impl WandDocuments for MockDocs {
        type Candidate = u64;

        fn len(&self) -> usize {
            1000
        }

        fn scoring_norms(&self) -> Option<&[u8]> {
            None
        }

        fn scoring_num_tokens(&self, _doc_id: u32) -> u32 {
            1
        }

        fn doc_length(&self, _doc: &DocInfo) -> u32 {
            1
        }

        fn document_key(&self, doc: &DocInfo) -> Option<u64> {
            self.key(doc.doc_id() as u32)
        }

        fn document_key_for_doc_id(&self, doc_id: u32) -> Option<u64> {
            self.key(doc_id)
        }

        fn candidate_from_key(&self, key: u64) -> u64 {
            key
        }

        fn flat_documents(&self) -> Option<FlatDocuments<'_>> {
            None
        }

        fn flat_doc_length(&self, _doc_id: u64, _key: u64, _compressed: bool) -> u32 {
            1
        }
    }

    /// Mock provider: in-memory posting (term_score = freq as f32), simulating
    /// block decoding `block` docs at a time. The sync counter supports
    /// contract assertions.
    struct MockBatch {
        docs: Vec<(u64, u32)>,
        pos: usize,
        ub: f32,
        block: usize,
        syncs: usize,
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
                syncs: 0,
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

        fn sync_after_batch(&mut self) {
            self.syncs += 1;
        }
    }

    /// Counterexample fixture: MUST docs 1..=20 each freq 1;
    /// SHOULD0 = {3,17} freq 10; SHOULD1 = {7} freq 10. Score = 1 + 10×hits.
    fn fixtures() -> (MockBatch, Vec<MockBatch>, MockDocs, Vec<(u64, f32)>) {
        let must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let should0 = MockBatch::new(vec![(3, 10), (17, 10)], 10.0, 4);
        let should1 = MockBatch::new(vec![(7, 10)], 10.0, 4);
        let docs = MockDocs {
            hidden: HashSet::new(),
        };
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
        (must, vec![should0, should1], docs, oracle)
    }

    /// Drain the K=1 FSM the way the collection driver does: re-acquire the
    /// providers per call and loop until exhaustion. The FSM yields on every
    /// produced hit (floor-freshness), so delivery must never be assumed
    /// complete in one call.
    fn drain_all(
        state: &mut IuBatchState,
        must: &mut MockBatch,
        shoulds: &mut [MockBatch],
        floor: f32,
        max_hits: usize,
    ) -> Vec<(u64, f32)> {
        let mut all = Vec::new();
        loop {
            let mut providers: Vec<&mut dyn BlockBatch> = shoulds
                .iter_mut()
                .map(|should| should as &mut dyn BlockBatch)
                .collect();
            let mut out = Vec::new();
            let exhausted = state.next_batch(must, providers.as_mut_slice(), floor, &mut out, max_hits);
            all.extend(out);
            if exhausted {
                break;
            }
        }
        all
    }

    #[test]
    fn batch_first_takeover_delivers_hits_above_floor() {
        let (mut must, mut shoulds, _docs, _oracle) = fixtures();
        let mut state = IuBatchState::new(1.0, vec![10.0, 10.0]);
        let out = drain_all(&mut state, &mut must, &mut shoulds, 11.0, 100);
        assert_eq!(
            out,
            vec![(3, 11.0), (7, 11.0), (17, 11.0)],
            "candidates tying (=) the floor must be kept"
        );
    }

    #[test]
    fn batch_pause_resume_matches_one_shot() {
        let (mut must, mut shoulds, _docs, oracle) = fixtures();
        let mut state = IuBatchState::new(1.0, vec![10.0, 10.0]);
        let mut incremental = Vec::new();
        let floor = 11.0;
        loop {
            // Providers are re-acquired per drain call (the borrow contract:
            // the FSM stores pure-data breakpoints, never borrows).
            let mut providers: Vec<&mut dyn BlockBatch> =
                shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
            let mut out = Vec::new();
            let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), floor, &mut out, 1);
            incremental.extend(out);
            if exhausted {
                break;
            }
        }
        let expected: Vec<(u64, f32)> = oracle
            .iter()
            .copied()
            .filter(|(_, score)| *score >= floor)
            .collect();
        assert_eq!(
            incremental, expected,
            "incremental (max_hits=1) and one-shot delivery must match"
        );
    }

    #[test]
    fn batch_keeps_walking_when_floor_rises_past_must_ub() {
        // Precondition: after floor (11.0) crosses must_ub (1.0), the FSM
        // performs no union→intersection pruning and keeps scanning MUST —
        // docs hit by should0 (11.0, tying the floor) must be kept, since
        // dropping them would lose a result.
        let (mut must, mut shoulds, _docs, oracle) = fixtures();
        let mut state = IuBatchState::new(1.0, vec![10.0, 10.0]);
        let out = drain_all(&mut state, &mut must, &mut shoulds, 11.0, 100);
        assert!(state.is_exhausted(), "must exhaust after the full scan");
        let expected: Vec<(u64, f32)> = oracle
            .iter()
            .copied()
            .filter(|(_, score)| *score >= 11.0)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn batch_exhausts_and_respects_visibility() {
        let hidden: HashSet<u32> = HashSet::from([3, 17]);
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        must.hidden = hidden.clone();
        let mut shoulds = vec![MockBatch::new(vec![(3, 10), (17, 10)], 10.0, 4)];
        shoulds[0].hidden = hidden;
        let mut state = IuBatchState::new(1.0, vec![10.0]);
        let mut providers: Vec<&mut dyn BlockBatch> =
            shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
        let mut out = Vec::new();
        let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 5.0, &mut out, 100);
        assert!(exhausted);
        assert!(
            out.is_empty(),
            "the only hits (score 11) are invisible; nothing else clears floor=5.0"
        );
    }


    #[test]
    fn batch_promoted_walk_drives_the_cheaper_sparse_lead() {
        // Promoted sub-mode: once MUST-only (1.0) cannot reach floor
        // (1.5) and the sparse SHOULD is individually necessary, the walk
        // drives the lead (cost 3 < MUST 20) and seeks MUST per candidate.
        // MUST-only docs never clear the floor, so only the lead hits come
        // out.
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut shoulds = vec![MockBatch::new(vec![(3, 1), (17, 1), (25, 1)], 1.0, 2)];
        let _docs = MockDocs {
            hidden: HashSet::new(),
        };
        let mut state = IuBatchState::new(1.0, vec![1.0]);
        let out = drain_all(&mut state, &mut must, &mut shoulds, 1.5, 100);
        assert_eq!(
            out,
            vec![(3, 2.0), (17, 2.0)],
            "doc 25 misses MUST and is excluded by the intersection"
        );
        assert!(state.is_exhausted());
    }

    #[test]
    fn batch_promoted_incremental_matches_one_shot() {
        // The promoted sub-mode must honour the same pause/resume contract as
        // the MUST walk: max_hits=1 drains concatenate to the one-shot drain,
        // with providers re-acquired per call (pure-data breakpoints only).
        let make = || {
            let must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
            let shoulds = vec![MockBatch::new(vec![(3, 1), (17, 1), (25, 1)], 1.0, 2)];
            (must, shoulds)
        };

        let (mut must, mut shoulds) = make();
        let mut state = IuBatchState::new(1.0, vec![1.0]);
        let one_shot = drain_all(&mut state, &mut must, &mut shoulds, 1.5, 100);
        assert_eq!(one_shot, vec![(3, 2.0), (17, 2.0)]);

        let (mut must, mut shoulds) = make();
        let mut state = IuBatchState::new(1.0, vec![1.0]);
        let mut incremental = Vec::new();
        loop {
            let mut providers: Vec<&mut dyn BlockBatch> =
                shoulds.iter_mut().map(|s| s as &mut dyn BlockBatch).collect();
            let mut out = Vec::new();
            let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 1.5, &mut out, 1);
            incremental.extend(out);
            if exhausted {
                break;
            }
        }
        assert_eq!(
            incremental, one_shot,
            "promoted pause/resume (max_hits=1) must match the one-shot drain"
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
        let mut state = IuBatchState::new(1.0, vec![2.0, 1.0]);
        let out = drain_all(&mut state, &mut must, &mut shoulds, 2.5, 100);
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
        let mut must = MockBatch::new(
            (1..=200).map(|d| (d, 1)).collect(),
            1.0,
            32,
        );
        must.hidden = (1..=199).collect();
        let mut shoulds = vec![MockBatch::new(vec![(200, 10)], 10.0, 32)];
        let _docs = MockDocs {
            hidden: (1..=199).collect(),
        };
        let mut state = IuBatchState::new(1.0, vec![10.0]);
        let mut providers: Vec<&mut dyn BlockBatch> =
            shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
        let mut out = Vec::new();
        let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 5.0, &mut out, 100);
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
        let mut shoulds = vec![
            MockBatch::new(vec![(1, 1)], 1.0, 4),
            MockBatch::new(vec![(1, 1)], 1.0, 4),
        ];
        let mut walk = PromotedWalk::new(0, vec![0, 1], 0);
        let mut providers: Vec<&mut dyn BlockBatch> = shoulds
            .iter_mut()
            .map(|should| should as &mut dyn BlockBatch)
            .collect();
        let mut out = Vec::new();
        let exhausted = walk.run(&mut must, providers.as_mut_slice(), 16_777_218.0, &mut out, 100);
        assert!(exhausted);
        assert_eq!(
            out,
            vec![(1, 16_777_218.0)],
            "optionals accumulate from zero, then MUST is added once"
        );
        assert_eq!(out[0].1.to_bits(), (16_777_216.0_f32 + 2.0).to_bits());
    }

    #[test]
    fn batch_promoted_resume_with_a_trailing_lead_matches_one_shot() {
        // The promoted lead is not the first optional, two optionals are
        // necessary, and the lead hits span blocks: incremental (max_hits = 1)
        // and one-shot delivery must agree exactly, with the optionals still
        // scored in query order on every resumed call.
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
        let mut state = IuBatchState::new(1.0, vec![1.0, 6.0, 6.0]);
        let one_shot = drain_all(&mut state, &mut must, &mut shoulds, floor, 100);

        let (mut must, mut shoulds) = make();
        let mut state = IuBatchState::new(1.0, vec![1.0, 6.0, 6.0]);
        let mut incremental = Vec::new();
        loop {
            let mut providers: Vec<&mut dyn BlockBatch> = shoulds
                .iter_mut()
                .map(|should| should as &mut dyn BlockBatch)
                .collect();
            let mut out = Vec::new();
            let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), floor, &mut out, 1);
            incremental.extend(out);
            if exhausted {
                break;
            }
        }
        assert_eq!(incremental, one_shot, "promoted resume must match one-shot");
        for (left, right) in incremental.iter().zip(one_shot.iter()) {
            assert_eq!(left.1.to_bits(), right.1.to_bits(), "score bits must be stable");
        }
        assert!(
            !one_shot.is_empty(),
            "the promoted walk (lead index 1) must have delivered the necessary hits"
        );
    }

    #[test]
    fn batch_yields_for_a_fresh_floor_instead_of_scanning_the_tail() {
        // Regression: both sparse optionals hit only inside the first
        // MUST block, neither is individually necessary (so no promotion can
        // mask the effect), and the MUST upper bound is loose (10 >= the real
        // 1.0), so a frozen floor cannot prove the tail dead. The driver raises
        // its floor after the produced hit (the collector's behaviour); only a
        // freshness yield lets the walk terminate at the next call. Without it
        // the first call scans all 60 blocks.
        let mut must = MockBatch::new((1..=240).map(|d| (d, 1)).collect(), 10.0, 4);
        let mut shoulds = vec![
            MockBatch::new(vec![(3, 10)], 10.0, 4),
            MockBatch::new(vec![(3, 10)], 10.0, 4),
        ];
        let mut state = IuBatchState::new(10.0, vec![10.0, 10.0]);
        let mut floor = 1.5_f32;
        let mut out = Vec::new();
        loop {
            let mut providers: Vec<&mut dyn BlockBatch> = shoulds
                .iter_mut()
                .map(|should| should as &mut dyn BlockBatch)
                .collect();
            let mut window = Vec::new();
            let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), floor, &mut window, 10);
            for &(_, score) in &window {
                floor = floor.max(score + 0.5);
            }
            out.extend(window);
            if exhausted {
                break;
            }
        }
        assert_eq!(out, vec![(3, 21.0)], "the far tail must not produce hits");
        assert!(
            must.takes <= 3,
            "the walk must stop soon after the driver's floor rises (took {} blocks)",
            must.takes
        );
    }

    #[test]
    fn multi_bounds_its_scan_so_an_external_floor_rise_is_seen() {
        // A long sub-floor tail while the
        // competitive floor rises *outside* this walk (the floor is shared
        // across partitions). The per-call candidate budget hands control
        // back at block granularity so the driver re-checks with the fresh
        // floor; without it the whole tail is scanned against a frozen floor.
        let mut must0 = MockBatch::new((1..=400).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut must1 = MockBatch::new((1..=400).map(|d| (d, 1)).collect(), 1.0, 4);
        // The optional's only hit sits far beyond the required candidates, so
        // it never contributes and never runs dry: the frozen floor cannot
        // prove the tail dead, while the externally raised floor can. Every
        // required candidate therefore scores 2.0, below the 2.5 floor.
        let mut optionals = vec![MockBatch::new(vec![(10_000, 1)], 1.0, 4)];
        // Loose required bound (4.0 >= the real 2.0): the frozen 2.5 floor
        // can neither prove the tail dead nor trigger the N == 1 promotion.
        let mut state = IuMultiBatchState::new(&[2.0, 2.0], &[1.0]);
        let mut floor = 2.5_f32; // above the real required sum (2.0)
        loop {
            let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
            let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
            let mut out = Vec::new();
            let exhausted =
                state.next_batch(&mut requireds, &mut optionals_ref, floor, &mut out, 10);
            assert!(out.is_empty(), "every candidate is below the floor");
            floor = 6.0; // simulated external rise of the shared floor
            if exhausted {
                break;
            }
        }
        let scanned = must0.seeks + must1.seeks;
        assert!(
            scanned <= 2 * super::MULTI_CANDIDATE_BUDGET + 16,
            "the walk must hand back at the budget so the fresh floor is seen (scanned {scanned})"
        );
    }

    #[test]
    fn batch_multi_seek_crosses_u32_domain_boundary() {
        // The multi FSM aligns candidates near the u32 doc-id domain edge:
        // a sparse required hit at u32::MAX must survive alignment and
        // deliver, and the walk must exhaust cleanly past the domain.
        let top = u32::MAX as u64;
        let mut must0 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut must1 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut optionals = vec![MockBatch::new(vec![(top - 5, 4)], 4.0, 4)];
        let mut state = super::IuMultiBatchState::new(&[1.0, 1.0], &[4.0]);
        let mut out = Vec::new();
        // The multi FSM yields on every produced hit (floor freshness), so
        // drain until exhaustion the way the collection driver does, with the
        // providers re-acquired per call.
        loop {
            let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
            let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
            let exhausted = state.next_batch(
                &mut requireds,
                &mut optionals_ref,
                3.0,
                &mut out,
                100,
            );
            if exhausted {
                break;
            }
        }
        assert_eq!(out, vec![(top - 5, 6.0)]);
    }

    #[test]
    fn batch_state_is_send() {
        // Compile-time proof of the borrow contract: the FSM is pure data ⇒
        // Send (it can move across threads with the host).
        fn assert_send<T: Send>(_: &T) {}
        let state = IuBatchState::new(1.0, vec![1.0]);
        assert_send(&state);
    }
}

// ===== K>=2 pausable batch state machine =====

//
// The multi walk in provider form: seek-to-max
// alignment across the required providers, per-candidate optional probes,
// N == 1 promotion (the optional joins the required set — appended last so
// the accumulation order is unchanged), and the live-budget termination.
// Pure-data breakpoints; providers are re-acquired per drain call.
pub(crate) struct IuMultiBatchState {
    /// Per required provider: does its cursor sit on the aligned candidate?
    aligned: Vec<bool>,
    /// Per required provider: the frequency at the aligned candidate.
    freqs: Vec<u32>,
    required_ub: f32,
    optional_ub: f32,
    dry: Vec<bool>,
    /// N == 1 promotion latch: the lone optional has been promoted into the
    /// required set. It has to be stateful because the provider collections
    /// are re-acquired per drain call — the flag must survive across calls.
    promoted_optional: bool,
    /// Promoted optional's frequency at the aligned candidate.
    promoted_freq: u32,
    /// Alignment flag for the promoted optional.
    aligned_promoted: bool,
    /// Current alignment target.
    candidate: u64,
    exhausted: bool,
}

impl IuMultiBatchState {
    pub(crate) fn new(required_ubs: &[f32], optional_ubs: &[f32]) -> Self {
        Self {
            aligned: vec![false; required_ubs.len()],
            freqs: vec![0; required_ubs.len()],
            required_ub: conservative_score_sum(required_ubs.iter().copied()),
            optional_ub: conservative_score_sum(optional_ubs.iter().copied()),
            dry: vec![false; optional_ubs.len()],
            promoted_optional: false,
            promoted_freq: 0,
            aligned_promoted: false,
            candidate: 0,
            exhausted: false,
        }
    }

    /// Advances one candidate (or a bounded run of candidates), delivering
    /// at most `max_hits` `(document_key, score)` pairs into `out`. Returns
    /// true = exhausted.
    ///
    /// Promotion (N == 1) mirrors the K = 1 walk: MUST-only documents are
    /// provably dead (`required_ub < floor` strictly), so the optional joins
    /// the required set — appended last, keeping the accumulation order.
    pub(crate) fn next_batch<'s>(
        &mut self,
        requireds: &'s mut Vec<&'s mut dyn BlockBatch>,
        optionals: &'s mut Vec<&'s mut dyn BlockBatch>,
        floor: f32,
        out: &mut Vec<(u64, f32)>,
        max_hits: usize,
    ) -> bool {
        debug_assert!(requireds.len() >= 2);
        debug_assert!(max_hits > 0, "max_hits must be positive");
        if self.exhausted {
            return true;
        }
        let mut hits = 0_usize;
        // Scanned candidates of this call only (cooperation budget).
        let mut scanned = 0_usize;
        loop {
            // Seek-to-max alignment across the required providers — plus
            // the promoted optional once it joined the required set.
            let mut max_doc = self.candidate;
            if self.promoted_optional {
                let optional = &mut optionals[0];
                if !self.aligned_promoted {
                    match optional.seek_eq(self.candidate) {
                        Some(freq) => {
                            self.promoted_freq = freq;
                            self.aligned_promoted = true;
                        }
                        None => match optional.current_doc_id() {
                            // The promoted optional ran out: with it being
                            // required, the intersection is empty from here.
                            None => {
                                self.exhausted = true;
                                return true;
                            }
                            Some(cur) => max_doc = max_doc.max(cur),
                        },
                    }
                }
            }
            for (index, posting) in requireds.iter_mut().enumerate() {
                if !self.aligned[index] {
                    match posting.seek_eq(self.candidate) {
                        Some(freq) => {
                            self.freqs[index] = freq;
                            self.aligned[index] = true;
                        }
                        None => {
                            let Some(cur) = posting.current_doc_id() else {
                                // A required list ran out: the intersection
                                // is empty from here on.
                                self.exhausted = true;
                                return true;
                            };
                            max_doc = max_doc.max(cur);
                        }
                    }
                }
                max_doc = max_doc.max(posting.current_doc_id().unwrap_or(u64::MAX));
            }
            if max_doc > self.candidate {
                self.candidate = max_doc;
                self.aligned.iter_mut().for_each(|aligned| *aligned = false);
                self.aligned_promoted = false;
                continue;
            }

            let mut required_sum = 0.0_f32;
            for (index, posting) in requireds.iter().enumerate() {
                let Some(freq) = (self.aligned[index]).then(|| self.freqs[index]) else {
                    self.exhausted = true;
                    return true;
                };
                required_sum += posting.term_score(self.candidate, freq);
            }
            if self.promoted_optional {
                // The promoted clause's contribution: after the N == 1
                // promotion it is a required clause — its aligned frequency
                // feeds required_sum exactly like the others.
                required_sum += optionals[0].term_score(self.candidate, self.promoted_freq);
            }
            let mut optional_sum = 0.0_f32;
            let mut newly_dry = false;
            for (index, posting) in optionals.iter_mut().enumerate() {
                if self.promoted_optional && index == 0 {
                    // Promoted: its frequency is already in required_sum and
                    // its hit is enforced by the alignment above.
                    continue;
                }
                if let Some(freq) = posting.seek_eq(self.candidate) {
                    optional_sum += posting.term_score(self.candidate, freq);
                } else if posting.current_doc_id().is_none() && !self.dry[index] {
                    self.dry[index] = true;
                    newly_dry = true;
                }
            }
            if newly_dry {
                self.optional_ub =
                    live_upper_bound(&self.dry, |index| optionals[index].global_upper_bound());
            }
            let current_floor = floor;
            if current_floor.is_finite() {
                if !self.promoted_optional
                    && optionals.len() == 1
                    && iu_cannot_compete(self.required_ub, 0.0, current_floor)
                {
                    // N == 1 promotion: MUST-only documents are provably
                    // dead. Pure state — the provider collections are
                    // re-acquired per drain call, so the flag (not vector
                    // surgery) carries the promotion across calls. The
                    // promoted clause stays in `optionals` at index 0 but
                    // participates as required from here on.
                    self.promoted_optional = true;
                    self.required_ub += optionals[0].global_upper_bound();
                    self.optional_ub = 0.0;
                    self.aligned_promoted = false;
                    continue;
                }
                if iu_cannot_compete(self.required_ub, self.optional_ub, current_floor) {
                    self.exhausted = true;
                    return true;
                }
            }
            let Some(key) = requireds[0].document_key_for_doc_id(self.candidate as u32) else {
                self.candidate += 1;
                self.aligned.iter_mut().for_each(|aligned| *aligned = false);
                self.aligned_promoted = false;
                continue;
            };
            // Only candidates that reach this call's floor enter the window
            // and the freshness yield. Sub-floor candidates are pushed by
            // neither: the driver re-checks every window entry against its
            // live floor and would discard them anyway, so emitting them only
            // bought a host round trip — provider re-acquisition, syncs and a
            // window copy — per candidate. Dropping them cannot lose a result
            // because the competitive floor only rises, so the driver's live
            // floor is never below this call's snapshot. Equality is kept: a
            // candidate tying the floor can still win the row-id tie-break.
            let score = required_sum + optional_sum;
            scanned += 1;
            if score >= floor {
                out.push((key, score));
                hits += 1;
            }
            self.candidate += 1;
            self.aligned.iter_mut().for_each(|aligned| *aligned = false);
            self.aligned_promoted = false;
            // Floor-freshness yield (same rule as the K = 1 walk): the driver
            // only refreshes its competitive floor between drains, and the
            // block-end decisions above read this call's frozen floor. Once
            // this call produced a hit — the only way the floor can rise —
            // hand control back with the cursor strictly advanced, so the
            // next call re-checks promotion and cannot-compete against a
            // fresh floor. When no hit is produced the yield falls to
            // `MULTI_CANDIDATE_BUDGET`, which bounds how much work a frozen
            // floor can buy.
            if hits >= max_hits.min(1) || scanned >= MULTI_CANDIDATE_BUDGET {
                for posting in requireds.iter_mut().chain(optionals.iter_mut()) {
                    posting.sync_after_batch();
                }
                return false;
            }
        }
    }
}
