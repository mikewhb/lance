// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Seed a MAXSCORE exclusive floor from the sparsest disjunction clause.
//!
//! One clause's contribution is a lower bound on the score of any document
//! that contains it, so the k-th largest such contribution is a floor no
//! document outside the top k can reach. That property does not hold for
//! conjunction: a document on the rare list may still fail the intersection.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use arrow_array::Array;

use super::super::LEGACY_BLOCK_SIZE;
use super::super::encoding::{
    MAX_POSTING_BLOCK_SIZE, decompress_posting_block, decompress_posting_remainder,
};
use super::super::scorer::{Scorer, bm25_doc_weight_with_norm};
use super::{PostingIterator, PostingList, WandDocuments};

/// Relative gate: skip seeding when the sparsest clause is more than this
/// fraction of the query's total posting length. Equal-length OR queries
/// reach a competitive floor on their own, so the pass would only decode
/// work the walk already does.
pub(super) const SEED_FLOOR_MAX_COST_SHARE: usize = 16;

/// Absolute decode budget. A sixteenth of a stopword list is still long
/// enough to cost more than the walk it saves (`the movement` is ~118k
/// postings). Every query this pass helps on Wikipedia seeds from under a
/// thousand postings.
pub(super) const SEED_FLOOR_MAX_POSTINGS: usize = 2048;

/// Score the sparsest clause's postings and, if it can fill top-k, return
/// one ULP below its k-th contribution as an exclusive floor.
///
/// The heap that normally publishes a floor already holds the k documents
/// that reach it, so pruning at exactly that score is safe there. Here the
/// heap is still empty: an exclusive comparison against an exact tie would
/// prune every document that ties for k-th.
pub(super) fn seed_floor_from_sparsest_clause<'a, S, D, I>(
    postings: I,
    limit: usize,
    scorer: &S,
    documents: &D,
    norm_k_ref: Option<(&'a [u8], &'a [f32; 256])>,
) -> Option<f32>
where
    S: Scorer,
    D: WandDocuments,
    I: IntoIterator<Item = &'a PostingIterator>,
{
    if limit == 0 || limit == usize::MAX {
        return None;
    }

    let postings: Vec<&PostingIterator> = postings.into_iter().collect();
    if postings.len() < 2 {
        return None;
    }

    let sparsest = postings.iter().min_by_key(|posting| posting.cost())?;
    let sparse_cost = sparsest.cost();
    let total_cost = postings.iter().map(|posting| posting.cost()).sum::<usize>();
    // Fewer than k documents cannot define a k-th best score, and the pass
    // only pays for its own decode when it is both a small slice of the
    // walk it stands to cut and short in absolute terms.
    if sparse_cost < limit
        || sparse_cost > SEED_FLOOR_MAX_POSTINGS
        || sparse_cost.saturating_mul(SEED_FLOOR_MAX_COST_SHARE) > total_cost
    {
        return None;
    }

    let PostingList::Compressed(list) = &sparsest.list else {
        return None;
    };
    if list.block_size != LEGACY_BLOCK_SIZE && list.block_size != MAX_POSTING_BLOCK_SIZE {
        return None;
    }

    let query_weight = sparsest.query_weight;
    let num_blocks = list.blocks.len();
    let remainder = list.length as usize % list.block_size;
    // Decode straight off the blocks rather than through a posting
    // iterator: this pass wants only (doc, freq), and the iterator
    // allocates a positions cursor per document.
    let mut buffer = [0u32; MAX_POSTING_BLOCK_SIZE];
    let mut doc_ids = Vec::with_capacity(list.block_size);
    let mut freqs = Vec::with_capacity(list.block_size);
    // Min-heap of the k largest contributions. BM25 weights are
    // non-negative, so their bit patterns order the same way.
    let mut best = BinaryHeap::with_capacity(limit);
    for block_idx in 0..num_blocks {
        let block = list.blocks.value(block_idx);
        doc_ids.clear();
        freqs.clear();
        if block_idx + 1 == num_blocks && remainder != 0 {
            decompress_posting_remainder(
                block,
                remainder,
                list.posting_tail_codec,
                list.block_size,
                &mut doc_ids,
                &mut freqs,
            );
        } else {
            decompress_posting_block(
                block,
                &mut buffer,
                &mut doc_ids,
                &mut freqs,
                list.block_size,
            );
        }
        for (&doc, &freq) in doc_ids.iter().zip(freqs.iter()) {
            // The walk drops deleted / filtered rows before insert. Counting
            // them here would put the exclusive floor above the live k-th.
            if documents.document_key_for_doc_id(doc).is_none() {
                continue;
            }
            let score = match norm_k_ref {
                Some((norms, cache)) => {
                    let Some(&code) = norms.get(doc as usize) else {
                        continue;
                    };
                    query_weight * bm25_doc_weight_with_norm(freq, cache[code as usize])
                }
                None => query_weight * scorer.doc_weight(freq, documents.scoring_num_tokens(doc)),
            };
            if score <= 0.0 || !score.is_finite() {
                continue;
            }
            let ranked = Reverse(score.to_bits());
            if best.len() < limit {
                best.push(ranked);
            } else if best.peek().is_some_and(|worst| ranked < *worst) {
                best.pop();
                best.push(ranked);
            }
        }
    }
    if best.len() != limit {
        return None;
    }
    let Reverse(kth) = best.peek().copied()?;
    let floor = f32::from_bits(kth.saturating_sub(1));
    (floor > 0.0 && floor.is_finite()).then_some(floor)
}
