// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

use arrow_array::Array;

use super::super::posting::FREQ_LUT_BUCKETS;
use super::super::wide_window::wide_gate;
use super::*;
use crate::scalar::inverted::LEGACY_BLOCK_SIZE;
use crate::scalar::inverted::impact::ImpactSkipData;

type WideRow = (u64, u32, Vec<(u32, u32)>);

/// One conjunction run: the rows, the score bits of each row, the k-th score
/// bits published to the shared floor, and the window counters.
#[derive(Debug)]
struct WideRun {
    rows: Vec<WideRow>,
    scores: Vec<u32>,
    floor: u32,
    windows: usize,
    fit_windows: usize,
    blocks_skipped: usize,
    lead_freq_decodes: usize,
    pairwise_windows: usize,
}

fn run_wide_path<S: Scorer>(
    build: &impl Fn() -> Vec<PostingIterator>,
    docs: &DocSet,
    scorer: &impl Fn() -> S,
    params: &FtsSearchParams,
    mode: BulkAndMode,
    wide: WideOverride,
) -> WideRun {
    let floor = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
    let mut wand = Wand::new(Operator::And, build().into_iter(), docs, scorer())
        .with_bulk_and_mode(mode)
        .with_bulk_and_wide(wide)
        .with_shared_threshold(floor.clone());
    let hits = wand.search(params, &NoOpMetricsCollector).unwrap();
    let mut rows = hits
        .into_iter()
        .map(|hit| {
            (
                (hit.document, hit.doc_length, hit.freqs),
                hit.score.to_bits(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    let (rows, scores) = rows.into_iter().unzip();
    let stats = &wand.and_window_stats;
    WideRun {
        rows,
        scores,
        floor: floor.load(Ordering::Relaxed),
        windows: stats.wide_windows,
        fit_windows: stats.wide_fit_windows,
        blocks_skipped: stats.wide_blocks_skipped,
        lead_freq_decodes: stats.wide_lead_freq_decodes,
        pairwise_windows: stats.pairwise_intersections,
    }
}

/// The classic loop and the bulk path with the wide window following the gate
/// or forced on must return the same rows with the same score bits and
/// publish the same k-th score bits. Returns the forced run so callers can
/// check its counters.
fn assert_wide_paths_agree<S: Scorer>(
    build: &impl Fn() -> Vec<PostingIterator>,
    docs: &DocSet,
    scorer: &impl Fn() -> S,
    params: &FtsSearchParams,
) -> WideRun {
    let classic = run_wide_path(
        build,
        docs,
        scorer,
        params,
        BulkAndMode::Off,
        WideOverride::Off,
    );
    let auto = run_wide_path(
        build,
        docs,
        scorer,
        params,
        BulkAndMode::On,
        WideOverride::Auto,
    );
    assert_eq!(auto.rows, classic.rows, "Auto changed the rows");
    assert_eq!(auto.scores, classic.scores, "Auto changed a score");
    assert_eq!(auto.floor, classic.floor, "Auto changed the k-th score");
    let always = run_wide_path(
        build,
        docs,
        scorer,
        params,
        BulkAndMode::On,
        WideOverride::Always,
    );
    assert_eq!(always.rows, classic.rows, "Always changed the rows");
    assert_eq!(always.scores, classic.scores, "Always changed a score");
    assert_eq!(always.floor, classic.floor, "Always changed the k-th score");
    always
}

/// A clause from (doc, frequency) pairs. 256-document blocks carry impact
/// data; 128-document blocks carry stored block maxima.
fn wide_clause(
    term: u32,
    weight: f32,
    docs: &[(u32, u32)],
    block_size: usize,
    num_docs: usize,
) -> PostingIterator {
    let ids = docs.iter().map(|&(doc, _)| doc).collect::<Vec<_>>();
    let freqs = docs.iter().map(|&(_, freq)| freq).collect::<Vec<_>>();
    let list = if block_size == MAX_POSTING_BLOCK_SIZE {
        let lengths = vec![1; ids.len()];
        generate_impact_posting_list_with_freqs_and_block_size(ids, freqs, lengths, block_size)
    } else {
        let max_freq = freqs.iter().copied().max().unwrap_or(1);
        generate_posting_list_with_freqs(ids, freqs, weight * max_freq as f32, None, true)
    };
    PostingIterator::with_query_weight(format!("t{term}"), term, term, weight, list, num_docs)
}

/// A phrase clause from (doc, positions) pairs, with at most four positions
/// per document so the stored maximum stays a bound.
fn wide_phrase_clause(
    term: u32,
    weight: f32,
    docs: &[(u32, Vec<u32>)],
    num_docs: usize,
) -> PostingIterator {
    let ids = docs.iter().map(|&(doc, _)| doc).collect::<Vec<_>>();
    let positions = docs.iter().map(|(_, pos)| pos.clone()).collect::<Vec<_>>();
    let list = generate_posting_list_with_positions(ids, positions, weight * 4.0, true);
    PostingIterator::with_query_weight(format!("t{term}"), term, term, weight, list, num_docs)
}

const WIDE_LIMITS: [usize; 4] = [1, 3, 10, 100];

fn wide_params(limit: usize) -> FtsSearchParams {
    FtsSearchParams::default().with_limit(Some(limit))
}

/// A 384-posting lead plus `follower_docs` postings split evenly across the
/// followers. With 128-document blocks, 6144 follower postings put
/// `8 * followers_cost` exactly on `block_size * lead_cost`.
fn gate_fixture(
    num_docs: usize,
    follower_docs: u32,
    clauses: usize,
) -> impl Fn() -> Vec<PostingIterator> {
    move || {
        let lead = (0..384u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
        let share = (follower_docs / (clauses as u32 - 1).max(1)).max(1);
        let mut postings = vec![wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs)];
        for term in 1..clauses {
            let docs = (0..share).map(|doc| (doc, 1)).collect::<Vec<_>>();
            postings.push(wide_clause(
                term as u32,
                1.0,
                &docs,
                LEGACY_BLOCK_SIZE,
                num_docs,
            ));
        }
        postings
    }
}

fn gate_probe(docs: &DocSet, follower_docs: u32, clauses: usize, limit: usize) -> WideRun {
    run_wide_path(
        &gate_fixture(docs.len(), follower_docs, clauses),
        docs,
        &|| UnitScorer,
        &wide_params(limit),
        BulkAndMode::On,
        WideOverride::Auto,
    )
}

#[test]
fn wide_gate_thresholds() {
    // block_size = 128, lead_cost = 1: two and three clauses need 16
    // follower postings, four or more need 10.
    assert!(!wide_gate(1, 15, 128, 2), "8 * 15 < 128");
    assert!(wide_gate(1, 16, 128, 2), "equality opens the gate");
    assert!(wide_gate(1, 16, 128, 3));
    assert!(!wide_gate(1, 9, 128, 4), "13 * 9 < 128");
    assert!(wide_gate(1, 10, 128, 4), "13 * 10 >= 128");
    // Doubling the block size doubles the follower density the gate needs.
    assert!(!wide_gate(1, 31, 256, 2));
    assert!(wide_gate(1, 32, 256, 2));
    // Saturating multiplication keeps the extremes defined.
    assert!(wide_gate(1, u64::MAX, 128, 2));
    assert!(!wide_gate(u64::MAX, 1, 128, 2));
}

/// A query built so that `8 * followers_cost == block_size * lead_cost`
/// reaches the dispatch, and one follower posting less does not.
#[rstest]
#[case::two_clauses(2)]
#[case::three_clauses(3)]
fn wide_gate_opens_at_exact_equality(#[case] clauses: usize) {
    let docs = unit_length_docs(8_192);
    let open = gate_probe(&docs, 6_144, clauses, 1);
    assert!(open.windows > 0, "{open:?}");
    let closed = gate_probe(&docs, 6_143, clauses, 1);
    assert_eq!(closed.windows + closed.fit_windows, 0, "{closed:?}");
    assert_eq!(open.rows, closed.rows, "the gate must not change results");
}

#[test]
fn wide_gate_opens_for_every_clause_count_from_two() {
    let docs = unit_length_docs(8_192);
    for clauses in [2usize, 3, 4, 5, 6] {
        let probe = gate_probe(&docs, 6_144 * (clauses as u32 - 1), clauses, 1);
        assert!(probe.windows > 0, "{clauses}: {probe:?}");
    }
    // One clause has no follower to cut the lead block.
    let single = gate_probe(&docs, 6_144, 1, 1);
    assert_eq!(single.windows + single.fit_windows, 0, "{single:?}");
    // Four clauses just below their density stay narrow.
    let closed = gate_probe(&docs, 3_780, 4, 1);
    assert_eq!(closed.windows + closed.fit_windows, 0, "{closed:?}");
}

/// A balanced query fails the gate. A skewed query whose narrow window already
/// reaches the lead block end passes it but keeps the narrow window.
#[test]
fn wide_gate_keeps_balanced_and_aligned_queries_narrow() {
    let num_docs = 2_048usize;
    let docs = unit_length_docs(num_docs);
    let all = (0..num_docs as u32)
        .map(|doc| (doc, 1 + doc % 3))
        .collect::<Vec<_>>();
    let balanced = move || {
        vec![
            wide_clause(0, 1.0, &all, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 0.7, &all, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(2, 0.4, &all, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let auto = run_wide_path(
        &balanced,
        &docs,
        &|| UnitScorer,
        &wide_params(10),
        BulkAndMode::On,
        WideOverride::Auto,
    );
    assert_eq!((auto.windows, auto.fit_windows), (0, 0));
    let always = assert_wide_paths_agree(&balanced, &docs, &|| UnitScorer, &wide_params(10));
    assert!(always.windows > 0, "Always bypasses the gate");

    let aligned = gate_probe(&docs, 6_144, 2, 1);
    assert!(aligned.fit_windows > 0, "{aligned:?}");
}

/// Scattered clause densities make followers cross lead blocks. The clauses are
/// about equally dense, so below sixteen clauses the gate keeps them narrow and
/// the forced wide window carries the comparison.
#[rstest]
#[case::k2(2)]
#[case::k3(3)]
#[case::k4(4)]
#[case::k5(5)]
#[case::k8(8)]
#[case::k16(16)]
fn wide_window_matches_classic_on_bulk_corpus(#[case] num_clauses: usize) {
    let (docs, build) = bulk_corpus(num_clauses);
    let mut windows = 0;
    for limit in WIDE_LIMITS {
        windows +=
            assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(limit)).windows;
        // Length-dependent BM25 scores are inexact, so their bits also check
        // the summation order and the norm cache.
        windows += assert_wide_paths_agree(
            &build,
            &docs,
            &|| VariedBm25ShapeScorer,
            &wide_params(limit),
        )
        .windows;
    }
    assert!(windows > 0);
}

/// With inexact BM25 scores over real document lengths the gate opens the wide
/// window, and every path still returns the same rows with the same score bits.
#[rstest]
#[case::k2(2)]
#[case::k3(3)]
#[case::k4(4)]
#[case::k5(5)]
fn wide_window_matches_classic_with_bm25_scores(#[case] num_clauses: usize) {
    let (docs, build) = skewed_bm25_corpus(num_clauses);
    let scorer = || VariedBm25ShapeScorer;
    for limit in WIDE_LIMITS {
        let always = assert_wide_paths_agree(&build, &docs, &scorer, &wide_params(limit));
        assert!(!always.rows.is_empty());
        let auto = run_wide_path(
            &build,
            &docs,
            &scorer,
            &wide_params(limit),
            BulkAndMode::On,
            WideOverride::Auto,
        );
        assert!(auto.windows > 0, "{limit}: {auto:?}");
    }
}

/// With two or three clauses the BM25 bounds of a follower block plus the
/// clauses after it fall below the floor for every candidate in some blocks,
/// so the wide window skips those blocks.
#[rstest]
#[case::k2(2)]
#[case::k3(3)]
fn wide_window_skips_follower_blocks_with_bm25_bounds(#[case] num_clauses: usize) {
    let (docs, build) = skewed_bm25_corpus(num_clauses);
    let scorer = || VariedBm25ShapeScorer;
    let skipped = [1, 3]
        .into_iter()
        .map(|limit| {
            run_wide_path(
                &build,
                &docs,
                &scorer,
                &wide_params(limit),
                BulkAndMode::On,
                WideOverride::Auto,
            )
            .blocks_skipped
        })
        .sum::<usize>();
    assert!(skipped > 0);
}

/// A wand factor up to one never prunes past the exact floor, so every path
/// keeps the classic results. Above one a candidate is dropped only when its
/// bound cannot reach the factor times the floor: every returned score is
/// still exact, and a document missing from the approximate results scores at
/// most the factor times the published k-th score.
#[rstest]
#[case::k2(2)]
#[case::k3(3)]
fn wide_window_wand_factor_bounds_what_it_drops(#[case] num_clauses: usize) {
    let (docs, build) = skewed_bm25_corpus(num_clauses);
    let scorer = || VariedBm25ShapeScorer;
    for factor in [0.0, 0.5, 1.0] {
        for limit in WIDE_LIMITS {
            let params = wide_params(limit).with_wand_factor(factor);
            assert_wide_paths_agree(&build, &docs, &scorer, &params);
        }
    }
    let all = run_wide_path(
        &build,
        &docs,
        &scorer,
        &FtsSearchParams::default(),
        BulkAndMode::Off,
        WideOverride::Off,
    );
    let factor = 1.5_f32;
    // Auto falls back to narrow windows where the narrow one already fits, and
    // those consume the factor too; only the forced run isolates the wide one.
    let mut dropped_by_wide = 0;
    for limit in WIDE_LIMITS {
        let exact = run_wide_path(
            &build,
            &docs,
            &scorer,
            &wide_params(limit),
            BulkAndMode::Off,
            WideOverride::Off,
        );
        for wide in [WideOverride::Auto, WideOverride::Always] {
            let approximate = run_wide_path(
                &build,
                &docs,
                &scorer,
                &wide_params(limit).with_wand_factor(factor),
                BulkAndMode::On,
                wide,
            );
            assert!(approximate.windows > 0, "{limit} {wide:?}: {approximate:?}");
            for (row, score) in approximate.rows.iter().zip(&approximate.scores) {
                let exact_score = all.scores[all.rows.binary_search(row).unwrap()];
                assert_eq!(*score, exact_score, "document {}", row.0);
            }
            let reach = factor * f32::from_bits(approximate.floor);
            for (row, score) in exact.rows.iter().zip(&exact.scores) {
                if approximate.rows.binary_search(row).is_err() {
                    if wide == WideOverride::Always {
                        dropped_by_wide += 1;
                    }
                    assert!(
                        f32::from_bits(*score) <= reach,
                        "{limit} {wide:?}: document {}",
                        row.0
                    );
                }
            }
        }
    }
    assert!(dropped_by_wide > 0);
}

/// A rare lead over dense followers in 128-document impact blocks. The impact
/// data sees the real document lengths and the follower frequencies change
/// every 512 documents, so the gate opens the wide window and the BM25 bounds
/// are tight enough to prune in it. The lead holds two blocks, so the second
/// window starts with a floor.
fn skewed_bm25_corpus(num_clauses: usize) -> (DocSet, impl Fn() -> Vec<PostingIterator>) {
    let num_docs = 8_192u32;
    let length = |doc: u32| 8 + doc.wrapping_mul(7_919) % 120;
    let mut docs = DocSet::default();
    for doc in 0..num_docs {
        docs.append(u64::from(doc), length(doc));
    }
    docs.set_quantized_scoring(true);
    let build = move || {
        (0..num_clauses as u32)
            .map(|term| {
                let ids = (0..num_docs)
                    .filter(|&doc| {
                        if term == 0 {
                            doc % 32 == 0
                        } else {
                            // A per-clause offset before the hash makes each
                            // follower keep about three quarters of the lead
                            // documents, independently of the others.
                            ((doc + term * 7_919).wrapping_mul(2_654_435_761) >> 13) & 3 != 0
                        }
                    })
                    .collect::<Vec<_>>();
                let freqs = ids
                    .iter()
                    .map(|&doc| 1 + (doc / 512 + term) % 4)
                    .collect::<Vec<_>>();
                let lengths = ids.iter().map(|&doc| length(doc)).collect::<Vec<_>>();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term,
                    term,
                    1.0 + term as f32 * 0.25,
                    generate_impact_posting_list_with_freqs_and_block_size(
                        ids,
                        freqs,
                        lengths,
                        LEGACY_BLOCK_SIZE,
                    ),
                    num_docs as usize,
                )
            })
            .collect::<Vec<_>>()
    };
    (docs, build)
}

/// A corpus whose documents have varied lengths and whose clause densities are
/// scattered, in 256-document impact blocks that see those lengths.
fn bulk_corpus(num_clauses: usize) -> (DocSet, impl Fn() -> Vec<PostingIterator>) {
    let num_docs = (BLOCK_SIZE * 8 + 37) as u32;
    let length = |doc: u32| 32 + doc % 57;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), length(doc_id));
    }
    docs.set_quantized_scoring(true);
    let clause_docs = |modulus: u32, salt: u32| -> Vec<(u32, u32)> {
        (0..num_docs)
            .filter(|doc| (doc.wrapping_mul(2654435761).wrapping_add(salt)) % modulus < 2)
            .map(|doc| (doc, 1 + doc % 5))
            .collect()
    };
    let clauses = [
        clause_docs(3, 7),
        clause_docs(4, 13),
        clause_docs(5, 29),
        clause_docs(3, 41),
    ];
    let build = move || {
        (0..num_clauses)
            .map(|term| {
                let clause = &clauses[term % clauses.len()];
                let ids = clause.iter().map(|&(doc, _)| doc).collect::<Vec<_>>();
                let freqs = clause.iter().map(|&(_, freq)| freq).collect::<Vec<_>>();
                let lengths = ids.iter().map(|&doc| length(doc)).collect::<Vec<_>>();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term as u32,
                    term as u32,
                    1.0 + term as f32 * 0.5,
                    generate_impact_posting_list_with_freqs_and_block_size(
                        ids,
                        freqs,
                        lengths,
                        MAX_POSTING_BLOCK_SIZE,
                    ),
                    num_docs as usize,
                )
            })
            .collect::<Vec<_>>()
    };
    (docs, build)
}

#[rstest]
#[case::legacy_k4(4, LEGACY_BLOCK_SIZE)]
#[case::legacy_k5(5, LEGACY_BLOCK_SIZE)]
#[case::legacy_k8(8, LEGACY_BLOCK_SIZE)]
#[case::modern_k4(4, MAX_POSTING_BLOCK_SIZE)]
#[case::modern_k5(5, MAX_POSTING_BLOCK_SIZE)]
#[case::modern_k8(8, MAX_POSTING_BLOCK_SIZE)]
fn wide_window_handles_many_clauses(#[case] num_clauses: usize, #[case] block_size: usize) {
    let num_docs = 2_048usize;
    let docs = unit_length_docs(num_docs);
    let clauses = (0..num_clauses)
        .map(|term| {
            let modulus = 2 + (term as u32 % 5);
            (0..num_docs as u32)
                .filter(|doc| doc % modulus == 0)
                .map(|doc| (doc, 1 + (doc + term as u32) % 4))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let build = move || {
        clauses
            .iter()
            .enumerate()
            .map(|(term, ids)| {
                wide_clause(
                    term as u32,
                    1.0 - 0.05 * term as f32,
                    ids,
                    block_size,
                    num_docs,
                )
            })
            .collect::<Vec<_>>()
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(20));
    assert!(run.windows > 0);
}

/// The norm cache, quantized norms without addends, and exact token counts
/// must all give the narrow path's rows.
#[test]
fn wide_window_matches_classic_on_every_length_source() {
    let num_docs = 2_048usize;
    let mut quantized = DocSet::default();
    let mut exact = DocSet::default();
    for doc in 0..num_docs {
        quantized.append(doc as u64, 1 + (doc * 7 % 50) as u32);
        exact.append(doc as u64, 1 + (doc * 7 % 50) as u32);
    }
    quantized.set_quantized_scoring(true);
    let clauses = [
        (0..num_docs as u32)
            .filter(|doc| doc % 2 == 0)
            .map(|doc| (doc, 1 + doc % 6))
            .collect::<Vec<_>>(),
        (0..num_docs as u32)
            .filter(|doc| doc % 3 == 0)
            .map(|doc| (doc, 2))
            .collect::<Vec<_>>(),
    ];
    let build = move || {
        clauses
            .iter()
            .enumerate()
            .map(|(term, ids)| wide_clause(term as u32, 1.0, ids, MAX_POSTING_BLOCK_SIZE, num_docs))
            .collect::<Vec<_>>()
    };
    for limit in WIDE_LIMITS {
        assert_wide_paths_agree(
            &build,
            &quantized,
            &|| VariedBm25ShapeScorer,
            &wide_params(limit),
        );
    }
    assert_wide_paths_agree(&build, &quantized, &|| UnitScorer, &wide_params(10));
    assert_wide_paths_agree(&build, &exact, &|| UnitScorer, &wide_params(10));
}

/// Each lead candidate spans another follower block and is either in phrase
/// order (accepted) or inverted (rejected by the position check).
#[rstest]
fn wide_window_serves_phrases(#[values(0u32, 2)] slop: u32) {
    let num_docs = 30_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..30u32)
        .map(|i| (i * 1000, vec![i % 7]))
        .collect::<Vec<_>>();
    let middle = (0..15_000u32)
        .map(|i| {
            let doc = i * 2;
            (doc, vec![1 + (doc % 3)])
        })
        .collect::<Vec<_>>();
    let tail = (0..num_docs as u32)
        .map(|doc| (doc, vec![2 + (doc % 5)]))
        .collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_phrase_clause(0, 1.0, &lead, num_docs),
            wide_phrase_clause(1, 0.6, &middle, num_docs),
            wide_phrase_clause(2, 0.3, &tail, num_docs),
        ]
    };
    let mut params = wide_params(10);
    params.phrase_slop = Some(slop);
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &params);
    assert!(run.windows > 0);
}

