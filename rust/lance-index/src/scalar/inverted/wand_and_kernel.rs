// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Window merge kernels for the bulk conjunction route.
//!
//! A window covers a doc range in which every conjunct clause contributes
//! exactly one decompressed posting block. [`merge_window`] intersects those
//! blocks and emits each matching doc id together with the per-clause
//! block-local offsets the caller needs to reach the matching frequencies.
//!
//! Everything here is a pure function over immutable window slices. The
//! kernels never advance a posting cursor, never read or update `threshold`,
//! and never touch phrase or terminal state: the caller materializes the
//! window, decides whether a positive floor exists, and does all scoring.
//! The only floor-derived input is a caller-owned frequency LUT.

use arrow_array::Array;
use lance_core::Result;
use smallvec::SmallVec;

use super::super::block::{BlockBuf, BlockStatus, Window};
use super::super::compound::ScoreBounds;
use super::super::scorer::{Scorer, bm25_doc_weight_with_norm};
#[cfg(target_arch = "x86_64")]
use super::find_next_geq_avx2;
use super::{
    FREQ_LUT_BUCKETS, PostingIterator, PostingList, TERMINATED_DOC_ID, WandDocuments,
    conservative_score_sum, find_next_geq, find_next_geq_scalar, intersection,
};

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

/// Inclusive end of the posting block that holds `target`: the document just
/// before the next block's first document, never below `target`. The last
/// block reports [`TERMINATED_DOC_ID`].
pub(super) fn posting_block_up_to(posting: &PostingIterator, target: u64) -> u64 {
    posting
        .next_block_first_doc()
        .map(|doc| doc.saturating_sub(1))
        .unwrap_or(TERMINATED_DOC_ID)
        .max(target)
}

/// Outcome of [`open_window`].
pub(super) struct OpenWindow {
    /// Inclusive end of the opened window; [`TERMINATED_DOC_ID`] when a
    /// clause's block is its last one.
    pub(super) up_to: u64,
    /// Some clause holds no document inside `[target, up_to]`, so the window
    /// cannot contain a conjunction match and only its end is meaningful.
    pub(super) skipped: bool,
    /// First document at or after `target` that a skipped window could not
    /// reach. The next window starts here when it lies beyond `up_to`.
    pub(super) jump_to: u64,
    /// A clause has no block left at or after `target`: the conjunction has no
    /// further matches.
    pub(super) exhausted: bool,
}

/// Position every clause's block cursor at the block that can hold `target`,
/// end the window at the nearest next-block boundary, and slice each clause's
/// block to `[target, up_to]`.
///
/// `win_cap` truncates the window for callers that must not emit more than a
/// fixed number of documents; the conjunction terminal passes
/// `u64::MAX`. `slice_prev` caches, per clause, the block that was sliced last
/// and the exclusive end of that slice, so contiguous windows resume with a
/// compare instead of a partition point.
///
/// All postings must be compressed. `posting.block_idx` is left on the block
/// the window uses; callers that need block maxima read them after this call.
///
/// One document block per clause stays decompressed while the returned
/// `wins` slices are live: the block cursor does not move inside a window, and
/// position decoding writes to separate fields of the same state.
pub(super) fn open_window(
    postings: &mut [Box<PostingIterator>],
    target: u64,
    win_cap: u64,
    wins: &mut SmallVec<[WindowList; 8]>,
    slice_prev: &mut [(usize, usize)],
) -> OpenWindow {
    debug_assert!(target <= u32::MAX as u64);
    debug_assert_eq!(slice_prev.len(), postings.len());
    let target32 = target as u32;

    let mut win_end = TERMINATED_DOC_ID;
    for posting in postings.iter_mut() {
        let PostingList::Compressed(ref list) = posting.list else {
            unreachable!("window opening requires compressed postings");
        };
        let block_idx = posting.block_idx_for_doc(list, posting.block_idx, target32);
        let block_up_to = if block_idx + 1 < list.blocks.len() {
            u64::from(list.block_least_doc_id(block_idx + 1)).saturating_sub(1)
        } else {
            TERMINATED_DOC_ID
        };
        posting.block_idx = block_idx;
        win_end = win_end.min(block_up_to.max(target));
    }
    let win_end = win_end.min(win_cap);
    let win_end32 = u32::try_from(win_end).unwrap_or(u32::MAX);

    wins.clear();
    for (index, posting) in postings.iter().enumerate() {
        let PostingList::Compressed(ref list) = posting.list else {
            unreachable!("window opening requires compressed postings");
        };
        let block_idx = posting.block_idx;
        // SAFETY: the pointer belongs to this posting's own `UnsafeCell`
        // state. The window does not move the cursor, so the decompressed
        // block stays valid until the caller finishes with `wins`.
        let state = unsafe { &mut *posting.ensure_compressed_doc_ids_ptr(list, block_idx) };
        let doc_ids = state.doc_ids.as_slice();
        let len = doc_ids.len();
        // Everything before the previous slice's end of this block is below the
        // current target, so resume from there.
        let (prev_block, prev_hi) = slice_prev[index];
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
        slice_prev[index] = (block_idx, hi);
        if lo == hi {
            // No document of this clause in the window: the whole window has no
            // conjunction match. If this was the clause's last block and it is
            // fully behind the target, the clause is exhausted.
            let (jump_to, exhausted) = if lo < len {
                (u64::from(doc_ids[lo]), false)
            } else if block_idx + 1 < list.blocks.len() {
                (u64::from(list.block_least_doc_id(block_idx + 1)), false)
            } else {
                (0, true)
            };
            return OpenWindow {
                up_to: win_end,
                skipped: true,
                jump_to,
                exhausted,
            };
        }
        wins.push(WindowList {
            docs: state.doc_ids.as_ptr(),
            freqs: std::ptr::null(),
            pos: lo,
            end: hi,
            block_start: block_idx << list.block_shift(),
        });
    }

    OpenWindow {
        up_to: win_end,
        skipped: false,
        jump_to: 0,
        exhausted: false,
    }
}

