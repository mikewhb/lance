// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! `ReqOptScorer`: required-plus-optional scoring for the IU shape.
//!
//! Extracted verbatim from `compound.rs` (PLAN-V3 P1a, zero behavior
//! change). Declared as a child module of `compound` so it can reach the
//! shared scorer-tree internals via `super::*` while keeping compound.rs
//! small.

use super::super::wand::{IuBatchState, IuMultiBatchState};

#[cfg(test)]
std::thread_local! {
    /// Test-only: how many times the batch profile took over a scorer.
    /// Capability routing must be assertable directly instead of being
    /// inferred from work metrics that the generic path can also move.
    /// Thread-local so parallel tests cannot pollute each other's counts.
    pub(super) static BATCH_ENGAGEMENTS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };

    /// Test-only: how many times the engaged batch profile was drained. Each
    /// drain costs the host a provider re-acquisition and a window copy, so a
    /// candidate that cannot raise the floor must not buy one.
    pub(super) static BATCH_DRAINS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}
use super::*;

/// Required-plus-optional scorer that only touches the optional side when it
/// can change competitiveness or an exact score is requested.
///
/// The required scorer drives iteration. Once a score floor is available,
/// block bounds may either skip the whole range or temporarily turn the
/// optional approximation into a required iterator when the required score
/// cannot reach the floor on its own.
#[derive(Clone, Copy)]
struct ReqOptBounds {
    up_to: u64,
    required: ScoreBounds,
    combined: ScoreBounds,
}

pub(super) struct ReqOptScorer<'a> {
    required: BoxScorer<'a>,
    optional: BoxScorer<'a>,
    current: Option<u64>,
    exhausted: bool,
    optional_initialized: bool,
    optional_is_required: bool,
    optional_checked_doc: Option<u64>,
    optional_matches: bool,
    confirmed_doc: Option<u64>,
    confirmed: bool,
    min_competitive_score: f32,
    shallow_bounds: Option<ReqOptBounds>,
    /// Set once the heap floor passes the required clause's list-wide upper
    /// bound: from then on no document can reach the heap on the required
    /// clause alone, so the optional clause is mandatory for the rest of the
    /// scan and the query is an intersection.
    pub(super) optional_globally_required: bool,
    /// Batch profile state (PLAN-V3 P2/P3). `Some` = a batch FSM owns this
    /// scorer: pure-data breakpoints, providers re-acquired per drain call,
    /// one-way until exhaustion (I-1/I-2 option (a)).
    batch: Option<IuBatchEngagement>,
    /// Reused scratch for the batch drain's hit window. The batch yields on
    /// every produced hit (floor-freshness), so this vector is touched on
    /// every drain — keeping its capacity across calls avoids a per-drain
    /// allocation.
    batch_hits: Vec<(u64, f32)>,
}

/// Which batch engine owns the scorer. `Single` = K = 1 (one required
/// provider, N optional probes); `Multi` = K >= 2 (seek-to-max alignment
/// across required providers).
enum IuBatchEngagement {
    Single(IuBatchState),
    Multi(IuMultiBatchState),
}

impl<'a> ReqOptScorer<'a> {
    pub(super) fn new(required: BoxScorer<'a>, optional: BoxScorer<'a>) -> Self {
        debug_assert!(required.scores_non_negative());
        debug_assert!(optional.scores_non_negative());
        Self {
            required,
            optional,
            current: None,
            exhausted: false,
            optional_initialized: false,
            optional_is_required: false,
            optional_checked_doc: None,
            optional_matches: false,
            confirmed_doc: None,
            confirmed: false,
            min_competitive_score: f32::NEG_INFINITY,
            shallow_bounds: None,
            optional_globally_required: false,
            batch: None,
            batch_hits: Vec::new(),
        }
    }

