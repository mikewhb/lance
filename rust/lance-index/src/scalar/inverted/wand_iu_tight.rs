// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Boolean scoring for the `intersection_union` shape: K required term
//! clauses plus N optional ones (named after the Search Benchmark Game's
//! query of that shape).
//!
//! Two kernels cover the family. `iu_tight_search` handles a single
//! required clause: walk MUST until the heap floor proves some SHOULD is
//! individually required (`max(MUST) + Σ_{j≠i} max(SHOULD_j) < floor`),
//! then switch the lead to the cheapest such list. Promotion uses
//! list-wide bounds only, and the optional union is never made required as
//! a whole — that forces a full-union walk before any intersection pruning
//! can start, which measured slower. `iu_tight_search_multi` aligns two or
//! more required clauses by seek-to-max; its only floor-driven pruning is
//! ending the walk once the optional budget runs dry (see the per-candidate
//! check below).

use lance_core::Result;

use super::super::compound::BlockBatch;
use super::super::scorer::Scorer;
use super::{
    BulkAndMode, CompetitiveFloorMode, PostingIterator, TERMINATED_DOC_ID, WandDocuments,
    conservative_score_sum, score_sum_cannot_compete, score_sum_upper_bound_factor,
};

/// Whether `partial + remaining` can no longer reach `floor`. Wraps the
/// generic path's predicate so the widening and the inclusive floor semantics
/// live in one audited place; a document scoring exactly at the floor still
/// competes, which is the contract the IU kernels are tested against. Bounds
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

/// Env gate for the IU fast paths, following the `LANCE_FTS_BULK_AND`
/// convention: `auto` (default) and `on`/`1` are equivalent — both run both
/// kernels — and `off`/`0` routes every IU-shaped Boolean back to the generic
/// scorer tree. This is the A/B and rollout switch; it does not change
/// results, only which engine computes them.
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

pub fn iu_tight_lead_is_cheaper(must_cost: usize, lead_cost: usize) -> bool {
    lead_cost.saturating_mul(IU_TIGHT_LEAD_COST_RATIO) < must_cost
}

struct IuTightScoring<'a, D, S> {
    documents: &'a D,
    scorer: &'a S,
}

impl<'a, D: WandDocuments, S: Scorer> IuTightScoring<'a, D, S> {
    fn term_score(&self, posting: &PostingIterator, doc: u64, freq: u32) -> f32 {
        posting.score(
            self.scorer,
            freq,
            self.documents.scoring_num_tokens(doc as u32),
        )
    }
}