/// First document of the window that follows one ending at `up_to`, or
/// `u64::MAX` when there is no later window.
pub(super) fn next_window_start(up_to: u64, jump_to: u64) -> u64 {
    if up_to == TERMINATED_DOC_ID {
        return u64::MAX;
    }
    (up_to + 1).max(jump_to)
}

/// Buffers one [`AndMergeKernel`] reuses across windows.
///
/// Only `slice_prev` survives a call, and only as a resume hint: windows are
/// contiguous, so the next slice of a block starts where the previous one
/// ended. Everything else is cleared before use, which keeps a window's cost
/// to the intersection work itself.
pub(super) struct AndWindowScratch {
    slice_prev: SmallVec<[(usize, usize); 8]>,
    docs: Vec<u32>,
    offs: Vec<u8>,
    cursor_scratch: Vec<usize>,
    intersection_scratch: Vec<u32>,
    anchors: SmallVec<[usize; 8]>,
}

impl AndWindowScratch {
    pub(super) fn new(num_lists: usize) -> Self {
        Self {
            slice_prev: std::iter::repeat_n((usize::MAX, 0), num_lists).collect(),
            docs: Vec::with_capacity(super::super::encoding::MAX_POSTING_BLOCK_SIZE),
            offs: Vec::with_capacity(super::super::encoding::MAX_POSTING_BLOCK_SIZE * num_lists),
            cursor_scratch: Vec::with_capacity(num_lists),
            intersection_scratch: Vec::new(),
            anchors: SmallVec::new(),
        }
    }
}

/// Conjunction block source over a flat posting list.
///
/// The kernel owns no state that outlives one call: [`open_window`] gives it
/// one decompressed block per clause, [`merge_window`] intersects them, and
/// [`window_bm25_score`] totals each match in query order. Scores are the same
/// f32 additions the per-document cursor performs, so a leaf that serves both
/// faces reports identical numbers.
///
/// The kernel never prunes. A competitive floor belongs to the caller, which
/// keeps the bound and the emitted documents independent of iteration order.
pub(super) struct AndMergeKernel<'a, 's, S: Scorer + ?Sized, D: WandDocuments> {
    postings: &'a mut [Box<PostingIterator>],
    /// Clause indices in query order: ascending `(position, token_id)`.
    score_order: &'a [usize],
    documents: &'a D,
    scorer: &'a S,
    scratch: &'s mut AndWindowScratch,
}

impl<'a, 's, S: Scorer, D: WandDocuments> AndMergeKernel<'a, 's, S, D> {
    pub(super) fn new(
        postings: &'a mut [Box<PostingIterator>],
        score_order: &'a [usize],
        documents: &'a D,
        scorer: &'a S,
        scratch: &'s mut AndWindowScratch,
    ) -> Self {
        Self {
            postings,
            score_order,
            documents,
            scorer,
            scratch,
        }
    }