    /// Capability probe + static skew criterion (tier 3 vs tier 2) on the
    /// same raw-df source as the collect_partition guard (PLAN-V3 §1.3).
    /// Returns the fresh batch state when the batch profile owns this shape.
    ///
    /// Tier 1 (union promotion, `optional_globally_required`) outranks the
    /// batch: a latched shape is already an intersection driven from the
    /// sparse side, which is the R-G1 fix itself.
    fn batch_engagement(&mut self) -> Option<IuBatchEngagement> {
        // Env gate: LANCE_FTS_IU_TIGHT=off must disable the batch profile
        // entirely (the escape hatch the PR promises). Capability alone is
        // not authorization.
        if !iu_tight_enabled() {
            return None;
        }
        // K = 1: the required side is itself one batch provider.
        if let Some(required_batch) = self.required.block_batch() {
            let optional_batch = self.optional.block_batch_children()?;
            let required_cost = required_batch.cost();
            let optional_cost: usize = optional_batch.iter().map(|p| p.cost()).sum();
            // Tier 2 (mean formula, same as the collect_partition guard):
            // sparse optionals against a dense required intersection yield
            // to the generic per-document tree whose block-max skipping and
            // union promotion own the shape.
            // K = 1: the required side is a single provider.
            if super::optionals_are_sparse(1, optional_cost, required_cost) {
                return None;
            }
            let must_ub = required_batch.global_upper_bound();
            let should_ubs = optional_batch
                .iter()
                .map(|provider| provider.global_upper_bound())
                .collect();
            return Some(IuBatchEngagement::Single(IuBatchState::new(
                must_ub, should_ubs,
            )));
        }
        // K >= 2: the required side is a flat AND aggregate.
        let mut requireds = self.required.block_batch_children()?;
        if requireds.len() < 2 {
            return None;
        }
        let mut optionals = self.optional.block_batch_children()?;
        let required_cost: usize = requireds.iter().map(|p| p.cost()).sum();
        let optional_cost: usize = optionals.iter().map(|p| p.cost()).sum();
        if super::optionals_are_sparse(requireds.len(), optional_cost, required_cost) {
            return None;
        }
        let required_ubs: Vec<f32> = requireds.iter().map(|p| p.global_upper_bound()).collect();
        let optional_ubs: Vec<f32> = optionals.iter().map(|p| p.global_upper_bound()).collect();
        Some(IuBatchEngagement::Multi(IuMultiBatchState::new(
            &required_ubs, &optional_ubs,
        )))
    }

    fn set_current(&mut self, current: Option<u64>) {
        if self.current != current {
            self.optional_checked_doc = None;
            self.optional_matches = false;
            self.confirmed_doc = None;
            self.confirmed = false;
        }
        self.current = current;
        self.optional_is_required = false;
    }

    fn set_optional_required(&mut self, required: bool) {
        if self.optional_is_required != required {
            self.confirmed_doc = None;
            self.confirmed = false;
        }
        self.optional_is_required = required;
    }

    /// Promote the optional clause to required once the required clause's
    /// list-wide upper bound can no longer reach the heap. The floor only ever
    /// rises, so this decision is permanent. Past this point there is nothing
    /// for a required-only walk to contribute, and the per-window bookkeeping
    /// is pure overhead.
    fn promote_optional_if_required(&mut self) -> Result<()> {
        if self.optional_globally_required || !self.min_competitive_score.is_finite() {
            return Ok(());
        }
        let Some(required_upper) = self.required.global_score_upper_bound() else {
            return Ok(());
        };
        if !required_upper.is_finite() || required_upper >= self.min_competitive_score {
            return Ok(());
        }
        self.optional_globally_required = true;
        self.shallow_bounds = None;
        Ok(())
    }

    /// Intersect both sides directly. Used once the optional clause is known to
    /// be mandatory: the required clause alone can no longer produce a
    /// competitive document, so the window bookkeeping earns nothing. The
    /// cheaper list drives, so a huge MUST paired with a sparse SHOULD walks the
    /// sparse list instead of the MUST tail.
    fn position_intersection(&mut self, target: u64) -> Result<Option<u64>> {
        self.shallow_bounds = None;
        if self.optional.cost() < self.required.cost() {
            self.intersect_from_optional(target)
        } else {
            self.intersect_from_required(target)
        }
    }

    /// Leapfrog driven by the required (MUST) list.
    fn intersect_from_required(&mut self, target: u64) -> Result<Option<u64>> {
        let Some(mut required_doc) = self.required.advance(target)? else {
            return Ok(self.exhaust());
        };
        loop {
            self.set_current(Some(required_doc));
            self.set_optional_required(true);
            let Some(optional_doc) = self.ensure_optional_at_or_after(required_doc)? else {
                return Ok(self.exhaust());
            };
            if optional_doc == required_doc {
                return Ok(self.current);
            }
            let Some(next_required) = self.required.advance(optional_doc)? else {
                return Ok(self.exhaust());
            };
            required_doc = next_required;
        }
    }