/// Walk every MUST doc, fold `must + sum(matching shoulds)`, and promote
/// to a sparse required SHOULD once the floor makes that list mandatory.
pub fn iu_tight_search<D, S>(
    documents: &D,
    scorer: &S,
    mut must: PostingIterator,
    mut shoulds: Vec<PostingIterator>,
    on_hit: &mut dyn FnMut(u64, f32) -> Result<bool>,
    floor: impl Fn() -> f32,
) -> Result<()>
where
    D: WandDocuments,
    S: Scorer,
{
    let scoring = IuTightScoring { documents, scorer };
    let must_ub = must.global_upper_bound(scorer);
    let should_ubs: Vec<f32> = shoulds
        .iter()
        .map(|should| should.global_upper_bound(scorer))
        .collect();
    let mut chunk = Vec::with_capacity(128);
    let mut last_doc = 0_u64;
    // The promotion decision is a pure function of the floor: the bounds are
    // list-wide constants and the floor only rises. Re-deriving it is O(N^2)
    // in the optional count, so reuse the decision while the floor is
    // unchanged.
    let mut promotion_decision: Option<(f32, Option<Vec<usize>>)> = None;
    // A SHOULD whose cursor ran dry contributes nothing from here on. When
    // even the must alone plus the live optionals cannot reach the floor,
    // the rest of the MUST walk is dead and ends early.
    let mut dry: Vec<bool> = vec![false; shoulds.len()];
    // Candidate doc ids of the current MUST block, ascending.
    let mut candidates: Vec<u64> = Vec::with_capacity(128);
    // Frequency scratch reused by merge_optional_freqs per optional clause.
    let mut freqs: Vec<u32> = Vec::with_capacity(128);
    let mut should_sums: Vec<f32> = Vec::with_capacity(128);
    loop {
        must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut chunk);
        if chunk.is_empty() {
            break;
        }
        candidates.clear();
        candidates.extend(chunk.iter().map(|(doc, _)| *doc));
        // `merge_optional_freqs` walks each SHOULD posting block-continuously
        // against the candidates: a block of a sparse MUST spans thousands of
        // document ids, so seeking each SHOULD posting once per candidate
        // pays a full block locate for every one of them; the merge turns
        // that into the block decodes that actually contain candidates, and
        // its buffers stay bounded by the candidate count instead of the
        // window's document span. Accumulating per clause in clause order
        // keeps the f32 sum bit-identical to the generic path's
        // `DisjunctionScorer` (Sum) plus `ReqOptScorer::score` accumulation.
        should_sums.clear();
        should_sums.resize(chunk.len(), 0.0);
        for (index, should) in shoulds.iter_mut().enumerate() {
            freqs.clear();
            freqs.resize(candidates.len(), 0);
            should.merge_optional_freqs(&candidates, &mut freqs);
            if should.current_doc_id().is_none() {
                dry[index] = true;
            }
            for (candidate, &freq) in freqs.iter().enumerate() {
                if freq != 0 {
                    should_sums[candidate] +=
                        scoring.term_score(should, candidates[candidate], freq);
                }
            }
        }
        for (candidate, &(doc, freq)) in chunk.iter().enumerate() {
            last_doc = doc;
            let Some(key) = scoring.documents.document_key_for_doc_id(doc as u32) else {
                continue;
            };
            let must_score = scoring.term_score(&must, doc, freq);
            let score = must_score + should_sums[candidate];
            if !on_hit(key, score)? {
                return Ok(());
            }
        }
        let current_floor = floor();
        if current_floor.is_finite() {
            // Dry optionals contribute nothing from here on; if the must
            // alone plus the live optionals cannot reach the floor, every
            // remaining MUST document is dead and the walk ends.
            let live_optional_ub = live_upper_bound(&dry, |index| should_ubs[index]);
            if iu_cannot_compete(must_ub, live_optional_ub, current_floor) {
                return Ok(());
            }
            if promotion_decision
                .as_ref()
                .is_none_or(|(decided_floor, _)| *decided_floor != current_floor)
            {
                promotion_decision = Some((
                    current_floor,
                    iu_tight_required_shoulds(must_ub, &should_ubs, current_floor),
                ));
            }
            if let Some(Some(required)) = promotion_decision.as_ref().map(|(_, decision)| decision)
            {
                if required.is_empty() {
                    return Ok(());
                }
                let lead_cost = required
                    .iter()
                    .map(|&index| shoulds[index].cost())
                    .min()
                    .unwrap_or(usize::MAX);
                if iu_tight_lead_is_cheaper(must.cost(), lead_cost) {
                    return scoring.promoted(
                        must,
                        shoulds,
                        required.clone(),
                        last_doc.saturating_add(1),
                        on_hit,
                    );
                }
            }
        }
    }
    Ok(())
}

