// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Pairwise intersection of the sorted, distinct DocIDs in posting blocks.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    _mm_castsi128_ps, _mm_cmpeq_epi32, _mm_loadu_si128, _mm_movemask_ps, _mm_or_si128,
    _mm_shuffle_epi32,
};

#[inline]
pub(super) fn intersect(left: &[u32], right: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(left.len());
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 support was checked above.
        unsafe { intersect_avx2(left, right, out) };
        return;
    }
    intersect_scalar(left, right, out);
}

#[inline]
fn intersect_scalar(left: &[u32], right: &[u32], out: &mut Vec<u32>) {
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                out.push(left[a]);
                a += 1;
                b += 1;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn intersect_avx2(left: &[u32], right: &[u32], out: &mut Vec<u32>) {
    let (mut a, mut b) = (0, 0);
    while a + 4 <= left.len() && b + 4 <= right.len() {
        let a_max = left[a + 3];
        let b_max = right[b + 3];
        if a_max < right[b] {
            a += 4;
            continue;
        }
        if b_max < left[a] {
            b += 4;
            continue;
        }
        // Compare every lane in the left tile with every lane in the right
        // tile. Equality needs no signedness conversion for high DocIDs.
        // SAFETY: both slices have at least four entries at these offsets;
        // unaligned loads are permitted and AVX2 was checked by the caller.
        let mut mask = unsafe {
            let av = _mm_loadu_si128(left.as_ptr().add(a).cast());
            let mut bv = _mm_loadu_si128(right.as_ptr().add(b).cast());
            let mut eq = _mm_cmpeq_epi32(av, bv);
            bv = _mm_shuffle_epi32::<0x39>(bv);
            eq = _mm_or_si128(eq, _mm_cmpeq_epi32(av, bv));
            bv = _mm_shuffle_epi32::<0x39>(bv);
            eq = _mm_or_si128(eq, _mm_cmpeq_epi32(av, bv));
            bv = _mm_shuffle_epi32::<0x39>(bv);
            eq = _mm_or_si128(eq, _mm_cmpeq_epi32(av, bv));
            _mm_movemask_ps(_mm_castsi128_ps(eq)) as u32
        };
        while mask != 0 {
            out.push(left[a + mask.trailing_zeros() as usize]);
            mask &= mask - 1;
        }
        // Retain the tile with the larger maximum: its remaining documents
        // may match the next tile on the other side. Distinct sorted inputs
        // ensure emitted matches remain ordered and cannot be repeated.
        a += usize::from(a_max <= b_max) * 4;
        b += usize::from(b_max <= a_max) * 4;
    }
    intersect_scalar(&left[a..], &right[b..], out);
}

/// Index pairs `(left_index, right_index)` for equal values.
///
/// Same 4×4 tile walk as [`intersect_avx2`]. Outputs are ordered by left
/// index. Block offsets fit `u16` because a posting block holds at most 256
/// documents.
#[inline]
pub(super) fn intersect_indices(left: &[u32], right: &[u32], out: &mut Vec<(u16, u16)>) {
    out.clear();
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 support was checked above.
        unsafe { intersect_indices_avx2(left, right, out) };
        return;
    }
    append_index_pairs(left, right, 0, 0, out);
}

#[cfg(test)]
#[inline]
pub(super) fn intersect_indices_scalar(left: &[u32], right: &[u32], out: &mut Vec<(u16, u16)>) {
    out.clear();
    append_index_pairs(left, right, 0, 0, out);
}

