// Parity and safety tests for the region bulk AND kernel.

#[allow(clippy::too_many_arguments)]
fn region_rows<S: Scorer>(
    postings: Vec<PostingIterator>,
    docs: &DocSet,
    scorer: S,
    limit: usize,
    kernel: Option<BulkAndKernel>,
    intersect: Option<RegionIntersect>,
    exact: Option<ExactPolicy>,
    bound: Option<RegionBound>,
    floor: Option<f32>,
) -> (Vec<RegionHit>, u32, usize, usize, usize, usize) {
    let shared = Arc::new(AtomicU32::new(floor.unwrap_or(0.0).to_bits()));
    let mode = if kernel == Some(BulkAndKernel::Region) {
        BulkAndMode::Auto
    } else {
        BulkAndMode::Off
    };
    let mut wand = Wand::new(Operator::And, postings.into_iter(), docs, scorer)
        .with_bulk_and_mode(mode)
        .with_shared_threshold(shared.clone());
    if let Some(kernel) = kernel {
        wand = wand.with_bulk_and_kernel(kernel);
    }
    if let Some(intersect) = intersect {
        wand = wand.with_region_intersect(intersect);
    }
    if let Some(exact) = exact {
        wand = wand.with_exact_policy(exact);
    }
    if let Some(bound) = bound {
        wand = wand.with_region_bound(bound);
    }
    let mut rows = wand
        .search(
            &FtsSearchParams::default().with_limit(Some(limit)),
            &NoOpMetricsCollector,
        )
        .unwrap()
        .into_iter()
        .map(|candidate| {
            (
                candidate.posting_doc_id,
                candidate.doc_length,
                candidate.freqs,
                candidate.document,
            )
        })
        .collect::<Vec<_>>();
    rows.sort_unstable();
    (
        rows,
        shared.load(Ordering::Relaxed),
        wand.region_and_searches,
        wand.regions,
        wand.regions_skipped,
        wand.segments_pruned,
    )
}

fn assert_region_matches<S: Scorer + Clone>(
    postings: &dyn Fn() -> Vec<PostingIterator>,
    docs: &DocSet,
    scorer: S,
    limit: usize,
    intersect: Option<RegionIntersect>,
    exact: Option<ExactPolicy>,
    bound: Option<RegionBound>,
) {
    let (classic, classic_floor, classic_region, _, _, _) = region_rows(
        postings(),
        docs,
        scorer.clone(),
        limit,
        None,
        None,
        None,
        None,
        None,
    );
    let (region, region_floor, searches, _, _, _) = region_rows(
        postings(),
        docs,
        scorer,
        limit,
        Some(BulkAndKernel::Region),
        intersect,
        exact,
        bound,
        None,
    );
    assert_eq!(classic_region, 0);
    assert_eq!(searches, 1);
    assert_eq!(region, classic);
    assert_eq!(region_floor, classic_floor);
}

fn varied_clauses(
    num_docs: u32,
    num_clauses: usize,
) -> (DocSet, impl Fn() -> Vec<PostingIterator>) {
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), 8 + doc_id % 17);
    }
    let docs_for_build = docs.clone();
    let build = move || {
        (0..num_clauses)
            .map(|term| {
                let doc_ids = (0..num_docs)
                    .filter(|doc| doc.wrapping_add(term as u32) % (2 + term as u32) == 0)
                    .collect::<Vec<_>>();
                let freqs = doc_ids.iter().map(|doc| 1 + doc % 5).collect::<Vec<_>>();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term as u32,
                    term as u32,
                    0.5 + term as f32,
                    generate_posting_list_with_freqs(doc_ids, freqs, 8.0, None, true),
                    docs_for_build.len(),
                )
            })
            .collect()
    };
    (docs, build)
}