#[test]
fn wide_window_serves_a_phrase_with_repeated_tokens() {
    let num_docs = 30_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..30u32)
        .map(|i| (i * 1000, vec![i % 5]))
        .collect::<Vec<_>>();
    let second = (0..15_000u32)
        .map(|i| (i * 2, vec![1 + (i % 4)]))
        .collect::<Vec<_>>();
    // The same token as the lead, one position later.
    let third = (0..num_docs as u32)
        .map(|doc| (doc, vec![1 + (doc % 5)]))
        .collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_phrase_clause(0, 1.0, &lead, num_docs),
            wide_phrase_clause(1, 0.7, &second, num_docs),
            wide_phrase_clause(2, 0.4, &third, num_docs),
        ]
    };
    let mut params = wide_params(10);
    params.phrase_slop = Some(1);
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &params);
    assert!(run.windows > 0);
}

/// Positions are checked after every follower has moved past the candidates'
/// blocks; the check must not decode those follower blocks a second time.
#[test]
fn wide_window_phrase_decodes_each_follower_block_once() {
    let num_docs = 30_000usize;
    let docs = unit_length_docs(num_docs);
    // Every lead document falls into its own block of both followers.
    let lead = (0..30u32)
        .map(|i| (i * 1000, vec![0u32]))
        .collect::<Vec<_>>();
    let middle = (0..15_000u32)
        .map(|i| (i * 2, vec![1u32]))
        .collect::<Vec<_>>();
    let tail = (0..num_docs as u32)
        .map(|doc| (doc, vec![2u32]))
        .collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_phrase_clause(0, 1.0, &lead, num_docs),
            wide_phrase_clause(1, 1.0, &middle, num_docs),
            wide_phrase_clause(2, 1.0, &tail, num_docs),
        ]
    };
    // Without a limit every candidate reaches the position check.
    let params = FtsSearchParams::default().with_phrase_slop(Some(0));
    let decodes = |wide: WideOverride| {
        let mut wand = Wand::new(Operator::And, build().into_iter(), &docs, UnitScorer)
            .with_bulk_and_mode(BulkAndMode::On)
            .with_bulk_and_wide(wide);
        let hits = wand.search(&params, &NoOpMetricsCollector).unwrap();
        assert_eq!(hits.len(), 30);
        let decodes = wand
            .lead
            .iter()
            .map(|posting| posting.frequency_blocks_decoded())
            .collect::<Vec<_>>();
        (decodes, wand.and_window_stats.wide_windows)
    };
    let (narrow, _) = decodes(WideOverride::Off);
    let (wide, windows) = decodes(WideOverride::Always);
    assert!(windows > 0);
    assert_eq!(wide, narrow);
    assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &params);
}