    /// Position every clause on the first common document at or after
    /// `target`, or report that no such document exists.
    pub(super) fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        if self.postings.is_empty() {
            return Ok(None);
        }
        let mut current = target;
        loop {
            for posting in self.postings.iter_mut() {
                if posting.doc().is_none_or(|doc| doc.doc_id() < current) {
                    posting.next(current);
                }
            }
            let mut max_doc = 0;
            let mut aligned = true;
            for posting in self.postings.iter() {
                let Some(doc) = posting.doc() else {
                    return Ok(None);
                };
                let doc_id = doc.doc_id();
                max_doc = max_doc.max(doc_id);
                aligned &= doc_id == current;
            }
            if aligned {
                return Ok(Some(current));
            }
            // Every clause is at or beyond `current` and at least one is past
            // it, so the leapfrog target strictly increases.
            current = max_doc;
        }
    }

    /// Inclusive end of the block holding `target`, for the tightest clause.
    /// Restores the block anchors it touches.
    pub(super) fn block_end_at(&mut self, target: u64) -> Result<u64> {
        self.save_block_anchors();
        let mut up_to = TERMINATED_DOC_ID;
        for posting in self.postings.iter_mut() {
            posting.shallow_next(target);
            up_to = up_to.min(posting_block_up_to(posting, target));
        }
        self.restore_block_anchors();
        Ok(up_to)
    }

    /// Conservative bound for `window`: the sum of every clause's block maxima
    /// over the window. Restores the block anchors it touches.
    pub(super) fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds> {
        self.save_block_anchors();
        for posting in self.postings.iter_mut() {
            posting.shallow_next(window.start);
        }
        let upper = conservative_score_sum(self.postings.iter().map(|posting| {
            posting
                .block_max_score_up_to_with_stats(window.up_to, self.scorer)
                .score
        }));
        self.restore_block_anchors();
        ScoreBounds::try_new(0.0, upper)
    }

    /// Produce the next block of conjunction matches at or after `target`.
    pub(super) fn next_block(
        &mut self,
        target: u64,
        budget: usize,
        out: &mut BlockBuf,
    ) -> Result<BlockStatus> {
        if budget == 0 {
            return Err(lance_core::Error::invalid_input(
                "FTS block request budget must be at least 1",
            ));
        }
        if self.postings.is_empty() {
            return Ok(BlockStatus::Exhausted);
        }
        let num_lists = self.postings.len();
        let mut start = target;
        // The window itself lives for one call: its slices are raw pointers
        // into per-posting state, so keeping them in the reusable scratch
        // would leave stale addresses in a structure that outlives the window.
        let mut wins = SmallVec::<[WindowList; 8]>::new();
        loop {
            if start > u32::MAX as u64 {
                return Ok(BlockStatus::Exhausted);
            }
            // The budget caps one produced block, so it is measured from the
            // window about to be opened; a window always ends at or after
            // `start` and the next one starts beyond that end, which bounds the
            // loop even when windows are skipped.
            let win_cap = start.saturating_add(budget as u64 - 1);
            let open = open_window(
                self.postings,
                start,
                win_cap,
                &mut wins,
                self.scratch.slice_prev.as_mut_slice(),
            );
            if open.exhausted {
                return Ok(BlockStatus::Exhausted);
            }
            if open.skipped {
                start = next_window_start(open.up_to, open.jump_to);
                continue;
            }
            let up_to = open.up_to;
            // The bound must be read while every cursor still sits in the
            // window's own block, which the intersection below leaves behind.
            let upper = self.upper_bound(&Window { start, up_to })?;

            let scratch = &mut *self.scratch;
            scratch.docs.clear();
            scratch.offs.clear();
            let freq_cannot_beat = [false; FREQ_LUT_BUCKETS];
            #[cfg(target_arch = "x86_64")]
            let use_avx2 = *super::HAS_AVX2;
            #[cfg(not(target_arch = "x86_64"))]
            let use_avx2 = false;
            // Block mode does not tune the intersection arm: without a floor
            // every arm produces the same document list.
            merge_window(
                &wins,
                num_lists,
                use_avx2,
                false,
                false,
                &freq_cannot_beat,
                &mut scratch.docs,
                &mut scratch.offs,
                &mut scratch.cursor_scratch,
                &mut scratch.intersection_scratch,
            );
            if scratch.docs.is_empty() {
                if up_to == TERMINATED_DOC_ID {
                    return Ok(BlockStatus::Exhausted);
                }
                start = up_to + 1;
                continue;
            }
            for (window, posting) in wins.iter_mut().zip(self.postings.iter()) {
                let PostingList::Compressed(ref list) = posting.list else {
                    unreachable!("window opening requires compressed postings");
                };
                let state =
                    unsafe { &mut *posting.ensure_compressed_block_ptr(list, posting.block_idx) };
                window.freqs = state.freqs.as_ptr();
            }

            out.clear();
            for (index, &doc) in scratch.docs.iter().enumerate() {
                // A filtered-out document contributes neither a candidate nor a
                // key, so the emitted block holds visible documents only.
                let Some(key) = self.documents.document_key_for_doc_id(doc) else {
                    continue;
                };
                let doc_length = self.documents.scoring_num_tokens(doc);
                let offs = &scratch.offs[index * num_lists..(index + 1) * num_lists];
                let score = window_bm25_score(
                    self.postings,
                    &wins,
                    offs,
                    self.score_order,
                    self.scorer,
                    None,
                    doc_length,
                );
                out.docs.push(u64::from(doc));
                out.scores.push(score);
                out.keys.push(key);
                out.matched.push(true);
                for clause in 0..num_lists {
                    out.freqs
                        .push(unsafe { *wins[clause].freqs.add(offs[clause] as usize) });
                }
            }

            out.start = start;
            out.up_to = up_to;
            out.clause_count = num_lists;
            out.upper = upper;
            return Ok(if out.docs.is_empty() {
                BlockStatus::Empty { up_to }
            } else {
                BlockStatus::Data
            });
        }
    }

    fn save_block_anchors(&mut self) {
        self.scratch.anchors.clear();
        self.scratch
            .anchors
            .extend(self.postings.iter().map(|posting| posting.block_idx));
    }

    fn restore_block_anchors(&mut self) {
        for (posting, &block_idx) in self.postings.iter_mut().zip(self.scratch.anchors.iter()) {
            posting.block_idx = block_idx;
        }
    }
}