/// IU kernel for K >= 2 required clauses plus N optional clauses.
///
/// Every required clause must hit the document; the optional clauses are
/// probed per candidate and summed. The classic seek-to-max convergence
/// aligns all required cursors on one document, then the optional clauses are
/// probed. No optional-to-required promotion: the required set is fixed.
pub fn iu_tight_search_multi<D, S>(
    documents: &D,
    scorer: &S,
    mut required: Vec<PostingIterator>,
    mut optional: Vec<PostingIterator>,
    on_hit: &mut dyn FnMut(u64, f32) -> Result<bool>,
    floor: impl Fn() -> f32,
) -> Result<()>
where
    D: WandDocuments,
    S: Scorer,
{
    debug_assert!(required.len() >= 2);
    let scoring = IuTightScoring { documents, scorer };
    let mut required_ub =
        conservative_score_sum(required.iter().map(|p| p.global_upper_bound(scorer)));
    let mut optional_ub =
        conservative_score_sum(optional.iter().map(|p| p.global_upper_bound(scorer)));
    let mut dry = vec![false; optional.len()];

    // Align every required cursor on one document: seek-to-max until they
    // agree. A required list running out empties the intersection. Alignment
    // uses the doc-id-only seek (`next_doc_id`), which leaves the frequency
    // stream lazy; `doc()` materializes it only for candidates that survive.
    let mut candidate = 0_u64;
    loop {
        let mut max_doc = candidate;
        for posting in required.iter_mut() {
            if posting.current_doc_id().is_none_or(|cur| cur < candidate) {
                posting.next_doc_id(candidate, true);
            }
            let Some(cur) = posting.current_doc_id() else {
                return Ok(());
            };
            max_doc = max_doc.max(cur);
        }
        if max_doc > candidate {
            candidate = max_doc;
            continue;
        }

        // A required posting may name an invisible document (deleted rows
        // stay in postings until compaction); the K=1 kernel skips those too.
        let Some(key) = documents.document_key_for_doc_id(candidate as u32) else {
            candidate += 1;
            continue;
        };

        // Accumulating the required clauses first, then the matching
        // optional clauses in plan order, is the same order as
        // RequiredConjunctionScorer plus DisjunctionScorer inside ReqOptScorer,
        // so the f32 result is bit-identical to the generic path.
        let mut required_sum = 0.0_f32;
        for posting in required.iter() {
            let Some(cur) = posting.doc() else {
                // The alignment loop above guarantees every required cursor
                // sits on `candidate`; a cursor that moved out from under us
                // can only mean the intersection is exhausted.
                return Ok(());
            };
            required_sum += scoring.term_score(posting, candidate, cur.frequency());
        }
        let mut optional_sum = 0.0_f32;
        let mut newly_dry = false;
        for (index, posting) in optional.iter_mut().enumerate() {
            if posting.current_doc_id().is_none_or(|cur| cur < candidate) {
                posting.next_doc_id(candidate, true);
            }
            // A cursor past every document can never contribute again: drop
            // its bound from the remaining budget for good.
            if posting.current_doc_id().is_none() && !dry[index] {
                dry[index] = true;
                newly_dry = true;
            }
            if posting.current_doc_id() == Some(candidate) {
                let Some(cur) = posting.doc() else {
                    continue;
                };
                optional_sum += scoring.term_score(posting, candidate, cur.frequency());
            }
        }
        // With the probes done, an optional that ran dry contributes nothing
        // forever. If even the full required lists cannot lift a document
        // past the floor with the remaining budget, every future candidate is
        // dead and the walk ends.
        if newly_dry {
            optional_ub =
                live_upper_bound(&dry, |index| optional[index].global_upper_bound(scorer));
        }
        let current_floor = floor();
        if current_floor.is_finite() {
            // Promotion (N == 1): MUST-only documents are provably dead —
            // required_ub < floor strictly — so only documents hit by the
            // optional can still compete. Merging the optional into the
            // required set turns the walk into a (K+1)-way seek-to-max
            // driven by the sparse optional cursor; the accumulation order
            // is unchanged (the promoted clause is appended last). N >= 2
            // keeps the list-wide termination: each of several optionals
            // needs its own necessity proof, which v1 does not attempt.
            if optional.len() == 1 && iu_cannot_compete(required_ub, 0.0, current_floor) {
                required.push(optional.remove(0));
                required_ub =
                    conservative_score_sum(required.iter().map(|p| p.global_upper_bound(scorer)));
                optional_ub = 0.0;
                continue;
            }
            if iu_cannot_compete(required_ub, optional_ub, current_floor) {
                return Ok(());
            }
        }
        if !on_hit(key, required_sum + optional_sum)? {
            return Ok(());
        }
        candidate += 1;
    }
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

fn iu_tight_seek_eq(posting: &mut PostingIterator, doc: u64) -> Option<u32> {
    if posting.doc().is_some_and(|info| info.doc_id() < doc) {
        posting.next(doc);
    }
    match posting.doc() {
        Some(info) if info.doc_id() == doc => Some(info.frequency()),
        _ => None,
    }
}

impl<'a, D: WandDocuments, S: Scorer> IuTightScoring<'a, D, S> {
    fn promoted(
        &self,
        mut must: PostingIterator,
        mut shoulds: Vec<PostingIterator>,
        required: Vec<usize>,
        start_doc: u64,
        on_hit: &mut dyn FnMut(u64, f32) -> Result<bool>,
    ) -> Result<()> {
        let Some(lead) = required
            .iter()
            .copied()
            .min_by_key(|&index| shoulds[index].cost())
        else {
            return Ok(());
        };
        // `take_docs_one_block_upto` consumed whole MUST blocks, so the live
        // MUST cursor already sits at or past `start_doc`; the forward seek
        // below is a no-op guard. The lead seek is load-bearing: the merge
        // loop stops at the last candidate, which can leave the lead cursor
        // mid-block below `start_doc`.
        if must.doc().is_none_or(|info| info.doc_id() < start_doc) {
            must.next(start_doc);
        }
        if shoulds[lead]
            .doc()
            .is_some_and(|info| info.doc_id() < start_doc)
        {
            shoulds[lead].next(start_doc);
        }
        let mut chunk = Vec::with_capacity(128);
        loop {
            shoulds[lead].take_docs_one_block_upto(TERMINATED_DOC_ID, &mut chunk);
            if chunk.is_empty() {
                break;
            }
            for &(doc, lead_freq) in &chunk {
                if doc < start_doc {
                    continue;
                }
                let Some(must_freq) = iu_tight_seek_eq(&mut must, doc) else {
                    continue;
                };
                let mut all_required = true;
                for &index in &required {
                    if index == lead {
                        continue;
                    }
                    if iu_tight_seek_eq(&mut shoulds[index], doc).is_none() {
                        all_required = false;
                        break;
                    }
                }
                if !all_required {
                    continue;
                }
                let Some(key) = self.documents.document_key_for_doc_id(doc as u32) else {
                    continue;
                };
                let must_score = self.term_score(&must, doc, must_freq);
                let mut should_sum = 0.0_f32;
                for (index, should) in shoulds.iter_mut().enumerate() {
                    let freq = if index == lead {
                        Some(lead_freq)
                    } else {
                        iu_tight_seek_eq(should, doc)
                    };
                    if let Some(freq) = freq {
                        should_sum += self.term_score(should, doc, freq);
                    }
                }
                if !on_hit(key, must_score + should_sum)? {
                    return Ok(());
                }
            }
        }
        Ok(())
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

// ===== P0.5 prototype: K=1 pausable batch state machine (PLAN-V3 §2.3/§3.4) =====
//
// Pure-data breakpoints (borrow contract): this struct holds no borrows into
// its children — providers (`BlockBatch`) are re-acquired by the host on
// every collect_confirmed_window call; resumption always resumes from the
// stored chunk/chunk-index breakpoint. Never use a forward posting seek as
// recovery (the remaining chunk candidates would be silently skipped — the
// kernel promoted() load-bearing seek comment at :418-431 is the mirror
// image of this exact problem).
//
// Floor semantics (I-4): each `next_batch` call's entry `floor` is the emit
// decision value for that segment, inclusive; the block-end dry-budget /
// promotion decisions use the same value (I-7: floor refresh point = yield
// boundary).
/// Promoted sub-mode (I-8): drive the cheaper lead clause block-wise and
/// seek every required clause per candidate (the kernel `promoted()` walk
/// in provider form). Pure data — providers are re-acquired per call, and
/// resumption always resumes from the stored lead chunk/index.
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
                // of its promoted region (I-1); a miss skips the candidate.
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
                let mut score = must.term_score(doc, must_freq);
                for (index, should) in shoulds.iter_mut().enumerate() {
                    let freq = if index == self.lead_index {
                        Some(lead_freq)
                    } else {
                        should.seek_eq(doc)
                    };
                    if let Some(freq) = freq {
                        score += should.term_score(doc, freq);
                    }
                }
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
    /// Merge scratch.
    freqs: Vec<u32>,
    /// Per-candidate Σ SHOULD scores (computed at block take; preserved
    /// across pauses).
    should_sums: Vec<f32>,
    /// In-chunk candidate index (pause breakpoint; >= chunk.len() means this
    /// block is fully consumed).
    chunk_idx: usize,
    /// Most recently processed candidate doc id (including invisible skipped
    /// ones — the promotion boundary depends on it).
    last_doc: u64,
    /// Whether at least one block has been taken (distinguishes the first
    /// call from a block-end check).
    chunk_started: bool,
    /// Promoted sub-mode state; the MUST walk is dead once this is Some
    /// (I-1: the engine is one-way, and the promoted walk owns everything).
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
            freqs: Vec::new(),
            should_sums: Vec::new(),
            chunk_idx: 0,
            last_doc: 0,
            chunk_started: false,
            promoted: None,
            exhausted: false,
        }
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Advances one segment: consumes the current chunk (resuming at the
    /// breakpoint) or takes the next block, delivering at most `max_hits`
    /// `(document_key, score)` pairs into `out` (score >= floor, inclusive;
    /// I-4). Returns true = exhausted (I-7: every non-exhausted return
    /// strictly advances the buffer offset).
    ///
    /// At a consumed block end: sync_after_batch (invalidate provider derived
    /// caches) → dry-budget termination → promotion decision (I-8: only the
    /// per-should necessity proof; a fired proof with a cheaper lead hands
    /// the walk to the promoted sub-mode, I-2 option (a) — the MUST walk
    /// never interleaves with it again).
    pub(crate) fn next_batch<'s>(
        &mut self,
        must: &'s mut dyn BlockBatch,
        shoulds: &'s mut [&'s mut dyn BlockBatch],
        floor: f32,
        out: &mut Vec<(u64, f32)>,
        max_hits: usize,
    ) -> bool {
        debug_assert!(max_hits > 0, "max_hits=0 has a separate contract (I-7)");
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
        loop {
            // 1) In-chunk resumption (pause-breakpoint recovery lands here:
            // chunk / should_sums already live in the state machine; the
            // providers are untouched).
            while self.chunk_idx < self.chunk.len() {
                let (doc, freq) = self.chunk[self.chunk_idx];
                let should_sum = self.should_sums[self.chunk_idx];
                self.chunk_idx += 1;
                self.last_doc = doc;
                let Some(key) = must.document_key_for_doc_id(doc as u32) else {
                    continue;
                };
                let score = must.term_score(doc, freq) + should_sum;
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
            // 3) Take the next block and rebuild the merge state.
            must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut self.chunk);
            if self.chunk.is_empty() {
                self.exhausted = true;
                return true;
            }
            self.candidates.clear();
            self.candidates
                .extend(self.chunk.iter().map(|(doc, _)| *doc));
            self.should_sums.clear();
            self.should_sums.resize(self.chunk.len(), 0.0);
            for (index, should) in shoulds.iter_mut().enumerate() {
                self.freqs.clear();
                self.freqs.resize(self.candidates.len(), 0);
                should.merge_optional_freqs(&self.candidates, &mut self.freqs);
                if should.current_doc_id().is_none() {
                    self.dry[index] = true;
                }
                for (candidate, &freq) in self.freqs.iter().enumerate() {
                    if freq != 0 {
                        self.should_sums[candidate] +=
                            should.term_score(self.candidates[candidate], freq);
                    }
                }
            }
            self.chunk_idx = 0;
            self.chunk_started = true;
        }
    }
}

