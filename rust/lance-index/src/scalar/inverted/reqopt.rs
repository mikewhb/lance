// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! `ReqOptScorer`: required-plus-optional scoring for the IU shape.
//!
//! Extracted verbatim from `compound.rs` (PLAN-V3 P1a, zero behavior
//! change). Declared as a child module of `compound` so it can reach the
//! shared scorer-tree internals via `super::*` while keeping compound.rs
//! small.

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
        }
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

    #[cfg(test)]
    fn debug_child_type_names(&self) -> Vec<&'static str> {
        vec![
            self.required.debug_type_name(),
            self.optional.debug_type_name(),
        ]
    }
}