#[inline]
fn append_index_pairs(
    left: &[u32],
    right: &[u32],
    left_base: usize,
    right_base: usize,
    out: &mut Vec<(u16, u16)>,
) {
    debug_assert!(
        left_base + left.len() <= u16::MAX as usize
            && right_base + right.len() <= u16::MAX as usize
    );
    let (mut a, mut b) = (0usize, 0usize);
    while a < left.len() && b < right.len() {
        match left[a].cmp(&right[b]) {
            std::cmp::Ordering::Less => a += 1,
            std::cmp::Ordering::Greater => b += 1,
            std::cmp::Ordering::Equal => {
                out.push(((left_base + a) as u16, (right_base + b) as u16));
                a += 1;
                b += 1;
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn intersect_indices_avx2(left: &[u32], right: &[u32], out: &mut Vec<(u16, u16)>) {
    let (mut a, mut b) = (0usize, 0usize);
    while a + 4 <= left.len() && b + 4 <= right.len() {
        let a_max = left[a + 3];
        let b_max = right[b + 3];
        if a_max < right[b] {
            a += 4;
            continue;
        }
        if b_max < left[a] {
            b += 4;
            continue;
        }
        // Each rotate compares the left tile with one alignment of the right
        // tile. Values are unique, so a left lane matches at most one right lane.
        let mut hits = [(0u16, 0u16); 4];
        let mut n_hits = 0usize;
        unsafe {
            let av = _mm_loadu_si128(left.as_ptr().add(a).cast());
            let mut bv = _mm_loadu_si128(right.as_ptr().add(b).cast());
            for rotate in 0..4u16 {
                let mut mask = _mm_movemask_ps(_mm_castsi128_ps(_mm_cmpeq_epi32(av, bv))) as u32;
                while mask != 0 {
                    let lane = mask.trailing_zeros() as u16;
                    hits[n_hits] = (a as u16 + lane, b as u16 + (lane + rotate) % 4);
                    n_hits += 1;
                    mask &= mask - 1;
                }
                bv = _mm_shuffle_epi32::<0x39>(bv);
            }
        }
        hits[..n_hits].sort_unstable_by_key(|hit| hit.0);
        out.extend_from_slice(&hits[..n_hits]);
        a += usize::from(a_max <= b_max) * 4;
        b += usize::from(b_max <= a_max) * 4;
    }
    append_index_pairs(&left[a..], &right[b..], a, b, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn assert_intersection(left: &[u32], right: &[u32]) {
        let expected = left
            .iter()
            .filter(|doc| right.contains(doc))
            .copied()
            .collect::<Vec<_>>();
        let mut actual = Vec::new();
        intersect_scalar(left, right, &mut actual);
        assert_eq!(actual, expected);
        intersect(left, right, &mut actual);
        assert_eq!(actual, expected, "intersection of {left:?} and {right:?}");
    }

    #[test]
    fn exhaustive_short_sets() {
        for left_mask in 0u32..256 {
            let left = (0..8)
                .filter(|bit| left_mask & (1 << bit) != 0)
                .collect::<Vec<_>>();
            for right_mask in 0u32..256 {
                let right = (0..8)
                    .filter(|bit| right_mask & (1 << bit) != 0)
                    .collect::<Vec<_>>();
                assert_intersection(&left, &right);
            }
        }
    }

    #[rstest]
    fn sparse_full_domain_blocks(
        #[values(0, 1, 3, 4, 5, 7, 8, 9, 127, 128, 255, 256)] right_len: u32,
    ) {
        for left_len in 0..=256 {
            let left = (0..left_len)
                .map(|i| u32::MAX - (255 - i) * 10_000_000)
                .collect::<Vec<_>>();
            let right = (0..right_len)
                .map(|i| u32::MAX - (255 - i) * 10_000_001)
                .collect::<Vec<_>>();
            assert_intersection(&left, &right);
        }
    }

    fn assert_index_pairs(left: &[u32], right: &[u32]) {
        let mut expected = Vec::new();
        let (mut a, mut b) = (0usize, 0usize);
        while a < left.len() && b < right.len() {
            match left[a].cmp(&right[b]) {
                std::cmp::Ordering::Less => a += 1,
                std::cmp::Ordering::Greater => b += 1,
                std::cmp::Ordering::Equal => {
                    expected.push((a as u16, b as u16));
                    a += 1;
                    b += 1;
                }
            }
        }
        let mut scalar = Vec::new();
        intersect_indices_scalar(left, right, &mut scalar);
        assert_eq!(scalar, expected);
        let mut actual = Vec::new();
        intersect_indices(left, right, &mut actual);
        assert_eq!(actual, expected, "indices of {left:?} and {right:?}");
        assert!(actual.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn intersect_indices_exhaustive_short_sets() {
        for left_mask in 0u32..256 {
            let left = (0..8)
                .filter(|bit| left_mask & (1 << bit) != 0)
                .collect::<Vec<_>>();
            for right_mask in 0u32..256 {
                let right = (0..8)
                    .filter(|bit| right_mask & (1 << bit) != 0)
                    .collect::<Vec<_>>();
                assert_index_pairs(&left, &right);
            }
        }
    }

    #[test]
    fn intersect_indices_random_long_slices() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..40 {
            let left_len = (next() % 257) as usize;
            let right_len = (next() % 257) as usize;
            let mut left = (0..left_len).map(|_| next() as u32).collect::<Vec<_>>();
            let mut right = (0..right_len).map(|_| next() as u32).collect::<Vec<_>>();
            left.sort_unstable();
            left.dedup();
            right.sort_unstable();
            right.dedup();
            assert_index_pairs(&left, &right);
        }
    }
}
