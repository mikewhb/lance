// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Window merge kernels for the bulk conjunction route.
//!
//! A window covers a doc range in which every conjunct clause contributes
//! exactly one decompressed posting block. [`merge_window`] intersects those
//! blocks and emits each matching doc id together with the per-clause
//! block-local offsets the caller needs to reach the matching frequencies.
//!
//! The merge kernels are pure functions over immutable window slices. They
//! never advance a posting cursor, never read or update `threshold`, and
//! never touch phrase or terminal state: the caller materializes the window,
//! decides whether a positive floor exists, and does all scoring. The only
//! floor-derived input is a caller-owned frequency LUT.
//!
//! The window steps around them ([`and_block_bound`], [`slice_and_window`],
//! [`load_window_freqs`]) move only the clauses' block cursors and take the
//! floor as an argument, so any conjunction cursor can drive them.
use arrow_array::Array;

use super::super::scorer::{Scorer, bm25_doc_weight_with_norm};
#[cfg(target_arch = "x86_64")]
use super::find_next_geq_avx2;
use super::{
    FREQ_LUT_BUCKETS, PostingIterator, PostingList, TERMINATED_DOC_ID, conservative_score_sum,
    find_next_geq, find_next_geq_scalar, intersection,
};
use smallvec::SmallVec;

/// Per-window view of one clause's current block. Raw pointers into the
/// clause's `CompressedState`; valid for the whole window because the block
/// cursor does not move within a window (position decoding writes to separate
/// fields of the same state).
pub(super) struct WindowList {
    pub(super) docs: *const u32,
    pub(super) freqs: *const u32,
    // Cursor and exclusive end, as offsets within the block.
    pub(super) pos: usize,
    pub(super) end: usize,
    // Absolute posting index of the block's first entry.
    pub(super) block_start: usize,
}

/// Sum one bulk-AND candidate's per-clause BM25 contributions in score order.
/// `wins` and `offs` are indexed by clause; the caller may run this before or
/// after position confirmation.
#[inline]
pub(super) fn window_bm25_score<S: Scorer + ?Sized>(
    lead: &[Box<PostingIterator>],
    wins: &[WindowList],
    offs: &[u8],
    score_order: &[usize],
    scorer: &S,
    norm_addend: Option<f32>,
    doc_length: u32,
) -> f32 {
    let mut score = 0.0_f32;
    for &clause_index in score_order {
        let win = &wins[clause_index];
        let posting = &lead[clause_index];
        let off = offs[clause_index];
        let freq = unsafe { *win.freqs.add(off as usize) };
        score += match norm_addend {
            Some(addend) => posting.query_weight * bm25_doc_weight_with_norm(freq, addend),
            None => posting.score(scorer, freq, doc_length),
        };
    }
    score
}