/// Most candidates of one window pass the intersection and then fail the
/// exact position check.
#[test]
fn wide_window_phrase_position_failures_stay_correct() {
    let num_docs = 20_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..20u32)
        .map(|i| {
            let pos = if i % 3 == 0 {
                vec![i % 5]
            } else {
                vec![50 + (i % 5)]
            };
            (i * 900, pos)
        })
        .collect::<Vec<_>>();
    let middle = (0..10_000u32)
        .map(|i| {
            let doc = i * 2;
            let pos = if doc % 3 == 0 {
                vec![1 + (doc % 5)]
            } else {
                vec![60 + (doc % 4)]
            };
            (doc, pos)
        })
        .collect::<Vec<_>>();
    let tail = (0..num_docs as u32)
        .map(|doc| (doc, vec![2 + (doc % 6)]))
        .collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_phrase_clause(0, 1.0, &lead, num_docs),
            wide_phrase_clause(1, 0.6, &middle, num_docs),
            wide_phrase_clause(2, 0.3, &tail, num_docs),
        ]
    };
    let mut params = wide_params(10);
    params.phrase_slop = Some(0);
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &params);
    assert!(run.windows > 0);
}

/// Phrase fixture over two spans of one lead block: the first span's three
/// documents fill the heap, the second holds two strong candidates around
/// doc 70_500, which only posts far-away positions.
fn lead_offset_fixture(middle_in_follower: bool) -> impl Fn() -> Vec<PostingIterator> {
    let num_docs = 80_000usize;
    let mut lead = vec![
        (0u32, (1..=4).collect::<Vec<u32>>()),
        (100, (1..=4).collect::<Vec<u32>>()),
        (200, (1..=3).collect::<Vec<u32>>()),
        (70_000, (1..=4).collect::<Vec<u32>>()),
        (71_000, (1..=4).collect::<Vec<u32>>()),
    ];
    let mut first = vec![
        (0u32, (2..=5).collect::<Vec<u32>>()),
        (100, (2..=4).collect::<Vec<u32>>()),
        (200, (2..=4).collect::<Vec<u32>>()),
        (70_000, (2..=5).collect::<Vec<u32>>()),
        (71_000, (2..=5).collect::<Vec<u32>>()),
    ];
    if middle_in_follower {
        // A lead frequency of one: the bound prunes it, not the intersection.
        lead.push((70_500, vec![50]));
        first.push((70_500, vec![51]));
    } else {
        lead.push((70_500, vec![50, 60, 70, 80]));
    }
    // Longer than the lead, so the lead stays the clause the cursor opens.
    first.extend((3_000..3_010u32).map(|doc| (doc, vec![2u32])));
    lead.sort_unstable_by_key(|&(doc, _)| doc);
    first.sort_unstable_by_key(|&(doc, _)| doc);
    move || {
        vec![
            wide_phrase_clause(0, 1.0, &lead, num_docs),
            wide_phrase_clause(1, 1.0, &first, num_docs),
        ]
    }
}

