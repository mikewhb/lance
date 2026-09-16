// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

// Opt-in microbenchmark for phrase-position confirmation.
//
// Run the batched pass with:
// `LANCE_PHRASE_POSITION_BENCH=1 cargo test -p lance-index phrase_position_bench --release -- --nocapture`.
// Add `LANCE_PHRASE_POSITION_BENCH_PERCALL=1` for single-call p50/p95 samples.

use super::*;

const ITERS: usize = 200;
const PERCALL_ITERS: usize = 2000;

/// Emit one row for `path`.  The two modes are intentionally separate because
/// collecting single-call samples changes cache state for following paths.
#[allow(clippy::print_stdout)] // Benchmark output is the machine-readable result.
fn bench_phrase_row<F: FnMut() -> bool>(name: &str, path: &str, matched: bool, batch: usize, f: F) {
    if std::env::var_os("LANCE_PHRASE_POSITION_BENCH_PERCALL").is_some() {
        let (p50, p95) = bench_phrase_percall(PERCALL_ITERS, f);
        println!("{name},{path},{matched},,,,{PERCALL_ITERS},{p50},{p95}");
    } else {
        let (mean, p50, p95) = bench_phrase_time(ITERS, batch, f);
        println!("{name},{path},{matched},{mean},{p50},{p95},{ITERS},,");
    }
}

/// Choose one batch size per fixture so all paths retain the same granularity.
fn bench_phrase_batch<F: FnMut() -> bool>(mut f: F) -> usize {
    let start = std::time::Instant::now();
    for _ in 0..8 {
        std::hint::black_box(f());
    }
    let estimate = start.elapsed().as_nanos() / 8;
    if estimate < 500 {
        64
    } else if estimate < 5_000 {
        8
    } else {
        1
    }
}

/// Report per-call mean/p50/p95; batched samples are means of `batch` calls.
fn bench_phrase_time<F: FnMut() -> bool>(
    iters: usize,
    batch: usize,
    mut f: F,
) -> (u128, u128, u128) {
    for _ in 0..64 {
        std::hint::black_box(f());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = std::time::Instant::now();
        for _ in 0..batch {
            std::hint::black_box(f());
        }
        samples.push(start.elapsed().as_nanos() / batch as u128);
    }
    samples.sort_unstable();
    let mean = samples.iter().sum::<u128>() / samples.len() as u128;
    (
        mean,
        samples[samples.len() / 2],
        samples[(samples.len() * 95) / 100],
    )
}