// Merge kernels: intersect the window's per-clause slices and record
// each match as (doc, per-clause block offsets). Hand-specialized for
// two and three clauses so cursors and bounds stay in registers; the
// generic kernel covers other widths. Offsets fit u8 because a block
// holds at most 256 (`MAX_POSTING_BLOCK_SIZE`) entries. The macro
// stamps a scalar and an AVX2 variant — `#[target_feature]` must
// cover the whole kernel for the vector catch-up scans to inline.
// Kernels prune a lead doc before any follower advance when its
// frequency-bucketed score bound (plus the other clauses' block
// maxes) cannot beat the threshold — the score-first ordering
// Lucene's conjunction scorer uses, with doc length dropped from the
// bound so no doc-length load is involved. The bound is monotone
// (doc length only shrinks a BM25 weight, and the last bucket holds
// the clause sup), so every skipped doc would also fail the exact
// per-candidate prune: results are unchanged, the work never happens.
macro_rules! merge_kernels {
    ($name2:ident, $name3:ident, $name_n:ident, $docs_name2:ident, $docs_name3:ident, $geq:ident $(, #[$feat:meta])?) => {
        $(#[$feat])?
        unsafe fn $name2(
            wins: &[WindowList],
            freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
            docs_out: &mut Vec<u32>,
            offs_out: &mut Vec<u8>,
        ) {
            let (d0, mut p0, e0) = (wins[0].docs, wins[0].pos, wins[0].end);
            let (d1, mut p1, e1) = (wins[1].docs, wins[1].pos, wins[1].end);
            let f0 = wins[0].freqs;
            unsafe {
                while p0 < e0 {
                    let doc = *d0.add(p0);
                    let freq = (*f0.add(p0) as usize).min(FREQ_LUT_BUCKETS - 1);
                    if freq_cannot_beat[freq] {
                        p0 += 1;
                        continue;
                    }
                    p1 = $geq(d1, p1, e1, doc);
                    if p1 >= e1 {
                        return;
                    }
                    let second = *d1.add(p1);
                    if second > doc {
                        p0 = $geq(d0, p0 + 1, e0, second);
                        continue;
                    }
                    docs_out.push(doc);
                    offs_out.push(p0 as u8);
                    offs_out.push(p1 as u8);
                    p0 += 1;
                }
            }
        }

        $(#[$feat])?
        unsafe fn $name3(
            wins: &[WindowList],
            freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
            docs_out: &mut Vec<u32>,
            offs_out: &mut Vec<u8>,
        ) {
            let (d0, mut p0, e0) = (wins[0].docs, wins[0].pos, wins[0].end);
            let (d1, mut p1, e1) = (wins[1].docs, wins[1].pos, wins[1].end);
            let (d2, mut p2, e2) = (wins[2].docs, wins[2].pos, wins[2].end);
            let f0 = wins[0].freqs;
            unsafe {
                'outer: while p0 < e0 {
                    let doc = *d0.add(p0);
                    let freq = (*f0.add(p0) as usize).min(FREQ_LUT_BUCKETS - 1);
                    if freq_cannot_beat[freq] {
                        p0 += 1;
                        continue 'outer;
                    }
                    p1 = $geq(d1, p1, e1, doc);
                    if p1 >= e1 {
                        return;
                    }
                    let second = *d1.add(p1);
                    if second > doc {
                        p0 = $geq(d0, p0 + 1, e0, second);
                        continue 'outer;
                    }
                    p2 = $geq(d2, p2, e2, doc);
                    if p2 >= e2 {
                        return;
                    }
                    let third = *d2.add(p2);
                    if third > doc {
                        p0 = $geq(d0, p0 + 1, e0, third);
                        continue 'outer;
                    }
                    docs_out.push(doc);
                    offs_out.push(p0 as u8);
                    offs_out.push(p1 as u8);
                    offs_out.push(p2 as u8);
                    p0 += 1;
                }
            }
        }

        $(#[$feat])?
        #[allow(clippy::too_many_arguments)]
        unsafe fn $name_n(
            wins: &[WindowList],
            freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
            cursors: &mut Vec<usize>,
            docs_out: &mut Vec<u32>,
            offs_out: &mut Vec<u8>,
        ) {
            cursors.clear();
            cursors.extend(wins.iter().map(|win| win.pos));
            let (d0, mut p0, e0) = (wins[0].docs, wins[0].pos, wins[0].end);
            let f0 = wins[0].freqs;
            unsafe {
                'outer: while p0 < e0 {
                    let doc = *d0.add(p0);
                    let freq = (*f0.add(p0) as usize).min(FREQ_LUT_BUCKETS - 1);
                    if freq_cannot_beat[freq] {
                        p0 += 1;
                        continue 'outer;
                    }
                    for j in 1..wins.len() {
                        let win = wins.get_unchecked(j);
                        let pos = $geq(win.docs, *cursors.get_unchecked(j), win.end, doc);
                        *cursors.get_unchecked_mut(j) = pos;
                        if pos >= win.end {
                            return;
                        }
                        let clause_doc = *win.docs.add(pos);
                        if clause_doc > doc {
                            p0 = $geq(d0, p0 + 1, e0, clause_doc);
                            continue 'outer;
                        }
                    }
                    docs_out.push(doc);
                    offs_out.push(p0 as u8);
                    for j in 1..wins.len() {
                        offs_out.push(*cursors.get_unchecked(j) as u8);
                    }
                    p0 += 1;
                }
            }
        }

        $(#[$feat])?
        unsafe fn $docs_name2(
            wins: &[WindowList],
            docs_out: &mut Vec<u32>,
            offs_out: &mut Vec<u8>,
        ) {
            let (d0, mut p0, e0) = (wins[0].docs, wins[0].pos, wins[0].end);
            let (d1, mut p1, e1) = (wins[1].docs, wins[1].pos, wins[1].end);
            unsafe {
                while p0 < e0 {
                    let doc = *d0.add(p0);
                    p1 = $geq(d1, p1, e1, doc);
                    if p1 >= e1 {
                        return;
                    }
                    let second = *d1.add(p1);
                    if second > doc {
                        p0 = $geq(d0, p0 + 1, e0, second);
                        continue;
                    }
                    docs_out.push(doc);
                    offs_out.push(p0 as u8);
                    offs_out.push(p1 as u8);
                    p0 += 1;
                }
            }
        }

        $(#[$feat])?
        unsafe fn $docs_name3(
            wins: &[WindowList],
            docs_out: &mut Vec<u32>,
            offs_out: &mut Vec<u8>,
        ) {
            let (d0, mut p0, e0) = (wins[0].docs, wins[0].pos, wins[0].end);
            let (d1, mut p1, e1) = (wins[1].docs, wins[1].pos, wins[1].end);
            let (d2, mut p2, e2) = (wins[2].docs, wins[2].pos, wins[2].end);
            unsafe {
                'outer: while p0 < e0 {
                    let doc = *d0.add(p0);
                    p1 = $geq(d1, p1, e1, doc);
                    if p1 >= e1 {
                        return;
                    }
                    let second = *d1.add(p1);
                    if second > doc {
                        p0 = $geq(d0, p0 + 1, e0, second);
                        continue 'outer;
                    }
                    p2 = $geq(d2, p2, e2, doc);
                    if p2 >= e2 {
                        return;
                    }
                    let third = *d2.add(p2);
                    if third > doc {
                        p0 = $geq(d0, p0 + 1, e0, third);
                        continue 'outer;
                    }
                    docs_out.push(doc);
                    offs_out.push(p0 as u8);
                    offs_out.push(p1 as u8);
                    offs_out.push(p2 as u8);
                    p0 += 1;
                }
            }
        }
    };
}
merge_kernels!(
    merge_window_2,
    merge_window_3,
    merge_window_n,
    merge_window_docs_2,
    merge_window_docs_3,
    find_next_geq_scalar
);
#[cfg(target_arch = "x86_64")]
merge_kernels!(
    merge_window_2_avx2,
    merge_window_3_avx2,
    merge_window_n_avx2,
    merge_window_docs_2_avx2,
    merge_window_docs_3_avx2,
    find_next_geq_avx2,
    #[target_feature(enable = "avx2")]
);

#[inline]
fn merge_window_1(wins: &[WindowList], docs_out: &mut Vec<u32>, offs_out: &mut Vec<u8>) {
    let win = &wins[0];
    for pos in win.pos..win.end {
        docs_out.push(unsafe { *win.docs.add(pos) });
        offs_out.push(pos as u8);
    }
}

#[inline]
fn merge_window_docs_n(
    wins: &[WindowList],
    cursors: &mut Vec<usize>,
    docs_out: &mut Vec<u32>,
    offs_out: &mut Vec<u8>,
) {
    cursors.clear();
    cursors.extend(wins.iter().map(|win| win.pos));
    'outer: while cursors[0] < wins[0].end {
        let doc = unsafe { *wins[0].docs.add(cursors[0]) };
        for j in 1..wins.len() {
            let win = &wins[j];
            let pos = unsafe { find_next_geq(win.docs, cursors[j], win.end, doc) };
            cursors[j] = pos;
            if pos >= win.end {
                return;
            }
            let clause_doc = unsafe { *win.docs.add(pos) };
            if clause_doc > doc {
                cursors[0] =
                    unsafe { find_next_geq(wins[0].docs, cursors[0] + 1, wins[0].end, clause_doc) };
                continue 'outer;
            }
        }
        docs_out.push(doc);
        for &pos in cursors.iter() {
            offs_out.push(pos as u8);
        }
        cursors[0] += 1;
    }
}

#[inline]
fn merge_window_docs_pairwise(
    wins: &[WindowList],
    scratch: &mut Vec<u32>,
    docs_out: &mut Vec<u32>,
    offs_out: &mut Vec<u8>,
) {
    // Every slice belongs to one decoded block and has at most 256
    // entries. Shrink candidates clause by clause, so later clauses
    // only intersect documents that survived the earlier clauses.
    let first = &wins[0];
    // SAFETY: window construction keeps these decoded blocks alive
    // and unchanged until both intersection and scoring complete.
    docs_out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(first.docs.add(first.pos), first.end - first.pos)
    });
    for win in &wins[1..] {
        let slice = unsafe { std::slice::from_raw_parts(win.docs.add(win.pos), win.end - win.pos) };
        let first_doc = slice[0];
        let last_doc = slice[slice.len() - 1];
        // Sorted distinct IDs with this span contain every integer
        // in the interval. This exact certificate also handles a
        // dense follower covering a sparse set of candidates.
        if u64::from(last_doc) - u64::from(first_doc) + 1 == slice.len() as u64 {
            if docs_out[0] >= first_doc && docs_out[docs_out.len() - 1] <= last_doc {
                continue;
            }
            let lo = docs_out.partition_point(|&doc| doc < first_doc);
            let hi = docs_out.partition_point(|&doc| doc <= last_doc);
            if lo != 0 {
                docs_out.copy_within(lo..hi, 0);
            }
            docs_out.truncate(hi - lo);
        } else {
            intersection::intersect(docs_out, slice, scratch);
            std::mem::swap(docs_out, scratch);
        }
        if docs_out.is_empty() {
            return;
        }
    }

    // Recover posting offsets only for the final intersection. No
    // frequencies or intermediate per-clause offset tables are needed.
    offs_out.resize(docs_out.len() * wins.len(), 0);
    for (clause, win) in wins.iter().enumerate() {
        let first_doc = unsafe { *win.docs.add(win.pos) };
        let last_doc = unsafe { *win.docs.add(win.end - 1) };
        let is_dense = u64::from(last_doc) - u64::from(first_doc) + 1 == (win.end - win.pos) as u64;
        let mut pos = win.pos;
        for (candidate, &doc) in docs_out.iter().enumerate() {
            pos = if is_dense {
                win.pos + (doc - first_doc) as usize
            } else {
                unsafe { find_next_geq(win.docs, pos, win.end, doc) }
            };
            debug_assert!(
                pos < win.end,
                "intersection must be present in every clause"
            );
            offs_out[candidate * wins.len() + clause] = pos as u8;
        }
    }
}