#[test]
fn region_matches_classic_across_widths_limits_and_intersect() {
    let (docs, build) = varied_clauses(BLOCK_SIZE as u32 * 3, 2);
    for limit in [1usize, 3, 10] {
        for intersect in [
            Some(RegionIntersect::Gallop),
            Some(RegionIntersect::Merge),
            None,
        ] {
            assert_region_matches(&build, &docs, UnitScorer, limit, intersect, None, None);
        }
    }
    for clauses in [3usize, 4, 5, 8] {
        let (docs, build) = varied_clauses(BLOCK_SIZE as u32 * 2, clauses);
        assert_region_matches(
            &build,
            &docs,
            UnitScorer,
            10,
            Some(RegionIntersect::Gallop),
            None,
            None,
        );
        assert_region_matches(
            &build,
            &docs,
            UnitScorer,
            10,
            Some(RegionIntersect::Merge),
            None,
            None,
        );
    }
}

#[test]
fn region_publishes_query_order_score_bits() {
    let contributions = [0.002_972_301_6_f32, 0.001_982_450_7_f32, 0.001_882_293_f32];
    let lengths = [3usize, 1, 2];
    let mut docs = DocSet::default();
    for doc_id in 0..3 {
        docs.append(doc_id, 1);
    }
    let build = || {
        contributions
            .into_iter()
            .zip(lengths)
            .enumerate()
            .map(|(position, (weight, len))| {
                let doc_ids = (0..len as u32).collect::<Vec<_>>();
                PostingIterator::with_query_weight(
                    format!("t{position}"),
                    position as u32,
                    position as u32,
                    weight,
                    generate_posting_list(doc_ids, weight, None, true),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    let query_order = contributions
        .into_iter()
        .fold(0.0_f32, |sum, value| sum + value);
    // Cost order is the short list first: weights of lengths 1, 2, 3.
    let cost_order = contributions[1] + contributions[2] + contributions[0];
    assert_ne!(query_order.to_bits(), cost_order.to_bits());
    let (_rows, floor, searches, _, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        1,
        Some(BulkAndKernel::Region),
        None,
        Some(ExactPolicy::Always),
        None,
        None,
    );
    assert_eq!(searches, 1);
    assert_eq!(floor, query_order.to_bits());
    let (_classic, classic_floor, _, _, _, _) =
        region_rows(build(), &docs, UnitScorer, 1, None, None, None, None, None);
    assert_eq!(classic_floor, query_order.to_bits());
}

#[test]
fn region_skips_follower_blocks_without_candidates() {
    let num_docs = BLOCK_SIZE as u32 * 20;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), 4);
    }
    let lead_docs = vec![0, 10, 20];
    let follower_docs = (0..num_docs).collect::<Vec<_>>();
    let build = || {
        vec![
            PostingIterator::new(
                "lead".into(),
                0,
                0,
                generate_posting_list(lead_docs.clone(), 1.0, None, true),
                docs.len(),
            ),
            PostingIterator::new(
                "fol".into(),
                1,
                1,
                generate_posting_list(follower_docs.clone(), 1.0, None, true),
                docs.len(),
            ),
        ]
    };
    let (rows, _, searches, regions, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        10,
        Some(BulkAndKernel::Region),
        None,
        None,
        None,
        None,
    );
    let (classic, _, _, _, _, _) =
        region_rows(build(), &docs, UnitScorer, 10, None, None, None, None, None);
    assert_eq!(rows, classic);
    assert_eq!(searches, 1);
    assert!(regions >= 1);
    let mut wand = Wand::new(Operator::And, build().into_iter(), &docs, UnitScorer)
        .with_bulk_and_kernel(BulkAndKernel::Region);
    wand.search(
        &FtsSearchParams::default().with_limit(Some(10)),
        &NoOpMetricsCollector,
    )
    .unwrap();
    assert!(
        wand.follower_doc_blocks_decoded < 10,
        "decoded {} follower blocks",
        wand.follower_doc_blocks_decoded
    );
}

#[test]
fn region_stops_when_follower_is_exhausted_and_crosses_block_gaps() {
    let mut docs = DocSet::default();
    for doc_id in 0u32..=5_000 {
        docs.append(u64::from(doc_id), 4);
    }
    // Follower blocks: 128 docs would be one block. Force a gap by putting
    // 128 early docs and one late doc, while the lead also asks for a doc
    // that sits between those blocks.
    let mut follower = (0..BLOCK_SIZE as u32).collect::<Vec<_>>();
    follower.push(5_000);
    let lead = vec![0, 50, 5_000];
    let build = || {
        vec![
            PostingIterator::new(
                "lead".into(),
                0,
                0,
                generate_posting_list(lead.clone(), 1.0, None, true),
                docs.len(),
            ),
            PostingIterator::new(
                "fol".into(),
                1,
                1,
                generate_posting_list(follower.clone(), 1.0, None, true),
                docs.len(),
            ),
        ]
    };
    // Doc 50 is inside the first follower block (0..128). Doc 5000 is the
    // next block. Both must survive; the follower has no doc past 5000.
    assert_region_matches(&build, &docs, UnitScorer, 10, None, None, None);
}

#[test]
fn region_limit_ladder_matches_classic() {
    let num_docs = BLOCK_SIZE as u32 * 4;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), 8);
    }
    let build = || {
        (0..3)
            .map(|term| {
                let doc_ids = (0..num_docs).collect::<Vec<_>>();
                let freqs = doc_ids
                    .iter()
                    .map(|doc| 1 + (doc + term) % 9)
                    .collect::<Vec<_>>();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term,
                    term,
                    1.0 + term as f32 * 0.25,
                    generate_posting_list_with_freqs(doc_ids, freqs, 64.0, None, true),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    for limit in [1usize, 3, 10, 100] {
        assert_region_matches(&build, &docs, UnitScorer, limit, None, None, None);
    }
}

#[test]
fn region_span_splits_a_rare_lead_and_a_high_floor_terminates() {
    let far = (1u32 << 16) + 4_000;
    let mut docs = DocSet::default();
    for doc_id in 0..=far {
        docs.append(u64::from(doc_id), 4);
    }
    let ids = vec![0, far];
    let build = || {
        vec![
            PostingIterator::new(
                "lead".into(),
                0,
                0,
                generate_posting_list(ids.clone(), 1.0, None, true),
                docs.len(),
            ),
            PostingIterator::new(
                "fol".into(),
                1,
                1,
                generate_posting_list(ids.clone(), 1.0, None, true),
                docs.len(),
            ),
        ]
    };
    let (rows, _, searches, regions, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        10,
        Some(BulkAndKernel::Region),
        None,
        None,
        None,
        None,
    );
    let (classic, _, _, _, _, _) =
        region_rows(build(), &docs, UnitScorer, 10, None, None, None, None, None);
    assert_eq!(rows, classic);
    assert_eq!(searches, 1);
    assert!(regions >= 2, "regions {regions}");

    let (skipped_rows, _, _, _, skipped, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        10,
        Some(BulkAndKernel::Region),
        None,
        None,
        None,
        Some(1.0e20),
    );
    assert!(skipped_rows.is_empty());
    assert!(
        skipped > 0,
        "a huge floor must skip regions and still return"
    );
}

#[test]
fn region_exact_policies_agree_and_always_prunes() {
    let num_docs = BLOCK_SIZE as u32 * 6;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs {
        docs.append(u64::from(doc_id), 4);
    }
    let hot = 0u32;
    let later = BLOCK_SIZE as u32 * 4;
    let build = || {
        (0..2)
            .map(|term| {
                let doc_ids = (0..num_docs).collect::<Vec<_>>();
                let freqs = doc_ids
                    .iter()
                    .map(|doc| if *doc == hot || *doc == later { 40 } else { 1 })
                    .collect::<Vec<_>>();
                let _ = doc_ids.len();
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term,
                    term,
                    1.0,
                    generate_posting_list_with_freqs(doc_ids, freqs, 40.0, None, true),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    let (always, _, _, _, _, pruned) = region_rows(
        build(),
        &docs,
        UnitScorer,
        1,
        Some(BulkAndKernel::Region),
        None,
        Some(ExactPolicy::Always),
        None,
        None,
    );
    let (never, _, _, _, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        1,
        Some(BulkAndKernel::Region),
        None,
        Some(ExactPolicy::Never),
        None,
        None,
    );
    let (auto, _, _, _, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        1,
        Some(BulkAndKernel::Region),
        None,
        Some(ExactPolicy::Auto),
        None,
        None,
    );
    let (classic, _, _, _, _, _) =
        region_rows(build(), &docs, UnitScorer, 1, None, None, None, None, None);
    assert_eq!(always, classic);
    assert_eq!(never, classic);
    assert_eq!(auto, classic);
    assert!(pruned > 0, "Always must prune at least one segment");
}

#[derive(Clone)]
struct LengthSensitiveScorer;

impl Scorer for LengthSensitiveScorer {
    fn query_weight(&self, _token: &str) -> f32 {
        1.0
    }

    fn doc_weight(&self, freq: u32, doc_tokens: u32) -> f32 {
        freq as f32 / (doc_tokens.max(1) as f32)
    }
}

#[test]
fn region_score_paths_match_and_excluded_shapes_stay_on_main() {
    let num_docs = 64u32;
    let mut plain = DocSet::default();
    let mut quantized = DocSet::default();
    for doc_id in 0..num_docs {
        plain.append(u64::from(doc_id), 3 + doc_id % 11);
        quantized.append(u64::from(doc_id), 3 + doc_id % 11);
    }
    quantized.set_quantized_scoring(true);
    let build = |docs: &DocSet| {
        (0..2)
            .map(|term| {
                let doc_ids = (0..num_docs)
                    .filter(|doc| doc % (2 + term) == 0)
                    .collect::<Vec<_>>();
                let freqs = vec![1 + term; doc_ids.len()];
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term,
                    term,
                    1.0,
                    generate_posting_list_with_freqs(doc_ids, freqs, 2.0, None, true),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_region_matches(&|| build(&plain), &plain, UnitScorer, 10, None, None, None);
    assert_region_matches(
        &|| build(&quantized),
        &quantized,
        LengthSensitiveScorer,
        10,
        None,
        None,
        None,
    );
    let scorer = MemBM25Scorer::new(
        num_docs as u64 * 8,
        num_docs as usize,
        std::collections::HashMap::new(),
    );
    assert_region_matches(
        &|| build(&quantized),
        &quantized,
        scorer,
        10,
        None,
        None,
        None,
    );

    let mut guarded = build(&plain);
    guarded[0] = guarded[0].fork_from_start().with_scorer_upper_bound();
    let (_, _, searches, _, _, _) = region_rows(
        guarded,
        &plain,
        UnitScorer,
        10,
        Some(BulkAndKernel::Region),
        None,
        None,
        None,
        None,
    );
    assert_eq!(searches, 0, "scorer-ceiling postings do not enter region");
}

#[test]
fn region_partial_prune_keeps_the_segment_tail_doc() {
    let mut docs = DocSet::default();
    for doc_id in 0..BLOCK_SIZE as u32 * 3 {
        docs.append(u64::from(doc_id), 4);
    }
    // Same follower block holds a strong doc and a weak tail doc. A later
    // doc must still be found after the weak tail is pruned.
    let strong = 1u32;
    let weak = 2u32;
    let later = BLOCK_SIZE as u32 + 5;
    let build = || {
        (0..2)
            .map(|term| {
                let doc_ids = vec![strong, weak, later];
                let freqs = vec![30, 1, 30];
                PostingIterator::with_query_weight(
                    format!("t{term}"),
                    term,
                    term,
                    1.0,
                    generate_posting_list_with_freqs(doc_ids, freqs, 30.0, None, true),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_region_matches(
        &build,
        &docs,
        UnitScorer,
        2,
        None,
        Some(ExactPolicy::Always),
        None,
    );
}

fn impact_bytes(doc_up_to: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    crate::scalar::inverted::encoding::encode_varint_u32(&mut bytes, doc_up_to);
    crate::scalar::inverted::encoding::encode_varint_u32(&mut bytes, 1);
    bytes.push(0);
    bytes
}

fn posting_with_impacts(
    doc_ids: Vec<u32>,
    impacts: super::super::impact::ImpactSkipData,
) -> PostingIterator {
    let mut list = generate_posting_list(doc_ids.clone(), 1.0, None, true);
    let PostingList::Compressed(compressed) = &mut list else {
        unreachable!("compressed");
    };
    compressed.impacts = Some(impacts);
    PostingIterator::with_query_weight("t".into(), 0, 0, 1.0, list, doc_ids.len().max(1))
}

#[test]
fn region_bound_rejects_corrupt_impacts_and_stays_safe() {
    let doc_ids = (0..BLOCK_SIZE as u32 * 8).collect::<Vec<_>>();
    let entries = arrow_array::LargeBinaryArray::from_iter_values(
        (0..8)
            .map(|block| impact_bytes((block + 1) * BLOCK_SIZE as u32 - 1))
            .chain(std::iter::once(vec![1, 2, 3])),
    );
    let impacts = super::super::impact::ImpactSkipData::new(entries, 8).unwrap();
    let mut docs = DocSet::default();
    docs.append(0, 4);
    let posting = posting_with_impacts(doc_ids, impacts);
    let mut wand = Wand::new(Operator::And, std::iter::once(posting), &docs, UnitScorer);
    wand.lead[0].block_idx = 0;
    let bound = wand.region_bound(0, BLOCK_SIZE as u32 * 8 - 1);
    assert!(
        bound.is_infinite(),
        "corrupt level1 must not prune, got {bound}"
    );

    // Level0 past the impact directory: score_cached would return 0.
    let wide = (0..BLOCK_SIZE as u32 * 40).collect::<Vec<_>>();
    let short_entries = arrow_array::LargeBinaryArray::from_iter_values(
        (0..32)
            .map(|block| impact_bytes((block + 1) * BLOCK_SIZE as u32 - 1))
            .chain(std::iter::once(impact_bytes(u32::MAX - 1))),
    );
    let short = super::super::impact::ImpactSkipData::new(short_entries, 32).unwrap();
    let posting = posting_with_impacts(wide, short);
    let mut wand = Wand::new(Operator::And, std::iter::once(posting), &docs, UnitScorer);
    wand.lead[0].block_idx = 32;
    let bound = wand.region_bound(0, BLOCK_SIZE as u32 * 40 - 1);
    assert!(
        bound.is_infinite(),
        "out-of-range level1 must be +inf, not the cached 0, got {bound}"
    );

    let missing = super::super::impact::ImpactSkipData::new(
        arrow_array::LargeBinaryArray::from_iter_values([Vec::<u8>::new()]),
        1,
    );
    // An empty level0 blob is malformed. If construction fails, the direct
    // block_bound_at case below still pins the +inf contract.
    if let Ok(missing) = missing {
        let posting = posting_with_impacts(vec![0, 1, 2], missing);
        let bound = super::region::block_bound_at(&posting, &UnitScorer, 0);
        assert!(
            bound.is_infinite(),
            "missing level0 must be +inf, got {bound}"
        );
    }

    let mut docs = DocSet::default();
    for doc_id in 0..32 {
        docs.append(doc_id, 4);
    }
    let build = || {
        (0..2)
            .map(|term| {
                PostingIterator::with_query_weight(
                    format!("z{term}"),
                    term,
                    term,
                    0.0,
                    generate_impact_posting_list_with_freqs(
                        (0..32).collect(),
                        vec![1; 32],
                        vec![4; 32],
                    ),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>()
    };
    let (rows, _, _, _, skipped, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        10,
        Some(BulkAndKernel::Region),
        None,
        None,
        None,
        Some(1.0),
    );
    let (classic, _, _, _, _, _) = region_rows(
        build(),
        &docs,
        UnitScorer,
        10,
        None,
        None,
        None,
        None,
        Some(1.0),
    );
    assert_eq!(rows, classic);
    assert_eq!(skipped, 0, "non-positive bounds must not skip the region");
}

#[test]
fn region_bound_is_at_least_the_block_maximum() {
    let mut seed = 0x0f1e_2d3c_4b5a_6978u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for with_impacts in [false, true] {
        let n = BLOCK_SIZE * 6;
        let doc_ids = (0..n as u32).collect::<Vec<_>>();
        let freqs = (0..n).map(|_| 1 + (next() % 9) as u32).collect::<Vec<_>>();
        let lengths = vec![4u32; n];
        let list = if with_impacts {
            generate_impact_posting_list_with_freqs(doc_ids.clone(), freqs.clone(), lengths)
        } else {
            generate_posting_list_with_freqs(doc_ids.clone(), freqs.clone(), 20.0, None, true)
        };
        let posting = PostingIterator::with_query_weight("t".into(), 0, 0, 1.0, list, n);
        let mut docs = DocSet::default();
        docs.append(0, 4);
        let mut wand = Wand::new(Operator::And, std::iter::once(posting), &docs, UnitScorer);
        for _ in 0..12 {
            let lo = (next() as u32) % n as u32;
            let hi = lo + ((next() as u32) % (n as u32 - lo));
            let PostingList::Compressed(list) = &wand.lead[0].list else {
                unreachable!("compressed");
            };
            let b0 = wand.lead[0].block_idx_for_doc(list, 0, lo);
            let b1 = wand.lead[0].block_idx_for_doc(list, b0, hi);
            wand.lead[0].block_idx = b0;
            let bound = wand.region_bound(0, hi);
            for block in b0..=b1 {
                let block_bound = super::region::block_bound_at(&wand.lead[0], &UnitScorer, block);
                assert!(
                    bound >= block_bound,
                    "impacts {with_impacts} bound {bound} < block {block} {block_bound}"
                );
            }
            let deque = wand.lead[0]
                .block_max_score_up_to_with_stats(u64::from(hi), &UnitScorer)
                .score;
            assert!(
                bound >= deque || bound.is_infinite(),
                "bound {bound} deque {deque}"
            );
        }
    }

    // b0 ≡ 31 (mod 32): the group's last block holds the only large score.
    let blocks = 32;
    let n = blocks * BLOCK_SIZE;
    let doc_ids = (0..n as u32).collect::<Vec<_>>();
    let hot = (31 * BLOCK_SIZE) as u32;
    let freqs = doc_ids
        .iter()
        .map(|doc| if *doc == hot { 80 } else { 1 })
        .collect::<Vec<_>>();
    let posting = PostingIterator::with_query_weight(
        "edge".into(),
        0,
        0,
        1.0,
        generate_impact_posting_list_with_freqs(doc_ids, freqs, vec![4; n]),
        n,
    );
    let mut docs = DocSet::default();
    docs.append(0, 4);
    let mut wand = Wand::new(Operator::And, std::iter::once(posting), &docs, UnitScorer);
    wand.lead[0].block_idx = 31;
    let bound = wand.region_bound(0, hot);
    assert!(
        bound >= 80.0,
        "group-end block must be included, got {bound}"
    );

    // Four-block edge whose maximum sits on the last block.
    let blocks = 4;
    let n = blocks * BLOCK_SIZE;
    let doc_ids = (0..n as u32).collect::<Vec<_>>();
    let hot = (3 * BLOCK_SIZE) as u32;
    let freqs = doc_ids
        .iter()
        .map(|doc| if *doc == hot { 90 } else { 1 })
        .collect::<Vec<_>>();
    let posting = PostingIterator::with_query_weight(
        "tail".into(),
        0,
        0,
        1.0,
        generate_impact_posting_list_with_freqs(doc_ids, freqs, vec![4; n]),
        n,
    );
    let mut wand = Wand::new(Operator::And, std::iter::once(posting), &docs, UnitScorer);
    wand.lead[0].block_idx = 0;
    let bound = wand.region_bound(0, hot);
    assert!(
        bound >= 90.0,
        "edge block {hot} must be included, got {bound}"
    );

    let (docs, build) = varied_clauses(BLOCK_SIZE as u32 * 3, 3);
    assert_region_matches(
        &build,
        &docs,
        UnitScorer,
        5,
        None,
        None,
        Some(RegionBound::Level1),
    );
    assert_region_matches(
        &build,
        &docs,
        UnitScorer,
        5,
        None,
        None,
        Some(RegionBound::Deque),
    );
}

fn positioned(
    token: &str,
    token_id: u32,
    query_pos: u32,
    rows: &[(u32, Vec<u32>)],
    num_docs: usize,
) -> PostingIterator {
    let doc_ids = rows.iter().map(|(doc, _)| *doc).collect::<Vec<_>>();
    let positions = rows.iter().map(|(_, pos)| pos.clone()).collect::<Vec<_>>();
    PostingIterator::with_query_weight(
        token.to_string(),
        token_id,
        query_pos,
        1.0,
        generate_posting_list_with_positions(doc_ids, positions, 4.0, true),
        num_docs,
    )
}

fn phrase_ids(
    postings: Vec<PostingIterator>,
    docs: &DocSet,
    slop: Option<u32>,
    limit: usize,
    region: bool,
) -> (Vec<u64>, usize) {
    let mut params = FtsSearchParams::default().with_limit(Some(limit));
    params.phrase_slop = slop;
    let mut wand = Wand::new(Operator::And, postings.into_iter(), docs, UnitScorer)
        .with_bulk_and_mode(if region {
            BulkAndMode::Auto
        } else {
            BulkAndMode::Off
        });
    if region {
        wand = wand.with_bulk_and_kernel(BulkAndKernel::Region);
    }
    let mut ids = wand
        .search(&params, &NoOpMetricsCollector)
        .unwrap()
        .into_iter()
        .map(|hit| hit.document)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    (ids, wand.pos_state_decodes)
}

#[test]
fn region_phrase_matches_across_blocks_and_slop() {
    let num_docs = 201usize;
    let mut docs = DocSet::default();
    for doc_id in 0..num_docs as u32 {
        docs.append(u64::from(doc_id), 8);
    }
    // The long clause puts doc 200 in its second block. The short clause keeps
    // that same doc in block 0, so one candidate's positions span blocks.
    let mut long_rows = (0..BLOCK_SIZE as u32)
        .map(|doc| (doc, vec![30]))
        .collect::<Vec<_>>();
    long_rows[0].1 = vec![10];
    long_rows[10].1 = vec![10];
    long_rows[12].1 = vec![3];
    long_rows.push((200, vec![4]));
    let short_rows = vec![(0, vec![11]), (10, vec![11]), (12, vec![6]), (200, vec![5])];
    let build = || {
        vec![
            positioned("a", 0, 0, &long_rows, num_docs),
            positioned("b", 1, 1, &short_rows, num_docs),
        ]
    };
    for slop in [0u32, 2] {
        let (region, decodes) = phrase_ids(build(), &docs, Some(slop), 10, true);
        let (classic, _) = phrase_ids(build(), &docs, Some(slop), 10, false);
        assert_eq!(region, classic, "slop {slop}");
        assert_eq!(decodes, 3, "each clause block is decoded once, slop {slop}");
    }
    let (slop0, _) = phrase_ids(build(), &docs, Some(0), 10, true);
    assert!(slop0.contains(&0) && slop0.contains(&10) && slop0.contains(&200));
    assert!(!slop0.contains(&12));
    let (slop2, _) = phrase_ids(build(), &docs, Some(2), 10, true);
    assert!(slop2.contains(&12), "slop 2 keeps the query-order pair");
}

#[test]
fn region_phrase_keeps_later_candidate_after_a_position_miss() {
    let mut docs = DocSet::default();
    for doc_id in 0..8u32 {
        docs.append(u64::from(doc_id), 8);
    }
    let build = || {
        vec![
            positioned("a", 0, 0, &[(0, vec![1]), (1, vec![9]), (2, vec![3])], 8),
            positioned("b", 1, 1, &[(0, vec![2]), (1, vec![1]), (2, vec![4])], 8),
        ]
    };
    let (region, _) = phrase_ids(build(), &docs, Some(0), 10, true);
    let (classic, _) = phrase_ids(build(), &docs, Some(0), 10, false);
    assert_eq!(region, classic);
    assert_eq!(region, vec![0, 2]);
}

#[test]
fn region_repeated_tokens_match_phrase_and_conjunction() {
    let mut docs = DocSet::default();
    for doc_id in 0..4u32 {
        docs.append(u64::from(doc_id), 8);
    }
    let tokens = ["to", "be", "or", "not", "to", "be"];
    let phrase_doc = [0u32, 1, 2, 3, 4, 5];
    let scrambled = [5u32, 4, 0, 1, 2, 3];
    let build = || {
        tokens
            .iter()
            .enumerate()
            .map(|(pos, token)| {
                positioned(
                    token,
                    pos as u32,
                    pos as u32,
                    &[(0, vec![phrase_doc[pos]]), (1, vec![scrambled[pos]])],
                    4,
                )
            })
            .collect::<Vec<_>>()
    };
    let (phrase, _) = phrase_ids(build(), &docs, Some(0), 10, true);
    let (classic_phrase, _) = phrase_ids(build(), &docs, Some(0), 10, false);
    assert_eq!(phrase, classic_phrase);
    assert_eq!(phrase, vec![0]);
    let (conjunction, _) = phrase_ids(build(), &docs, None, 10, true);
    let (classic_and, _) = phrase_ids(build(), &docs, None, 10, false);
    assert_eq!(conjunction, classic_and);
    assert_eq!(conjunction, vec![0, 1]);
}

#[test]
#[ignore = "release timing for S6 constants; not part of wand::tests"]
fn region_primitive_times() {
    let iters = 200u32;
    let started = std::time::Instant::now();
    let mut acc = 0.0f32;
    for i in 0..iters * 1000 {
        acc += super::super::scorer::bm25_doc_weight_with_norm(1 + (i % 32), 0.75);
    }
    let bm25_ns = started.elapsed().as_nanos() as f64 / f64::from(iters * 1000);
    assert!(acc.is_finite());

    let mut bytes = vec![0u8; 5 * 1024 * 1024];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = (i % 251) as u8;
    }
    let mut gather = 0u64;
    let started = std::time::Instant::now();
    let gathers = 128usize * 2000;
    for i in 0..gathers {
        gather += u64::from(bytes[(i * 997) % bytes.len()]);
    }
    let norm_ns = started.elapsed().as_nanos() as f64 / gathers as f64;
    assert!(gather > 0);

    let docs = (0..BLOCK_SIZE as u32).collect::<Vec<_>>();
    let list = generate_posting_list(docs, 1.0, None, true);
    let posting = PostingIterator::new("t".into(), 0, 0, list, BLOCK_SIZE);
    let PostingList::Compressed(compressed) = &posting.list else {
        panic!("compressed");
    };
    let started = std::time::Instant::now();
    for _ in 0..iters {
        let state = unsafe { &mut *posting.compressed_state_ptr() };
        state.doc_ids.clear();
        state.block_idx = usize::MAX;
        let decoded = unsafe { &*posting.ensure_compressed_doc_ids_ptr(compressed, 0) };
        assert_eq!(decoded.doc_ids.len(), BLOCK_SIZE);
    }
    let dec_ns = started.elapsed().as_nanos() as f64 / f64::from(iters);

    let state = unsafe { &*posting.compressed_state_ptr() };
    let doc_ids = state.doc_ids.clone();
    let started = std::time::Instant::now();
    let mut pos = 0usize;
    for i in 0..10_000u32 {
        pos = unsafe { super::find_next_geq(doc_ids.as_ptr(), 0, doc_ids.len(), i % 128) };
    }
    let geq_ns = started.elapsed().as_nanos() as f64 / 10_000.0;
    assert!(pos < doc_ids.len());

    let left: Vec<u32> = (0..64).map(|i| i * 2).collect();
    let right: Vec<u32> = (0..128).collect();
    let mut out = Vec::new();
    let started = std::time::Instant::now();
    for _ in 0..2_000 {
        super::intersection::intersect_indices(&left, &right, &mut out);
    }
    let merge_ns =
        started.elapsed().as_nanos() as f64 / (2_000.0 * (left.len() + right.len()) as f64);
    assert!(!out.is_empty());

    eprintln!(
        "CALIB bm25_ns={bm25_ns:.2} norm_ns={norm_ns:.2} dec_ns={dec_ns:.1} geq_ns={geq_ns:.2} merge_per_elem_ns={merge_ns:.3}"
    );
}