// ===== P0.5 harness: protocol path walkthrough (first takeover / pause and
// resume / latch no-op / exhaustion). Drives IuBatchState with mock
// providers to verify the borrow design (FSM = pure data, Send), incremental
// vs one-shot equivalence, and soundness under a dynamic floor. The real
// provider (TermLeafScorer) lands in P1a/P2.

#[cfg(test)]
mod batch_proto_tests {
    use std::collections::HashSet;

    use super::super::super::DocInfo;
    use super::super::{FlatDocuments, WandDocuments};
    use super::{BlockBatch, IuBatchState};

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

    /// EXT R1 counterexample skeleton: MUST docs 1..=20 each freq 1;
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

    #[test]
    fn batch_first_takeover_delivers_hits_above_floor() {
        let (mut must, mut shoulds, docs, _oracle) = fixtures();
        let mut state = IuBatchState::new(1.0, vec![10.0, 10.0]);
        let mut providers: Vec<&mut dyn BlockBatch> =
            shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
        let mut out = Vec::new();
        let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 11.0, &mut out, 100);
        assert!(!exhausted || !out.is_empty());
        assert_eq!(
            out,
            vec![(3, 11.0), (7, 11.0), (17, 11.0)],
            "EXT R1 counterexample shape: candidates tying (=) the floor must be kept (I-4)"
        );
    }

    #[test]
    fn batch_pause_resume_matches_one_shot() {
        let (mut must, mut shoulds, docs, oracle) = fixtures();
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
        // ③→① precondition: after floor (11.0) crosses must_ub (1.0), the FSM
        // performs no union→intersection pruning and keeps scanning MUST —
        // docs hit by should0 (11.0, tying the floor) must be kept (dropping
        // them is the EXT R1 recall loss).
        let (mut must, mut shoulds, docs, oracle) = fixtures();
        let mut state = IuBatchState::new(1.0, vec![10.0, 10.0]);
        let mut providers: Vec<&mut dyn BlockBatch> =
            shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
        let mut out = Vec::new();
        let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 11.0, &mut out, 100);
        assert!(exhausted, "must exhaust after the full scan");
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
        // I-8 promoted sub-mode: once MUST-only (1.0) cannot reach floor
        // (1.5) and the sparse SHOULD is individually necessary, the walk
        // drives the lead (cost 3 < MUST 20) and seeks MUST per candidate.
        // MUST-only docs never clear the floor, so only the lead hits come
        // out.
        let mut must = MockBatch::new((1..=20).map(|d| (d, 1)).collect(), 1.0, 4);
        let mut shoulds = vec![MockBatch::new(vec![(3, 1), (17, 1), (25, 1)], 1.0, 2)];
        let docs = MockDocs {
            hidden: HashSet::new(),
        };
        let mut state = IuBatchState::new(1.0, vec![1.0]);
        let mut out = Vec::new();
        let mut providers: Vec<&mut dyn BlockBatch> =
            shoulds.iter_mut().map(|should| should as &mut dyn BlockBatch).collect();
        let exhausted = state.next_batch(&mut must, providers.as_mut_slice(), 1.5, &mut out, 100);
        assert!(exhausted);
        assert_eq!(
            out,
            vec![(3, 2.0), (17, 2.0)],
            "doc 25 misses MUST and is excluded by the intersection"
        );
        assert!(state.is_exhausted());
        let _ = docs;
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
        let docs = MockDocs {
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
    fn batch_multi_seek_crosses_u32_domain_boundary() {
        // The multi FSM aligns candidates near the u32 doc-id domain edge:
        // a sparse required hit at u32::MAX must survive alignment and
        // deliver, and the walk must exhaust cleanly past the domain.
        let top = u32::MAX as u64;
        let mut must0 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut must1 = MockBatch::new(vec![(top - 5, 1), (top, 1)], 1.0, 4);
        let mut optionals = vec![MockBatch::new(vec![(top - 5, 4)], 4.0, 4)];
        let mut state = super::IuMultiBatchState::new(&[1.0, 1.0], &[4.0]);
        let mut requireds: Vec<&mut dyn BlockBatch> = vec![&mut must0, &mut must1];
        let mut optionals_ref: Vec<&mut dyn BlockBatch> = vec![&mut optionals[0]];
        let mut out = Vec::new();
        let exhausted = state.next_batch(
            &mut requireds,
            &mut optionals_ref,
            3.0,
            &mut out,
            100,
        );
        assert!(exhausted);
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

// ===== P3 prototype: K>=2 pausable batch state machine (PLAN-V3 P3) =====
//
// The multi kernel (iu_tight_search_multi) in provider form: seek-to-max
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
    /// required set. Stateful (unlike the kernel's vector surgery) because
    /// the provider collections are re-acquired per drain call — the flag
    /// must survive across calls.
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

    pub(crate) fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Advances one candidate (or a bounded run of candidates), delivering
    /// at most `max_hits` `(document_key, score)` pairs into `out`. Returns
    /// true = exhausted.
    ///
    /// Promotion (N == 1) mirrors the kernel: MUST-only documents are
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
        if self.exhausted {
            return true;
        }
        let mut hits = 0_usize;
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
                if optionals.len() == 1 && iu_cannot_compete(self.required_ub, 0.0, current_floor)
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
                continue;
            };
            out.push((key, required_sum + optional_sum));
            hits += 1;
            self.candidate += 1;
            self.aligned.iter_mut().for_each(|aligned| *aligned = false);
            self.aligned_promoted = false;
            if hits >= max_hits {
                for posting in requireds.iter_mut().chain(optionals.iter_mut()) {
                    posting.sync_after_batch();
                }
                return false;
            }
        }
    }
}