/// Intersect one window's per-clause blocks.
///
/// `docs_out` receives the matching doc ids in ascending order and `offs_out`
/// `num_lists` block-local offsets per match, in clause order. The two scratch
/// vectors are reused across windows and carry no state between calls.
///
/// `freq_cannot_beat` is only consulted when a positive floor was in effect
/// when the window started and the conjunction has at least two clauses: it
/// marks frequency buckets whose lead contribution cannot reach the floor, so
/// those lead docs are skipped before any follower advance. The last bucket is
/// the frequency-independent supremum, so clamping a frequency into the table
/// stays a valid upper bound.
///
/// `use_pairwise` selects a different intersection strategy for wide
/// conjunctions; it only applies without a floor, where no frequency pruning
/// happens.
#[allow(clippy::too_many_arguments)]
#[inline]
pub(super) fn merge_window(
    wins: &[WindowList],
    num_lists: usize,
    use_avx2: bool,
    use_pairwise: bool,
    window_started_with_floor: bool,
    freq_cannot_beat: &[bool; FREQ_LUT_BUCKETS],
    docs_out: &mut Vec<u32>,
    offs_out: &mut Vec<u8>,
    cursor_scratch: &mut Vec<usize>,
    intersection_scratch: &mut Vec<u32>,
) {
    debug_assert_eq!(num_lists, wins.len());
    // The pruning kernels inspect `wins[0]`; the caller must put the clause
    // that produced `freq_cannot_beat` first.
    match (num_lists, use_avx2, window_started_with_floor) {
        (1, _, _) => merge_window_1(wins, docs_out, offs_out),
        #[cfg(target_arch = "x86_64")]
        (2, true, false) => unsafe { merge_window_docs_2_avx2(wins, docs_out, offs_out) },
        #[cfg(target_arch = "x86_64")]
        (3, true, false) => unsafe { merge_window_docs_3_avx2(wins, docs_out, offs_out) },
        (2, _, false) => unsafe { merge_window_docs_2(wins, docs_out, offs_out) },
        (3, _, false) => unsafe { merge_window_docs_3(wins, docs_out, offs_out) },
        (_, _, false) if use_pairwise => {
            merge_window_docs_pairwise(wins, intersection_scratch, docs_out, offs_out)
        }
        (_, _, false) => merge_window_docs_n(wins, cursor_scratch, docs_out, offs_out),
        #[cfg(target_arch = "x86_64")]
        (2, true, true) => unsafe {
            merge_window_2_avx2(wins, freq_cannot_beat, docs_out, offs_out)
        },
        #[cfg(target_arch = "x86_64")]
        (3, true, true) => unsafe {
            merge_window_3_avx2(wins, freq_cannot_beat, docs_out, offs_out)
        },
        (2, _, true) => unsafe { merge_window_2(wins, freq_cannot_beat, docs_out, offs_out) },
        (3, _, true) => unsafe { merge_window_3(wins, freq_cannot_beat, docs_out, offs_out) },
        #[cfg(target_arch = "x86_64")]
        (_, true, true) => unsafe {
            merge_window_n_avx2(wins, freq_cannot_beat, cursor_scratch, docs_out, offs_out)
        },
        (_, _, true) => unsafe {
            merge_window_n(wins, freq_cannot_beat, cursor_scratch, docs_out, offs_out)
        },
    }
}