/// The last candidate is compacted over doc 70_500, so its position check must
/// read its own lead offset.
#[rstest]
#[case::dropped_by_the_intersection(false)]
#[case::dropped_by_the_bound(true)]
fn wide_window_phrase_keeps_the_lead_offset_of_a_compacted_candidate(
    #[case] middle_in_follower: bool,
) {
    let docs = unit_length_docs(80_000);
    let mut params = wide_params(3);
    params.phrase_slop = Some(0);
    let run = assert_wide_paths_agree(
        &lead_offset_fixture(middle_in_follower),
        &docs,
        &|| UnitScorer,
        &params,
    );
    assert!(run.windows > 0);
    let returned = run.rows.iter().map(|row| row.0).collect::<Vec<_>>();
    assert!(returned.contains(&70_000) && returned.contains(&71_000));
    assert!(!returned.contains(&70_500));
}

/// The last clause keeps two of six candidates; their clause columns must
/// travel with them through the compaction.
#[test]
fn wide_window_moves_survivor_columns() {
    let num_docs = 4_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..6u32).map(|i| (i * 10, 1 + i)).collect::<Vec<_>>();
    // Fillers keep the followers longer than the lead.
    let mut middle = (0..6u32).map(|i| (i * 10, 10 - i)).collect::<Vec<_>>();
    middle.extend((1_000..1_010u32).map(|doc| (doc, 1)));
    let mut tail = vec![(0u32, 3u32), (50u32, 4u32)];
    tail.extend((2_000..2_100u32).map(|doc| (doc, 1)));
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &middle, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(2, 1.0, &tail, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(10));
    assert!(run.windows > 0);
    let mut rows = run.rows;
    for row in rows.iter_mut() {
        row.2.sort_unstable();
    }
    assert_eq!(
        rows,
        vec![
            (0, 1, vec![(0, 1), (1, 10), (2, 3)]),
            (50, 1, vec![(0, 6), (1, 5), (2, 4)]),
        ]
    );
}

