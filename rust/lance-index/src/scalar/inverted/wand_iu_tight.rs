// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! MUST-driven Boolean IU for one term MUST plus N SHOULD clauses.
//!
//! Walk MUST until the heap floor proves some SHOULD is individually
//! required (`max(MUST) + Σ_{j≠i} max(SHOULD_j) < floor`), then switch the
//! lead to the cheapest such list. Promotion uses list-wide bounds only.
//! The optional union is never made required as a whole (that is 5a's
//! leapfrog regression).

use lance_core::Result;

use super::super::scorer::{Scorer, bm25_doc_weight_with_norm};
use super::{
    ExactBm25Addends, PostingIterator, TERMINATED_DOC_ID, WandDocuments, exact_bm25_addend_slab,
    outward_f32_upper_bound,
};

/// Conservative sum of upper bounds so f32 rounding cannot shrink below the
/// true total and trigger premature IU termination.
fn conservative_ub_sum(values: impl IntoIterator<Item = f32>) -> f32 {
    let sum: f64 = values.into_iter().map(f64::from).sum();
    outward_f32_upper_bound(sum)
}

fn conservative_ub_pair_sum(left: f32, right: f32) -> f32 {
    outward_f32_upper_bound(f64::from(left) + f64::from(right))
}

/// Promote only when the new lead is at least this many times cheaper than
/// MUST. A medium-df SHOULD that just barely becomes required is slower to
/// walk than finishing MUST.
pub const IU_TIGHT_LEAD_COST_RATIO: usize = 3;

pub fn iu_tight_lead_is_cheaper(must_cost: usize, lead_cost: usize) -> bool {
    lead_cost.saturating_mul(IU_TIGHT_LEAD_COST_RATIO) < must_cost
}

/// Everything the kernel needs to turn `(doc, freq)` into a score. Bundled as
/// one `Copy` value so the hot helpers stay plain free functions (inlinable,
/// no trait dispatch) without tripping the argument-count lint.
struct IuTightScoring<'a, D, S> {
    documents: &'a D,
    scorer: &'a S,
    exact_addends: Option<ExactBm25Addends<'a>>,
}

// Hand-written: `derive(Copy)` would add `D: Copy, S: Copy` bounds, and the
// scorer is not `Copy`. Every field is a reference or a `Copy` view.
impl<D, S> Clone for IuTightScoring<'_, D, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D, S> Copy for IuTightScoring<'_, D, S> {}

fn iu_tight_term_score<D, S>(
    scoring: IuTightScoring<'_, D, S>,
    posting: &PostingIterator,
    doc: u64,
    freq: u32,
) -> f32
where
    D: WandDocuments,
    S: Scorer,
{
    match scoring.exact_addends {
        Some(addends) => {
            posting.query_weight * bm25_doc_weight_with_norm(freq, addends.get(doc as u32))
        }
        None => posting.score(
            scoring.scorer,
            freq,
            scoring.documents.scoring_num_tokens(doc as u32),
        ),
    }
}

fn iu_tight_complete_shoulds<D, S>(
    scoring: IuTightScoring<'_, D, S>,
    shoulds: &mut [PostingIterator],
    doc: u64,
) -> f32
where
    D: WandDocuments,
    S: Scorer,
{
    let mut should_sum = 0.0_f32;
    for should in shoulds.iter_mut() {
        if should.doc().is_some_and(|info| info.doc_id() < doc) {
            should.next(doc);
        }
        if let Some(info) = should.doc()
            && info.doc_id() == doc
        {
            should_sum += iu_tight_term_score(scoring, should, doc, info.frequency());
        }
    }
    should_sum
}

/// Score one MUST document and hand it to `on_hit`. The whole per-document
/// body lives here so the caller's loop stays inside the inliner's budget;
/// splitting the SHOULD fold into its own out-of-line call costs a call per
/// candidate document and shows up as ~7% of the IU profile.
fn iu_tight_emit_must_doc<D, S>(
    scoring: IuTightScoring<'_, D, S>,
    must: &PostingIterator,
    shoulds: &mut [PostingIterator],
    doc: u64,
    freq: u32,
    on_hit: &mut dyn FnMut(u64, f32) -> Result<bool>,
) -> Result<bool>
where
    D: WandDocuments,
    S: Scorer,
{
    let Some(key) = scoring.documents.document_key_for_doc_id(doc as u32) else {
        return Ok(true);
    };
    let score = iu_tight_term_score(scoring, must, doc, freq)
        + iu_tight_complete_shoulds(scoring, shoulds, doc);
    on_hit(key, score)
}