/// End of `posting`'s current block, at least `target`.
pub(super) fn posting_block_up_to(posting: &PostingIterator, target: u64) -> u64 {
    posting
        .next_block_first_doc()
        .map(|doc| doc.saturating_sub(1))
        .unwrap_or(TERMINATED_DOC_ID)
        .max(target)
}

/// Block bound of a conjunction from some target.
pub(super) struct AndBlockBound {
    pub(super) up_to: u64,
    pub(super) max_score: f32,
    /// The bound spans the lead's whole block rather than the nearest block
    /// end of any clause.
    pub(super) wide: bool,
    #[cfg(test)]
    pub(super) range_blocks_scanned: usize,
}

/// Moves every clause's block cursor to `target` and bounds the conjunction
/// from there: up to the nearest block end, or, when that window cannot reach
/// `floor`, up to the lead's block end if that wider window cannot either.
/// `lead[0]` is the lead clause.
pub(super) fn and_block_bound<S: Scorer + ?Sized>(
    lead: &mut [Box<PostingIterator>],
    scorer: &S,
    target: u64,
    floor: f32,
) -> AndBlockBound {
    for posting in lead.iter_mut() {
        posting.shallow_next(target);
    }
    let narrow_up_to = lead
        .iter()
        .map(|posting| posting_block_up_to(posting, target))
        .min()
        .unwrap_or(TERMINATED_DOC_ID);
    let narrow_max_score =
        conservative_score_sum(lead.iter().map(|posting| posting.block_max_score(scorer)));
    let mut bound = AndBlockBound {
        up_to: narrow_up_to,
        max_score: narrow_max_score,
        wide: false,
        #[cfg(test)]
        range_blocks_scanned: 0,
    };
    if narrow_max_score >= floor {
        return bound;
    }
    let lead_up_to = lead
        .first()
        .map(|posting| posting_block_up_to(posting, target))
        .unwrap_or(TERMINATED_DOC_ID);
    let can_try_wide = lead_up_to > narrow_up_to
        && lead_up_to != TERMINATED_DOC_ID
        && lead.iter().all(|posting| posting.is_compressed());
    if !can_try_wide {
        return bound;
    }
    let mut wide_bounds = SmallVec::<[f32; 8]>::new();
    for posting in lead.iter() {
        let block_max = posting.block_max_score_up_to_with_stats(lead_up_to, scorer);
        wide_bounds.push(block_max.score);
        #[cfg(test)]
        {
            bound.range_blocks_scanned += block_max.blocks_scanned;
        }
    }
    let wide_max_score = conservative_score_sum(wide_bounds.into_iter());
    if wide_max_score < floor {
        bound.up_to = lead_up_to;
        bound.max_score = wide_max_score;
        bound.wide = true;
    }
    bound
}