/// Report true per-call p50/p95, including the clock cost of each sample.
fn bench_phrase_percall<F: FnMut() -> bool>(iters: usize, mut f: F) -> (u128, u128) {
    for _ in 0..256 {
        std::hint::black_box(f());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = std::time::Instant::now();
        std::hint::black_box(f());
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    (
        samples[samples.len() / 2],
        samples[(samples.len() * 95) / 100],
    )
}

/// Create a two-clause phrase fixture.  The tail keeps the follower longer
/// than the anchor, making the anchor's skip behavior observable.
fn phrase_case(
    name: &str,
    n: u32,
    anchor: Vec<u32>,
    mut follower: Vec<u32>,
) -> (String, Vec<Vec<u32>>) {
    let far = 16 * n;
    follower.extend(far..far + n);
    (format!("{name}_n{n}"), vec![anchor, follower])
}

#[test]
#[allow(clippy::print_stdout)] // Benchmark output is the machine-readable result.
fn phrase_position_bench() {
    if std::env::var_os("LANCE_PHRASE_POSITION_BENCH").is_none() {
        return;
    }
    let scenario_filter = std::env::var("LANCE_PHRASE_POSITION_BENCH_SCENARIO").ok();
    let path_filter = std::env::var("LANCE_PHRASE_POSITION_BENCH_PATH").ok();

    println!(
        "# size_of Option<PositionCursor> = {}",
        std::mem::size_of::<Option<PositionCursor<'static>>>()
    );
    println!(
        "# size_of [Option<PositionCursor>;16] = {}",
        std::mem::size_of::<[Option<PositionCursor<'static>>; 16]>()
    );

    let mut scenarios: Vec<(String, Vec<Vec<u32>>)> = Vec::new();
    for &n in &[32_u32, 256, 2048, 16384] {
        let anchor: Vec<u32> = (0..n).collect();
        scenarios.push(phrase_case(
            "early_hit",
            n,
            anchor.clone(),
            (1..n + 1).collect(),
        ));
        scenarios.push(phrase_case(
            "disjoint",
            n,
            anchor.clone(),
            (2 * n..3 * n).collect(),
        ));
        let mut sparse = vec![n / 2];
        sparse.extend(2 * n..3 * n);
        scenarios.push(phrase_case("sparse_hit", n, anchor.clone(), sparse));
        let mut late: Vec<u32> = (n / 2 + 1..n + 1).collect();
        late.extend(2 * n..3 * n);
        scenarios.push(phrase_case("late_hit", n, anchor.clone(), late));
        scenarios.push(phrase_case(
            "strided_miss",
            n,
            (0..n).map(|position| 3 * position).collect(),
            (0..n).map(|position| 3 * position + 2).collect(),
        ));
    }

    for &n in &[32_u32, 2048] {
        let long: Vec<u32> = (0..n).collect();
        scenarios.push((
            format!("mc3_miss_mid_n{n}"),
            vec![vec![5, 900_000], vec![7, 900_002], long.clone()],
        ));
        scenarios.push((
            format!("mc3_miss_last_n{n}"),
            vec![vec![1000], vec![1001], (2000..2000 + n).collect()],
        ));
        let midpoint = n / 2;
        scenarios.push((
            format!("mc3_hit_late_n{n}"),
            vec![
                (0..n).collect(),
                (midpoint + 1..2 * n + 1).collect(),
                (midpoint + 2..2 * n + 2).collect(),
            ],
        ));
        // The first anchor occurrence lies below query offset 1.  This guards
        // the underflow/non-advancing-loop path.
        scenarios.push((
            format!("mc3_anchor_below_offset_n{n}"),
            vec![(0..n).collect(), vec![0, 500_000], (2..2 + n).collect()],
        ));
    }

    for &n in &[32_u32, 2048] {
        let long_a: Vec<u32> = (0..n).collect();
        let long_b: Vec<u32> = (0..n).collect();
        scenarios.push((
            format!("mc4_miss_first_n{n}"),
            vec![
                vec![5, 900_000],
                vec![7, 900_002],
                long_a.clone(),
                long_b.clone(),
            ],
        ));
        scenarios.push((
            format!("mc4_miss_last_n{n}"),
            vec![
                vec![1000],
                vec![1001],
                vec![1002],
                (2000..2000 + n).collect(),
            ],
        ));
        scenarios.push((
            format!("mc4_anchor_rare_n{n}"),
            vec![long_a, long_b, vec![5, 900_000], vec![8, 900_003]],
        ));
        let midpoint = n / 2;
        scenarios.push((
            format!("mc4_hit_late_n{n}"),
            vec![
                (0..n).collect(),
                (midpoint + 1..2 * n + 1).collect(),
                (midpoint + 2..2 * n + 2).collect(),
                (midpoint + 3..2 * n + 3).collect(),
            ],
        ));
    }
    scenarios.push((
        "mc4_hit_early".to_string(),
        vec![vec![1000], vec![1001], vec![1002], vec![1003]],
    ));

    println!("scenario,path,matched,mean_ns,p50_ns,p95_ns,iters,percall_p50_ns,percall_p95_ns");
    for (name, clauses) in scenarios {
        if scenario_filter
            .as_ref()
            .is_some_and(|filter| filter != &name)
        {
            continue;
        }
        let mut docs = DocSet::default();
        docs.append(0, 1_000_001);
        let postings = clauses
            .iter()
            .enumerate()
            .map(|(index, positions)| {
                PostingIterator::new(
                    format!("t{index}"),
                    index as u32,
                    index as u32,
                    generate_posting_list_with_positions(
                        vec![0],
                        vec![positions.clone()],
                        1.0,
                        true,
                    ),
                    docs.len(),
                )
            })
            .collect::<Vec<_>>();
        let wand = Wand::new(
            Operator::And,
            postings.into_iter(),
            &docs,
            IndexBM25Scorer::new(std::iter::empty()),
        );

        let matched = wand.check_exact_positions().unwrap();
        assert_eq!(wand.check_positions(0).unwrap(), matched, "{name}");
        assert_eq!(
            wand.check_exact_positions_bulk().unwrap(),
            matched,
            "{name}"
        );

        let batch = bench_phrase_batch(|| wand.check_exact_positions().unwrap());
        println!("# batch {name} = {batch}");
        if path_filter
            .as_ref()
            .is_none_or(|filter| filter == "classic")
        {
            bench_phrase_row(&name, "classic", matched, batch, || {
                wand.check_exact_positions().unwrap()
            });
        }
        if path_filter
            .as_ref()
            .is_none_or(|filter| filter == "compound_dispatch")
        {
            bench_phrase_row(&name, "compound_dispatch", matched, batch, || {
                wand.check_positions(0).unwrap()
            });
        }
        if path_filter.as_ref().is_none_or(|filter| filter == "bulk") {
            bench_phrase_row(&name, "bulk", matched, batch, || {
                wand.check_exact_positions_bulk().unwrap()
            });
        }
    }
}