    /// Leapfrog driven by the optional (SHOULD) list, which is the cheaper list
    /// once the optional is mandatory.
    fn intersect_from_optional(&mut self, target: u64) -> Result<Option<u64>> {
        self.optional_initialized = true;
        let Some(mut optional_doc) = self.optional.advance(target)? else {
            return Ok(self.exhaust());
        };
        loop {
            let Some(required_doc) = self.required.advance(optional_doc)? else {
                return Ok(self.exhaust());
            };
            if required_doc == optional_doc {
                self.set_current(Some(optional_doc));
                self.set_optional_required(true);
                return Ok(self.current);
            }
            let Some(next_optional) = self.optional.advance(required_doc)? else {
                return Ok(self.exhaust());
            };
            optional_doc = next_optional;
        }
    }

    fn exhaust(&mut self) -> Option<u64> {
        self.exhausted = true;
        self.set_current(None);
        None
    }

    fn ensure_optional_at_or_after(&mut self, target: u64) -> Result<Option<u64>> {
        if !self.optional_initialized || self.optional.doc().is_some_and(|doc| doc < target) {
            self.optional.advance(target)?;
            self.optional_initialized = true;
        }
        Ok(self.optional.doc())
    }

    fn optional_matches_current(&mut self) -> Result<bool> {
        let Some(current) = self.current else {
            return Ok(false);
        };
        if self.optional_checked_doc == Some(current) {
            return Ok(self.optional_matches);
        }
        self.optional_matches = self.ensure_optional_at_or_after(current)? == Some(current)
            && self.optional.matches()?;
        self.optional_checked_doc = Some(current);
        Ok(self.optional_matches)
    }

    fn usable_bounds(bounds: ScoreBounds) -> bool {
        bounds.lower.is_finite() && bounds.upper.is_finite() && bounds.lower <= bounds.upper
    }

    fn bounds(&mut self, up_to: u64) -> Result<ReqOptBounds> {
        if let Some(bounds) = self.shallow_bounds
            && bounds.up_to == up_to
        {
            return Ok(bounds);
        }

        let required = self.required.score_bounds(up_to)?;
        let optional = if self.optional.doc().is_some_and(|doc| doc <= up_to) {
            let bounds = self.optional.score_bounds(up_to)?;
            if Self::usable_bounds(bounds) {
                bounds.include_zero()
            } else {
                ScoreBounds::UNBOUNDED
            }
        } else {
            ScoreBounds::ZERO
        };
        let combined = required.add(optional);
        let bounds = ReqOptBounds {
            up_to,
            required,
            combined,
        };
        self.shallow_bounds = Some(bounds);
        Ok(bounds)
    }

    fn position(&mut self, mut target: u64) -> Result<Option<u64>> {
        if self.exhausted {
            return Ok(None);
        }
        if self.optional_globally_required {
            return self.position_intersection(target);
        }

        'search: loop {
            let bounds = self.shallow_bounds.filter(|bounds| target <= bounds.up_to);

            if self.min_competitive_score.is_finite()
                && self.min_competitive_score > 0.0
                && let Some(bounds) = bounds
                && Self::usable_bounds(bounds.required)
                && Self::usable_bounds(bounds.combined)
            {
                if bounds.combined.upper < self.min_competitive_score {
                    if bounds.up_to == u64::MAX {
                        return Ok(self.exhaust());
                    }
                    target = bounds.up_to + 1;
                    self.shallow_bounds = None;
                    continue;
                }

                if bounds.required.upper < self.min_competitive_score {
                    // The optional contribution is necessary throughout this
                    // cached shallow range. Intersect approximations until
                    // both sides agree or the range is exhausted.
                    let Some(mut required_doc) = self.required.advance(target)? else {
                        return Ok(self.exhaust());
                    };
                    if required_doc > bounds.up_to {
                        target = required_doc;
                        self.shallow_bounds = None;
                        continue;
                    }
                    self.set_current(Some(required_doc));
                    self.set_optional_required(true);
                    loop {
                        self.set_current(Some(required_doc));
                        self.set_optional_required(true);
                        let Some(optional_doc) = self.ensure_optional_at_or_after(required_doc)?
                        else {
                            if bounds.up_to == u64::MAX {
                                return Ok(self.exhaust());
                            }
                            target = bounds.up_to + 1;
                            self.shallow_bounds = None;
                            continue 'search;
                        };
                        if optional_doc > bounds.up_to {
                            if bounds.up_to == u64::MAX {
                                return Ok(self.exhaust());
                            }
                            target = bounds.up_to + 1;
                            self.shallow_bounds = None;
                            continue 'search;
                        }
                        if optional_doc == required_doc {
                            return Ok(self.current);
                        }
                        let Some(next_required) = self.required.advance(optional_doc)? else {
                            return Ok(self.exhaust());
                        };
                        if next_required > bounds.up_to {
                            target = next_required;
                            self.shallow_bounds = None;
                            continue 'search;
                        }
                        required_doc = next_required;
                    }
                }
            }

            let Some(required_doc) = self.required.advance(target)? else {
                return Ok(self.exhaust());
            };
            self.set_current(Some(required_doc));
            self.set_optional_required(false);
            return Ok(self.current);
        }
    }

    fn ensure_confirmed(&mut self) -> Result<bool> {
        let Some(current) = self.current else {
            return Ok(false);
        };
        if self.confirmed_doc == Some(current) {
            return Ok(self.confirmed);
        }
        self.confirmed = self.required.matches()?
            && (!self.optional_is_required || self.optional_matches_current()?);
        self.confirmed_doc = Some(current);
        Ok(self.confirmed)
    }
}