/// Outcome of slicing one conjunction window.
pub(super) enum AndWindowSlice {
    /// Some clause has no document left: the conjunction is done.
    Exhausted,
    /// `wins` holds one non-empty slice per clause, all within `win_end`.
    Ready { win_end: u64 },
    /// Some clause has no document in the window; no match starts before
    /// `jump_to`.
    Empty { win_end: u64, jump_to: u64 },
}

/// Positions every clause's block cursor at the block that can hold `target`,
/// ends the window at the nearest next-block boundary, and slices each
/// clause's decompressed block to `[target, win_end]` into `wins`.
///
/// `slice_prev` holds, per clause, the last sliced block and slice end, so a
/// contiguous next window resumes there.
pub(super) fn slice_and_window(
    lead: &mut [Box<PostingIterator>],
    target: u64,
    slice_prev: &mut [(usize, usize)],
    wins: &mut Vec<WindowList>,
) -> AndWindowSlice {
    debug_assert!(target <= u32::MAX as u64);
    let target32 = target as u32;

    let mut win_end = TERMINATED_DOC_ID;
    for posting in lead.iter_mut() {
        let (block_idx, block_up_to) = {
            let PostingList::Compressed(ref list) = posting.list else {
                unreachable!("bulk AND requires compressed postings");
            };
            let block_idx = posting.block_idx_for_doc(list, posting.block_idx, target32);
            let block_up_to = if block_idx + 1 < list.blocks.len() {
                u64::from(list.block_least_doc_id(block_idx + 1)).saturating_sub(1)
            } else {
                TERMINATED_DOC_ID
            };
            (block_idx, block_up_to.max(target))
        };
        posting.block_idx = block_idx;
        win_end = win_end.min(block_up_to);
    }
    let win_end32 = u32::try_from(win_end).unwrap_or(u32::MAX);

    wins.clear();
    // When a clause has no document in the window, the conjunction cannot
    // match before that clause's next document, so the next window starts
    // there instead of at the next block boundary. A sparse lead then drives
    // the traversal and dense followers skip whole blocks by metadata instead
    // of being sliced block by block.
    for (j, posting) in lead.iter().enumerate() {
        let PostingList::Compressed(ref list) = posting.list else {
            unreachable!("bulk AND requires compressed postings");
        };
        let block_idx = posting.block_idx;
        let state = unsafe { &mut *posting.ensure_compressed_doc_ids_ptr(list, block_idx) };
        let doc_ids = state.doc_ids.as_slice();
        let len = doc_ids.len();
        // Everything before the previous slice's end of this block is below
        // the current target, so resume from there.
        let (prev_block, prev_hi) = slice_prev[j];
        let start = if prev_block == block_idx {
            prev_hi.min(len)
        } else {
            0
        };
        let lo = if start < len && doc_ids[start] >= target32 {
            start
        } else {
            start + doc_ids[start..].partition_point(|&doc| doc < target32)
        };
        let hi = if win_end32 == u32::MAX || len == 0 || doc_ids[len - 1] <= win_end32 {
            len
        } else {
            lo + doc_ids[lo..].partition_point(|&doc| doc <= win_end32)
        };
        slice_prev[j] = (block_idx, hi);
        if lo == hi {
            // No docs of this clause in the window. If this was the clause's
            // last block and it is fully behind the target, the clause is
            // exhausted and the conjunction is done.
            if lo < state.doc_ids.len() {
                return AndWindowSlice::Empty {
                    win_end,
                    jump_to: u64::from(state.doc_ids[lo]),
                };
            }
            if block_idx + 1 < list.blocks.len() {
                return AndWindowSlice::Empty {
                    win_end,
                    jump_to: u64::from(list.block_least_doc_id(block_idx + 1)),
                };
            }
            return AndWindowSlice::Exhausted;
        }
        wins.push(WindowList {
            docs: state.doc_ids.as_ptr(),
            freqs: std::ptr::null(),
            pos: lo,
            end: hi,
            block_start: block_idx << list.block_shift(),
        });
    }
    AndWindowSlice::Ready { win_end }
}