/// The span cap splits one lead block in two; in the second span a follower
/// block whose candidates are all out of reach is skipped without decoding.
#[test]
fn wide_window_skips_a_block_whose_candidates_all_fail() {
    let num_docs = 80_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = vec![(0u32, 9u32), (100, 9), (200, 9), (66_000, 9), (68_000, 1)];
    // The filler at 65_921 keeps the second span's block bound high while the
    // candidate at 68_000 is missing from this clause.
    let first = (0..num_docs as u32)
        .filter(|&doc| doc != 68_000)
        .map(|doc| (doc, if doc < 256 || doc == 65_921 { 9 } else { 1 }))
        .collect::<Vec<_>>();
    let tail = (0..num_docs as u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, MAX_POSTING_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &first, MAX_POSTING_BLOCK_SIZE, num_docs),
            wide_clause(2, 1.0, &tail, MAX_POSTING_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(3));
    assert!(run.windows > 0);
    assert_eq!(run.blocks_skipped, 2, "{run:?}");
    assert_eq!(run.rows.len(), 3);
}

/// A follower that runs out mid-window ends the conjunction, but the
/// candidates it kept must still clear the remaining clauses.
#[test]
fn wide_window_keeps_scoring_after_a_follower_is_exhausted() {
    let num_docs = 8_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..12u32).map(|i| (i * 700, 1)).collect::<Vec<_>>();
    let short = (0..2_000u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let tail = (0..num_docs as u32).map(|doc| (doc, 2)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &short, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(2, 1.0, &tail, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(100));
    assert!(run.windows > 0);
    assert_eq!(
        run.rows,
        vec![
            (0, 1, vec![(0, 1), (1, 1), (2, 2)]),
            (700, 1, vec![(0, 1), (1, 1), (2, 2)]),
            (1_400, 1, vec![(0, 1), (1, 1), (2, 2)]),
        ]
    );
}

#[test]
fn wide_window_stops_when_a_follower_is_exhausted() {
    let num_docs = 8_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..8u32).map(|i| (i * 1000, 1)).collect::<Vec<_>>();
    let short = (0..2_000u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 0.6, &short, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(100));
    assert!(run.windows > 0);
    assert_eq!(run.rows.len(), 2);
}

/// Candidates in the gap between two follower blocks must not keep the cursor
/// from reaching the next block.
#[test]
fn wide_window_crosses_follower_block_gaps() {
    let num_docs = 5_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..8u32).map(|i| (i * 700, 1)).collect::<Vec<_>>();
    let gapped = (0..100u32)
        .chain(2_800..2_900)
        .map(|doc| (doc, 1))
        .collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 0.6, &gapped, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(100));
    assert!(run.windows > 0);
    assert_eq!(run.rows.len(), 2, "0 and 2800 are in both clauses");
}

/// A skewed two-clause query intersects a sparse lead with a dense follower
/// one lead block at a time.
#[test]
fn wide_window_skips_follower_blocks_between_candidates() {
    let num_docs = 100_000usize;
    let docs = unit_length_docs(num_docs);
    let lead = [0u32, 1, 20_000, 20_001, 40_000, 40_001, 60_000, 60_001]
        .map(|doc| (doc, 1))
        .to_vec();
    let follower = (0..num_docs as u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 0.6, &follower, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(100));
    assert!(run.windows > 0);
}

/// The first lead block fills the heap with mediocre rows, the second holds
/// the winners, and the weak rest must be cut by the follower block bound.
#[test]
fn wide_window_prunes_against_a_rising_floor() {
    let num_docs = 2_048usize;
    let docs = unit_length_docs(num_docs);
    let ramp =
        |freq: u32, from: u32, to: u32| (from..to).map(|doc| (doc, freq)).collect::<Vec<_>>();
    let clause_a = ramp(5, 0, 128)
        .into_iter()
        .chain(ramp(6, 128, 256))
        .chain(ramp(1, 256, num_docs as u32))
        .collect::<Vec<_>>();
    let clause_b = clause_a.clone();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &clause_a, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &clause_b, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(3));
    assert!(run.windows > 0);
    assert!(run.blocks_skipped > 0, "{run:?}");
    assert_eq!(run.rows.len(), 3);
    assert_eq!(run.rows[0].0, 128, "the winners are the strong block");
}

#[test]
fn wide_window_respects_the_span_cap() {
    let num_docs = 120_000usize;
    let docs = unit_length_docs(num_docs);
    let rare = vec![(0u32, 1), (100_000, 1)];
    let dense = (0..num_docs as u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &rare, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 0.8, &dense, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(10));
    assert!(run.windows >= 2, "{run:?}");
    assert_eq!(run.rows.len(), 2);
}