impl ComposableScorer for ReqOptScorer<'_> {
    fn doc(&self) -> Option<u64> {
        self.current
    }

    fn document_key(&self) -> Option<u64> {
        self.required.document_key()
    }

    fn next(&mut self) -> Result<Option<u64>> {
        let target = match self.current {
            None => 0,
            Some(u64::MAX) => return Ok(self.exhaust()),
            Some(current) => current + 1,
        };
        self.position(target)
    }

    fn advance(&mut self, target: u64) -> Result<Option<u64>> {
        if self.current.is_some_and(|current| current >= target) {
            return Ok(self.current);
        }
        self.position(target)
    }

    fn cost(&self) -> usize {
        self.required.cost()
    }

    fn score(&mut self) -> Result<f32> {
        if !self.ensure_confirmed()? {
            return Err(Error::internal(
                "required-plus-optional FTS score requested for an unconfirmed document",
            ));
        }
        let mut score = self.required.score()?;
        if self.optional_matches_current()? {
            score += self.optional.score()?;
        }
        checked_score(score, "required-plus-optional FTS scorer")
    }

    fn advance_shallow(&mut self, target: u64) -> Result<u64> {
        let target = self.current.map_or(target, |current| target.max(current));
        if let Some(bounds) = self.shallow_bounds
            && target <= bounds.up_to
        {
            return Ok(bounds.up_to);
        }
        self.shallow_bounds = None;
        let mut up_to = self.required.advance_shallow(target)?;
        match self.ensure_optional_at_or_after(target)? {
            Some(optional_doc) if optional_doc <= target => {
                up_to = up_to.min(self.optional.advance_shallow(target)?);
            }
            Some(optional_doc) => {
                up_to = up_to.min(optional_doc.saturating_sub(1));
            }
            None => {}
        }
        Ok(up_to)
    }

    fn score_bounds(&mut self, up_to: u64) -> Result<ScoreBounds> {
        Ok(self.bounds(up_to)?.combined)
    }

    fn global_score_upper_bound(&self) -> Option<f32> {
        let required = self.required.global_score_upper_bound()?;
        let optional = self.optional.global_score_upper_bound()?;
        let combined = ScoreBounds {
            lower: 0.0,
            upper: required,
        }
        .add(ScoreBounds {
            lower: 0.0,
            upper: optional,
        })
        .upper;
        combined.is_finite().then_some(combined)
    }

    fn set_min_competitive_score(&mut self, min_score: f32) -> Result<()> {
        if min_score.is_nan() {
            return Err(Error::invalid_input(
                "minimum competitive FTS score cannot be NaN",
            ));
        }
        if min_score > self.min_competitive_score {
            self.min_competitive_score = min_score;
            // Once the required clause alone can no longer reach the heap the
            // optional clause is mandatory, so switch to a direct intersection.
            self.promote_optional_if_required()?;
        }
        Ok(())
    }

    fn current_score_upper_bound(&mut self) -> Result<Option<f32>> {
        let Some(current) = self.current else {
            return Ok(None);
        };
        let Some(required_upper) = self.required.current_score_upper_bound()? else {
            return Ok(None);
        };
        if !required_upper.is_finite() {
            return Ok(None);
        }
        if required_upper >= self.min_competitive_score {
            // The required side alone keeps this document competitive. Keep
            // the optional iterator lazy; scoring will align it only for a
            // surviving candidate.
            return Ok(Some(f32::INFINITY));
        }
        let optional_upper = if self.ensure_optional_at_or_after(current)? == Some(current) {
            let Some(optional_upper) = self.optional.current_score_upper_bound()? else {
                return Ok(None);
            };
            if !optional_upper.is_finite() {
                return Ok(None);
            }
            optional_upper.max(0.0)
        } else {
            0.0
        };
        let upper = ScoreBounds {
            lower: 0.0,
            upper: required_upper.max(0.0),
        }
        .add(ScoreBounds {
            lower: 0.0,
            upper: optional_upper,
        })
        .upper;
        Ok(upper.is_finite().then_some(upper))
    }

    fn supports_doc_local_confirmation_pruning(&self) -> bool {
        self.required.supports_doc_local_confirmation_pruning()
            || self.optional.supports_doc_local_confirmation_pruning()
    }

    fn matches(&mut self) -> Result<bool> {
        self.ensure_confirmed()
    }

    fn match_cost(&self) -> Option<f32> {
        self.required
            .match_cost()
            .into_iter()
            .chain(self.optional.match_cost())
            .reduce(|left, right| left + right)
    }

    fn scores_non_negative(&self) -> bool {
        true
    }

    /// Batch profile (PLAN-V3 P2): when both sides are flat single-posting
    /// providers and the static skew criterion does not yield, the drain is
    /// driven by the IuBatchState FSM instead of the per-document walk.
    ///
    /// I-6: the entry runs the same decision hook as the default.
    /// EXT R5: a non-positive floor keeps the collector's per-document
    /// branch in charge, so the batch never engages there.
    /// I-2 option (a): once the batch owns the scorer it runs to exhaustion;
    /// a promotion latch recorded mid-run is terminal bookkeeping, not an
    /// engine switch.
    fn collect_confirmed_window(
        &mut self,
        min_score: f32,
        out: &mut Vec<ConfirmedHit>,
        max_hits: usize,
    ) -> Result<WindowCollect> {
        self.set_min_competitive_score(min_score)?;
        if self.exhausted {
            return Ok(WindowCollect { exhausted: true });
        }
        if self.batch.is_none()
            && min_score > 0.0
            && !self.optional_globally_required
            && let Some(state) = self.batch_engagement()
        {
            #[cfg(test)]
            BATCH_ENGAGEMENTS.with(|count| count.set(count.get() + 1));
            self.batch = Some(state);
        }
        if self.batch.is_some() {
            let Self {
                required,
                optional,
                exhausted,
                current,
                batch,
                optional_globally_required,
                batch_hits,
                ..
            } = self;
            // I-2 option (a): a promotion latch recorded mid-run (the floor
            // crossing the required upper bound while the batch is engaged)
            // is terminal bookkeeping. The batch keeps full-scanning — sound
            // (nothing is dropped), just less pruning. The latch would only
            // re-route a scorer that has not engaged the batch yet.
            #[cfg(test)]
            BATCH_DRAINS.with(|count| count.set(count.get() + 1));
            batch_hits.clear();
            let exhausted_now = match batch.as_mut() {
                Some(IuBatchEngagement::Single(state)) => {
                    let Some(required_batch) = required.block_batch() else {
                        return Err(Error::internal(
                            "IU batch profile lost its required block provider",
                        ));
                    };
                    let Some(mut optional_batch) = optional.block_batch_children() else {
                        return Err(Error::internal(
                            "IU batch profile lost its optional block providers",
                        ));
                    };
                    state.next_batch(
                        required_batch,
                        optional_batch.as_mut_slice(),
                        min_score,
                        batch_hits,
                        max_hits,
                    )
                }
                Some(IuBatchEngagement::Multi(state)) => {
                    let Some(mut requireds) = required.block_batch_children() else {
                        return Err(Error::internal(
                            "IU batch profile lost its required block providers",
                        ));
                    };
                    let Some(mut optionals) = optional.block_batch_children() else {
                        return Err(Error::internal(
                            "IU batch profile lost its optional block providers",
                        ));
                    };
                    state.next_batch(
                        &mut requireds,
                        &mut optionals,
                        min_score,
                        batch_hits,
                        max_hits,
                    )
                }
                None => return Err(Error::internal("IU batch profile vanished while engaged")),
            };
            for &(document_key, score) in batch_hits.iter() {
                out.push(ConfirmedHit { document_key, score });
            }
            if exhausted_now {
                *exhausted = true;
                *current = None;
                return Ok(WindowCollect { exhausted: true });
            }
            return Ok(WindowCollect { exhausted: false });
        }
        collect_confirmed_window_default(self, min_score, out, max_hits)
    }

    #[cfg(test)]
    fn debug_child_type_names(&self) -> Vec<&'static str> {
        vec![
            self.required.debug_type_name(),
            self.optional.debug_type_name(),
        ]
    }
}