/// Points each window at its clause's decompressed block frequencies.
pub(super) fn load_window_freqs(lead: &[Box<PostingIterator>], wins: &mut [WindowList]) {
    for (win, posting) in wins.iter_mut().zip(lead) {
        let PostingList::Compressed(ref list) = posting.list else {
            unreachable!("bulk AND requires compressed postings");
        };
        let state = unsafe { &mut *posting.ensure_compressed_block_ptr(list, posting.block_idx) };
        win.freqs = state.freqs.as_ptr();
    }
}

/// Upper bound of `lead`'s score by clamped frequency, independent of the
/// document length. A negative query weight gets an all-infinite table, so
/// the pre-filter never fires for it.
pub(super) fn lead_freq_bound_lut<S: Scorer + ?Sized>(
    lead: &PostingIterator,
    scorer: &S,
) -> [f32; FREQ_LUT_BUCKETS] {
    let mut lut = [f32::INFINITY; FREQ_LUT_BUCKETS];
    if lead.query_weight < 0.0 {
        return lut;
    }
    // Prefer the scorer's explicit per-frequency upper bound. A scorer
    // that cannot provide one is not covered by the `doc_weight`
    // monotonicity contract, so fall back to the scorer's existing
    // global bound; when that is also unavailable, leave the bucket
    // infinite so the pre-filter never fires (no candidate is rejected).
    let global_fallback = match scorer.doc_weight_upper_bound() {
        Some(weight) => lead.query_weight * weight,
        None => f32::INFINITY,
    };
    for (freq, slot) in lut.iter_mut().enumerate().take(FREQ_LUT_BUCKETS - 1) {
        *slot = match scorer.doc_weight_upper_bound_for_freq(freq as u32) {
            Some(weight) => lead.query_weight * weight,
            None => global_fallback,
        };
    }
    // The clamp bucket covers every in-list frequency >= 63, so the
    // list-derived bound is a valid (and tighter) ceiling for it.
    lut[FREQ_LUT_BUCKETS - 1] = lead.frequency_clamp_upper_bound(scorer);
    #[cfg(test)]
    {
        // Only a scorer-provided per-frequency bound makes a claim that
        // can be checked against `doc_weight`; the list-wide fallback is
        // not a per-frequency bound (it may be exceeded by a frequency
        // that does not occur in this list).
        for freq in 0..FREQ_LUT_BUCKETS - 1 {
            let Some(weight) = scorer.doc_weight_upper_bound_for_freq(freq as u32) else {
                continue;
            };
            let bound = lead.query_weight * weight;
            for sample_len in [1_u32, 4096] {
                let real = lead.score(scorer, freq as u32, sample_len);
                debug_assert!(
                    real <= bound + 1e-6,
                    "doc_weight_upper_bound_for_freq(freq={freq})={weight} must \
                     upper-bound doc_weight (len={sample_len} gives {real})"
                );
            }
        }
    }
    lut
}