/// A high floor retires the second lead block's candidates on their bound, and
/// a single-document lead still ends the query.
#[test]
fn wide_window_skips_the_last_lead_block_on_the_bound() {
    let num_docs = 8_192usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..256u32)
        .map(|i| (i * 32, if i == 0 { 9 } else { 1 }))
        .collect::<Vec<_>>();
    let dense = (0..num_docs as u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let dense_for_rare = dense.clone();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &dense, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(1));
    assert_eq!(run.rows.len(), 1);
    assert!(run.windows >= 2);
    assert!(run.blocks_skipped > 0, "{run:?}");

    let rare = vec![(4_095u32, 1)];
    let rare_build = move || {
        vec![
            wide_clause(0, 1.0, &rare, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &dense_for_rare, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&rare_build, &docs, &|| UnitScorer, &wide_params(10));
    assert_eq!(run.rows.len(), 1);
}

/// Lead frequencies are decoded once per window with candidates to score, not
/// once per window: not for a span without lead documents, and without a floor
/// not for a window whose candidates all miss a follower.
#[test]
fn wide_window_decodes_lead_frequencies_only_for_windows_with_survivors() {
    let num_docs = 200_001usize;
    let docs = unit_length_docs(num_docs);
    // Doc 0 fills the heap; the span cap splits [65536, 131071] off the lead's
    // only block, and that span holds no lead document.
    let lead = vec![(0u32, 9u32), (200_000, 1)];
    let mut follower = (0..256u32)
        .map(|doc| (doc, if doc == 0 { 9 } else { 1 }))
        .collect::<Vec<_>>();
    follower.push((200_000, 1));
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, MAX_POSTING_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &follower, MAX_POSTING_BLOCK_SIZE, num_docs),
        ]
    };
    let run = run_wide_path(
        &build,
        &docs,
        &|| UnitScorer,
        &wide_params(1),
        BulkAndMode::On,
        WideOverride::Always,
    );
    assert_eq!(run.windows, 2, "{run:?}");
    assert_eq!(run.lead_freq_decodes, 1, "{run:?}");
    assert_eq!(run.rows.len(), 1);

    let num_docs = 4_096usize;
    let docs = unit_length_docs(num_docs);
    let odd_lead = (0..64u32).map(|i| (i * 64 + 1, 1)).collect::<Vec<_>>();
    let even_follower = (0..num_docs as u32 / 2)
        .map(|i| (i * 2, 1))
        .collect::<Vec<_>>();
    let disjoint = move || {
        vec![
            wide_clause(0, 1.0, &odd_lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &even_follower, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = assert_wide_paths_agree(&disjoint, &docs, &|| UnitScorer, &wide_params(10));
    assert!(run.windows > 0);
    assert!(run.rows.is_empty());
    assert_eq!(run.lead_freq_decodes, 0, "{run:?}");
}

/// The gate, not heap fullness, decides the window: an unfilled heap still
/// takes the wide window, which prunes nothing but skips follower blocks.
#[test]
fn wide_window_runs_before_the_heap_is_full() {
    let docs = unit_length_docs(8_192);
    let build = gate_fixture(8_192, 8_192, 2);
    // 384 matches, so a limit of 4096 never fills the heap.
    let roomy = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(4_096));
    assert_eq!(roomy.rows.len(), 384);
    assert!(roomy.windows > 0);
    let tight = assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &wide_params(1));
    assert_eq!(tight.rows, roomy.rows[..1].to_vec());
}

/// A positive shared floor with an unfilled heap: the wide window prunes later
/// blocks and matches the classic rows.
#[test]
fn wide_window_prunes_at_a_shared_floor_without_a_full_heap() {
    let num_docs = 8_192usize;
    let docs = unit_length_docs(num_docs);
    let lead = (0..256u32)
        .map(|i| (i * 32, if i == 0 { 9 } else { 1 }))
        .collect::<Vec<_>>();
    let dense = (0..num_docs as u32).map(|doc| (doc, 1)).collect::<Vec<_>>();
    let build = move || {
        vec![
            wide_clause(0, 1.0, &lead, LEGACY_BLOCK_SIZE, num_docs),
            wide_clause(1, 1.0, &dense, LEGACY_BLOCK_SIZE, num_docs),
        ]
    };
    let run = |mode: BulkAndMode, wide: WideOverride| {
        let floor = Arc::new(AtomicU32::new(5.0_f32.to_bits()));
        let mut wand = Wand::new(Operator::And, build().into_iter(), &docs, UnitScorer)
            .with_bulk_and_mode(mode)
            .with_bulk_and_wide(wide)
            .with_shared_threshold(floor);
        let rows = wand
            .search(&wide_params(4_096), &NoOpMetricsCollector)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.document)
            .collect::<Vec<_>>();
        (
            rows,
            wand.and_window_stats.wide_windows,
            wand.and_window_stats.wide_blocks_skipped,
        )
    };
    let (classic, ..) = run(BulkAndMode::Off, WideOverride::Off);
    let (rows, windows, skipped) = run(BulkAndMode::On, WideOverride::Auto);
    assert_eq!(classic.len(), 1);
    assert!(windows > 0);
    assert!(skipped > 0, "the shared floor must skip later blocks");
    assert_eq!(rows, classic);
}

#[test]
fn wide_paths_agree_without_a_limit() {
    let docs = unit_length_docs(8_192);
    let run = assert_wide_paths_agree(
        &gate_fixture(8_192, 8_192, 4),
        &docs,
        &|| UnitScorer,
        &FtsSearchParams::default(),
    );
    assert!(run.windows > 0);
}

#[test]
fn wide_paths_agree_with_inclusive_floor() {
    let docs = unit_length_docs(8_192);
    let build = gate_fixture(8_192, 8_192, 4);
    let run = |mode: BulkAndMode, wide: WideOverride| {
        let mut wand = Wand::new(Operator::And, build().into_iter(), &docs, UnitScorer)
            .with_bulk_and_mode(mode)
            .with_bulk_and_wide(wide)
            .with_floor_mode(CompetitiveFloorMode::Inclusive);
        let mut rows = wand
            .search(&wide_params(10), &NoOpMetricsCollector)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.document, hit.doc_length, hit.freqs, hit.score.to_bits()))
            .collect::<Vec<_>>();
        rows.sort_unstable();
        (rows, wand.and_window_stats.wide_windows)
    };
    let (classic, _) = run(BulkAndMode::Off, WideOverride::Off);
    let (rows, windows) = run(BulkAndMode::On, WideOverride::Auto);
    assert_eq!(rows, classic);
    assert!(windows > 0);
}

