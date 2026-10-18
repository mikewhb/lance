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

use super::super::scorer::{Scorer, bm25_doc_weight_with_norm};
#[cfg(target_arch = "x86_64")]
use super::find_next_geq_avx2;
use super::{FREQ_LUT_BUCKETS, PostingIterator, find_next_geq, find_next_geq_scalar, intersection};

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