/// Walk every MUST doc, fold `must + sum(matching shoulds)`, and promote
/// to a sparse required SHOULD once the floor makes that list mandatory.
///
/// There is deliberately no per-document floor pre-check: the collector
/// already rejects anything below the competitive floor, and the extra
/// conservative f32 sum per candidate costs more than the SHOULD folds it
/// skips (measured: the analysis tree is ~17% faster on SBG IU without it).
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
    let scoring = IuTightScoring {
        documents,
        scorer,
        exact_addends: exact_bm25_addend_slab(scorer, documents),
    };
    let must_ub = must.global_upper_bound(scorer);
    let should_ubs: Vec<f32> = shoulds
        .iter()
        .map(|should| should.global_upper_bound(scorer))
        .collect();
    let mut chunk = Vec::with_capacity(128);
    let mut last_doc = 0_u64;
    loop {
        must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut chunk);
        if chunk.is_empty() {
            break;
        }
        for &(doc, freq) in &chunk {
            last_doc = doc;
            let keep = iu_tight_emit_must_doc(scoring, &must, &mut shoulds, doc, freq, on_hit)?;
            if !keep {
                return Ok(());
            }
        }
        let current_floor = floor();
        if current_floor.is_finite()
            && let Some(required) = iu_tight_required_shoulds(must_ub, &should_ubs, current_floor)
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
                return iu_tight_promoted(
                    scoring,
                    must,
                    shoulds,
                    required,
                    last_doc.saturating_add(1),
                    on_hit,
                );
            }
        }
    }
    Ok(())
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
    let sum_shoulds = conservative_ub_sum(should_ubs.iter().copied());
    if conservative_ub_pair_sum(must_ub, sum_shoulds) < floor {
        return Some(Vec::new());
    }
    let required: Vec<usize> = should_ubs
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let others = conservative_ub_sum(
                should_ubs
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| other != index)
                    .map(|(_, upper)| *upper),
            );
            conservative_ub_pair_sum(must_ub, others) < floor
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

fn iu_tight_promoted<D, S>(
    scoring: IuTightScoring<'_, D, S>,
    must: PostingIterator,
    mut shoulds: Vec<PostingIterator>,
    required: Vec<usize>,
    start_doc: u64,
    on_hit: &mut dyn FnMut(u64, f32) -> Result<bool>,
) -> Result<()>
where
    D: WandDocuments,
    S: Scorer,
{
    let Some(lead) = required
        .iter()
        .copied()
        .min_by_key(|&index| shoulds[index].cost())
    else {
        return Ok(());
    };
    // `take_docs_one_block_upto` already consumed the current MUST block, so
    // the live MUST cursor may sit past `start_doc`. Fork and seek instead.
    let mut must = must.fork_from_start();
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
            let Some(key) = scoring.documents.document_key_for_doc_id(doc as u32) else {
                continue;
            };
            let must_score = iu_tight_term_score(scoring, &must, doc, must_freq);
            let mut should_sum = 0.0_f32;
            for (index, should) in shoulds.iter_mut().enumerate() {
                let freq = if index == lead {
                    Some(lead_freq)
                } else {
                    iu_tight_seek_eq(should, doc)
                };
                if let Some(freq) = freq {
                    should_sum += iu_tight_term_score(scoring, should, doc, freq);
                }
            }
            if !on_hit(key, must_score + should_sum)? {
                return Ok(());
            }
        }
    }
    Ok(())
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
        // leftover IU: MUST df is close to the cheapest SHOULD (airport ~56k vs
        // security ~77k). The 3x lead-cost gate must not switch kernels.
        assert!(!iu_tight_lead_is_cheaper(56_000, 77_000));
        assert!(!iu_tight_lead_is_cheaper(56_000, 56_000));
    }
}