/// Four clauses over 256-document impact blocks intersect pairwise in narrow
/// windows that start without a floor. With the gate open, a lead block whose
/// narrow window already reaches its end keeps that narrow window, so one query
/// can alternate between wide windows and pairwise narrow ones.
#[test]
fn wide_and_pairwise_windows_share_one_query() {
    // Every region holds one 256-posting lead block: a dense lead over sparse
    // followers fits its narrow window, a sparse lead over dense followers
    // takes the wide one.
    let dense_lead = 256u32;
    let period = dense_lead + 8_192;
    let num_docs = (2 * period) as usize;
    let docs = unit_length_docs(num_docs);
    let in_dense_lead = move |doc: u32| doc % period < dense_lead;
    let lead = (0..num_docs as u32)
        .filter(|&doc| in_dense_lead(doc) || (doc % period - dense_lead).is_multiple_of(32))
        .map(|doc| (doc, 1 + doc % 3))
        .collect::<Vec<_>>();
    let followers = (1..=3u32)
        .map(|term| {
            (0..num_docs as u32)
                .filter(|&doc| {
                    if in_dense_lead(doc) {
                        doc % 16 == 0
                    } else {
                        doc % 4 != term
                    }
                })
                .map(|doc| (doc, 1 + (doc + term) % 5))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let build = move || {
        let mut postings = vec![wide_clause(0, 1.0, &lead, MAX_POSTING_BLOCK_SIZE, num_docs)];
        for (term, docs) in followers.iter().enumerate() {
            postings.push(wide_clause(
                term as u32 + 1,
                1.0,
                docs,
                MAX_POSTING_BLOCK_SIZE,
                num_docs,
            ));
        }
        postings
    };
    // Without a limit no window has a floor; with one, only the first does.
    for params in [FtsSearchParams::default(), wide_params(10)] {
        assert_wide_paths_agree(&build, &docs, &|| UnitScorer, &params);
        let auto = run_wide_path(
            &build,
            &docs,
            &|| UnitScorer,
            &params,
            BulkAndMode::On,
            WideOverride::Auto,
        );
        assert!(auto.windows > 0 && auto.pairwise_windows > 0, "{auto:?}");
    }
}

/// The production default routes a conjunction of four or more clauses with
/// impact data into the bulk path, for either block size, and opens the wide
/// window once the gate allows it.
#[rstest]
#[case::legacy_blocks(LEGACY_BLOCK_SIZE)]
#[case::modern_blocks(MAX_POSTING_BLOCK_SIZE)]
fn auto_routes_impacted_wide_conjunctions_into_bulk(#[case] block_size: usize) {
    let num_docs = MAX_POSTING_BLOCK_SIZE * 8;
    let docs = unit_length_docs(num_docs);
    let lead = (0..128u32).collect::<Vec<_>>();
    let dense = (0..num_docs as u32).collect::<Vec<_>>();
    let build = || {
        [&lead, &dense, &dense, &dense]
            .into_iter()
            .enumerate()
            .map(|(term, ids)| {
                let len = ids.len();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term as u32,
                    term as u32,
                    1.0,
                    generate_impact_posting_list_with_freqs_and_block_size(
                        ids.clone(),
                        vec![1; len],
                        vec![1; len],
                        block_size,
                    ),
                    num_docs,
                )
            })
            .collect::<Vec<_>>()
    };
    let mut wand = Wand::new(Operator::And, build().into_iter(), &docs, UnitScorer);
    let hits = wand
        .search(&wide_params(10), &NoOpMetricsCollector)
        .unwrap();
    assert_eq!(hits.len(), 10);
    assert!(wand.bulk_and_searches > 0);
    assert!(wand.and_window_stats.wide_windows > 0);
    let mut rows = hits
        .into_iter()
        .map(|hit| {
            (
                (hit.document, hit.doc_length, hit.freqs),
                hit.score.to_bits(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    let classic = run_wide_path(
        &build,
        &docs,
        &|| UnitScorer,
        &wide_params(10),
        BulkAndMode::Off,
        WideOverride::Off,
    );
    assert_eq!(
        rows,
        classic
            .rows
            .into_iter()
            .zip(classic.scores)
            .collect::<Vec<_>>()
    );
}

#[test]
fn frequency_bound_lut_tail_is_finite_for_impact_lists() {
    let num_docs = 512usize;
    let doc_ids = (0..num_docs as u32).collect::<Vec<_>>();
    let freqs = doc_ids
        .iter()
        .map(|&doc| if doc == 0 { 300 } else { 1 })
        .collect::<Vec<_>>();
    let list = generate_impact_posting_list_with_freqs_and_block_size(
        doc_ids,
        freqs,
        vec![1; num_docs],
        MAX_POSTING_BLOCK_SIZE,
    );
    let scorer = CountingBm25ShapeScorer {
        scored: Arc::new(AtomicUsize::new(0)),
    };
    let posting = PostingIterator::with_query_weight(String::from("t"), 0, 0, 1.0, list, num_docs);
    let lut = posting.frequency_bound_lut(&scorer);
    for (freq, bound) in lut.iter().enumerate().take(FREQ_LUT_BUCKETS - 1) {
        assert_eq!(*bound, posting.score(&scorer, freq as u32, 0));
    }
    assert_eq!(
        lut[FREQ_LUT_BUCKETS - 1],
        scorer.doc_weight_upper_bound().unwrap()
    );
}

#[test]
fn frequency_bound_lut_is_infinite_for_grouped_clauses() {
    let num_docs = 512usize;
    let doc_ids = (0..num_docs as u32).collect::<Vec<_>>();
    let list = generate_impact_posting_list_with_freqs_and_block_size(
        doc_ids,
        vec![7; num_docs],
        vec![1; num_docs],
        MAX_POSTING_BLOCK_SIZE,
    );
    let scorer = CountingBm25ShapeScorer {
        scored: Arc::new(AtomicUsize::new(0)),
    };
    let grouped_terms = Arc::<[GroupedTermScorer]>::from([GroupedTermScorer::new(1.0, &list)]);
    let posting = PostingIterator::with_query_weight(String::from("t"), 0, 0, 1.0, list, num_docs)
        .with_grouped_terms(grouped_terms);
    assert!(
        posting
            .frequency_bound_lut(&scorer)
            .iter()
            .all(|bound| *bound == f32::INFINITY)
    );
}

/// Lead documents beyond the last frequency bucket must be bounded by the
/// finite tail bucket, not dropped by it. The best document of the first lead
/// block sets the floor; the winner sits in the second lead block, so both
/// bulk paths meet it with that floor in place, and only its lead frequency
/// lifts it above the floor.
#[test]
fn bulk_frequency_prune_agrees_with_classic_at_high_lead_frequency() {
    let num_docs = (MAX_POSTING_BLOCK_SIZE * 4) as u32;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), 1);
    }
    let run = |mode, wide| {
        let lead_docs = (0..num_docs).step_by(2).collect::<Vec<_>>();
        let lead_freqs = lead_docs
            .iter()
            .map(|&doc| match doc {
                0 => 100,
                768 => 200,
                _ => 1,
            })
            .collect::<Vec<_>>();
        let follower_docs = (0..num_docs).collect::<Vec<_>>();
        let follower_freqs = vec![1; follower_docs.len()];
        let clauses = [
            (lead_docs, lead_freqs),
            (follower_docs.clone(), follower_freqs.clone()),
            (follower_docs, follower_freqs),
        ];
        let postings = clauses
            .into_iter()
            .enumerate()
            .map(|(term, (doc_ids, freqs))| {
                let len = doc_ids.len();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term as u32,
                    term as u32,
                    1.0,
                    generate_impact_posting_list_with_freqs_and_block_size(
                        doc_ids,
                        freqs,
                        vec![1; len],
                        MAX_POSTING_BLOCK_SIZE,
                    ),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>();
        let mut wand = Wand::new(
            Operator::And,
            postings.into_iter(),
            &docs,
            CountingBm25ShapeScorer {
                scored: Arc::new(AtomicUsize::new(0)),
            },
        )
        .with_bulk_and_mode(mode)
        .with_bulk_and_wide(wide);
        let hits = wand
            .search(&wide_params(1), &NoOpMetricsCollector)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.document, hit.doc_length, hit.freqs, hit.score.to_bits()))
            .collect::<Vec<_>>();
        (hits, wand.threshold, wand.and_window_stats.wide_windows)
    };
    let (classic, classic_floor, _) = run(BulkAndMode::Off, WideOverride::Off);
    assert_eq!(classic[0].0, 768);
    // The gate keeps this corpus narrow; the forced wide window gives the
    // second lead block a window of its own, after the first set the floor.
    let (bulk, bulk_floor, _) = run(BulkAndMode::On, WideOverride::Auto);
    assert_eq!(bulk, classic);
    assert_eq!(bulk_floor, classic_floor);
    let (wide, wide_floor, windows) = run(BulkAndMode::On, WideOverride::Always);
    assert!(windows >= 2, "{windows}");
    assert_eq!(wide, classic);
    assert_eq!(wide_floor, classic_floor);
}

/// Contiguous doc ids where every document of a block shares that block's
/// frequency and length, so a block's exact contribution has a closed form.
struct BlockBoundFixture {
    posting: PostingIterator,
    num_blocks: usize,
}

impl BlockBoundFixture {
    const QUERY_WEIGHT: f32 = 1.7;

    fn new(num_blocks: usize, block_size: usize) -> Self {
        let total = num_blocks * block_size;
        let doc_ids = (0..total as u32).collect::<Vec<_>>();
        let freqs = (0..total)
            .map(|index| Self::freq_of(index / block_size))
            .collect::<Vec<_>>();
        let doc_lengths = (0..total)
            .map(|index| Self::len_of(index / block_size))
            .collect::<Vec<_>>();
        let list = if block_size == MAX_POSTING_BLOCK_SIZE {
            generate_impact_posting_list_with_freqs_and_block_size(
                doc_ids,
                freqs,
                doc_lengths,
                block_size,
            )
        } else {
            let block_max_scores = (0..num_blocks).map(Self::exact).collect::<Vec<_>>();
            let max_score = block_max_scores.iter().copied().fold(0.0_f32, f32::max);
            generate_posting_list_with_freqs(
                doc_ids,
                freqs,
                max_score,
                Some(block_max_scores),
                true,
            )
        };
        Self {
            posting: PostingIterator::with_query_weight(
                String::from("term"),
                0,
                0,
                Self::QUERY_WEIGHT,
                list,
                total,
            ),
            num_blocks,
        }
    }

    fn freq_of(block: usize) -> u32 {
        1 + (block % 4) as u32
    }

    fn len_of(block: usize) -> u32 {
        8 + ((block * 13) % 61) as u32
    }

    fn exact(block: usize) -> f32 {
        Self::QUERY_WEIGHT
            * VariedBm25ShapeScorer.doc_weight(Self::freq_of(block), Self::len_of(block))
    }

    /// `exact` at the quantized length that impact entries score.
    fn quantized_exact(block: usize) -> f32 {
        let length = crate::scalar::inverted::index::dequantize_doc_length(
            crate::scalar::inverted::index::quantize_doc_length(Self::len_of(block)),
        );
        Self::QUERY_WEIGHT * VariedBm25ShapeScorer.doc_weight(Self::freq_of(block), length)
    }
}

/// `block_bound_at` is the maximum score of the block it names, in both posting
/// formats, and answers +infinity for a block past the list.
#[rstest]
#[case::impact_blocks(MAX_POSTING_BLOCK_SIZE)]
#[case::stored_block_maxima(LEGACY_BLOCK_SIZE)]
fn block_bound_at_bounds_every_document(#[case] block_size: usize) {
    let fixture = BlockBoundFixture::new(6, block_size);
    for block in 0..fixture.num_blocks {
        let bound = fixture
            .posting
            .block_bound_at(block, &VariedBm25ShapeScorer);
        // Stored maxima use the exact length; impact entries the quantized one.
        let expected = if block_size == MAX_POSTING_BLOCK_SIZE {
            BlockBoundFixture::quantized_exact(block)
        } else {
            BlockBoundFixture::exact(block)
        };
        assert_eq!(bound, expected, "block {block}");
    }
    for block in [fixture.num_blocks, fixture.num_blocks + 7] {
        assert_eq!(
            fixture
                .posting
                .block_bound_at(block, &VariedBm25ShapeScorer),
            f32::INFINITY
        );
    }
}

/// A malformed impact entry must read as "cannot prune" while its neighbours
/// keep answering.
#[test]
fn block_bound_at_reports_infinity_for_a_corrupt_entry() {
    let fixture = BlockBoundFixture::new(6, MAX_POSTING_BLOCK_SIZE);
    let PostingList::Compressed(ref list) = fixture.posting.list else {
        panic!("the fixture is compressed");
    };
    let impacts = list.impacts.as_ref().expect("the fixture carries impacts");
    let mut entries = arrow_array::builder::LargeBinaryBuilder::new();
    for index in 0..impacts.entries().len() {
        if index == 2 {
            entries.append_null();
        } else {
            entries.append_value(impacts.entries().value(index));
        }
    }
    let mut list = list.clone();
    list.impacts = Some(ImpactSkipData::new(entries.finish(), impacts.level0_len()).unwrap());
    let corrupted = PostingIterator::with_query_weight(
        String::from("term"),
        0,
        0,
        BlockBoundFixture::QUERY_WEIGHT,
        PostingList::Compressed(list),
        fixture.num_blocks * MAX_POSTING_BLOCK_SIZE,
    );
    let scorer = VariedBm25ShapeScorer;
    assert_eq!(corrupted.block_bound_at(2, &scorer), f32::INFINITY);
    assert!(corrupted.block_bound_at(1, &scorer).is_finite());
    assert!(corrupted.block_bound_at(3, &scorer).is_finite());
}
