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
use super::{PostingIterator, TERMINATED_DOC_ID, WandDocuments, exact_bm25_addend_slab};

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
    exact_addends: Option<&'a [f32]>,
}

impl<'a, D: WandDocuments, S: Scorer> IuTightScoring<'a, D, S> {
    fn term_score(&self, posting: &PostingIterator, doc: u64, freq: u32) -> f32 {
        match self.exact_addends {
            Some(addends) => {
                posting.query_weight * bm25_doc_weight_with_norm(freq, addends[doc as usize])
            }
            None => posting.score(
                self.scorer,
                freq,
                self.documents.scoring_num_tokens(doc as u32),
            ),
        }
    }

    fn complete_shoulds(&self, shoulds: &mut [PostingIterator], doc: u64) -> f32 {
        let mut should_sum = 0.0_f32;
        for should in shoulds.iter_mut() {
            if should.doc().is_some_and(|info| info.doc_id() < doc) {
                should.next(doc);
            }
            if let Some(info) = should.doc()
                && info.doc_id() == doc
            {
                should_sum += self.term_score(should, doc, info.frequency());
            }
        }
        should_sum
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
    let should_ub_sum: f32 = should_ubs.iter().copied().sum();
    let mut chunk = Vec::with_capacity(128);
    let mut last_doc = 0_u64;
    loop {
        must.take_docs_one_block_upto(TERMINATED_DOC_ID, &mut chunk);
        if chunk.is_empty() {
            break;
        }
        for &(doc, freq) in &chunk {
            last_doc = doc;
            let Some(key) = scoring.documents.document_key_for_doc_id(doc as u32) else {
                continue;
            };
            let must_score = scoring.term_score(&must, doc, freq);
            let current_floor = floor();
            if current_floor.is_finite() && must_score + should_ub_sum < current_floor {
                continue;
            }
            let score = must_score + scoring.complete_shoulds(&mut shoulds, doc);
            if !on_hit(key, score)? {
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
                return scoring.promoted(
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
    let sum_shoulds: f32 = should_ubs.iter().sum();
    if must_ub + sum_shoulds < floor {
        return Some(Vec::new());
    }
    let required: Vec<usize> = should_ubs
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let others: f32 = should_ubs
                .iter()
                .enumerate()
                .filter(|(other, _)| other != index)
                .map(|(_, upper)| *upper)
                .sum();
            must_ub + others < floor
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
        must: PostingIterator,
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
