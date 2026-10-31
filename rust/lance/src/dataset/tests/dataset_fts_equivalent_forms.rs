// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Score-bit equalities between equivalent FTS query forms.
//!
//! Query normalization (E1 single-clause unwrap, E2 head flattening, E3 head
//! Match merging) relies on these equalities. Every test compares complete
//! (`limit = None`) results by row id and `f32::to_bits`, so a future change
//! to the scorers' summation order fails here first.

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow::array::AsArray;
use arrow::datatypes::{Float32Type, UInt64Type};
use arrow_array::{ArrayRef, RecordBatch, RecordBatchIterator, StringArray};
use lance_index::IndexType;
use lance_index::scalar::FullTextSearchQuery;
use lance_index::scalar::inverted::{
    SCORE_COL,
    query::{BooleanQuery, FtsQuery, MatchQuery, Occur, Operator},
    tokenizer::InvertedIndexParams,
};

use crate::Dataset;
use crate::dataset::ROW_ID;
use crate::dataset::write::WriteParams;
use crate::index::DatasetIndexExt;

const DOCS: [&str; 16] = [
    "alpha beta gamma delta",
    "alpha alpha beta",
    "beta gamma gamma gamma",
    "alpha gamma delta delta",
    "delta epsilon",
    "alpha beta gamma delta epsilon zeta",
    "gamma beta alpha",
    "zeta zeta alpha",
    "epsilon beta beta delta",
    "alpha delta zeta gamma beta",
    "beta",
    "gamma delta",
    "alpha beta beta gamma gamma gamma delta",
    "epsilon zeta",
    "alpha epsilon gamma",
    "delta delta delta beta alpha",
];

async fn write_dataset(docs: &[&str]) -> Dataset {
    let batch = RecordBatch::try_from_iter(vec![(
        "text",
        Arc::new(StringArray::from(docs.to_vec())) as ArrayRef,
    )])
    .unwrap();
    let schema = batch.schema();
    Dataset::write(
        RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema),
        "memory://",
        Some(WriteParams {
            max_rows_per_file: 6,
            ..Default::default()
        }),
    )
    .await
    .unwrap()
}

async fn indexed_dataset() -> Dataset {
    let mut dataset = write_dataset(&DOCS).await;
    dataset
        .create_index(
            &["text"],
            IndexType::Inverted,
            None,
            &InvertedIndexParams::default().with_position(true),
            true,
        )
        .await
        .unwrap();
    dataset
}

/// Indexed dataset plus one unindexed fragment, so compound planning falls
/// back to `BooleanQueryExec` for unlimited queries.
async fn partially_indexed_dataset() -> Dataset {
    let mut dataset = indexed_dataset().await;
    let batch =
        arrow_array::record_batch!(("text", Utf8, ["alpha beta gamma", "beta delta"])).unwrap();
    let schema = batch.schema();
    dataset
        .append(
            RecordBatchIterator::new(vec![batch].into_iter().map(Ok), schema),
            None,
        )
        .await
        .unwrap();
    dataset
}

fn m(terms: &str) -> FtsQuery {
    MatchQuery::new(terms.to_owned())
        .with_column(Some("text".to_owned()))
        .into()
}

fn m_op(terms: &str, operator: Operator) -> FtsQuery {
    MatchQuery::new(terms.to_owned())
        .with_column(Some("text".to_owned()))
        .with_operator(operator)
        .into()
}

fn boolean(must: Vec<FtsQuery>, should: Vec<FtsQuery>) -> FtsQuery {
    BooleanQuery::new(
        must.into_iter()
            .map(|query| (Occur::Must, query))
            .chain(should.into_iter().map(|query| (Occur::Should, query))),
    )
    .into()
}

async fn score_bits(dataset: &Dataset, query: FtsQuery) -> BTreeMap<u64, u32> {
    let mut scan = dataset.scan();
    scan.with_row_id()
        .full_text_search(FullTextSearchQuery::new_query(query))
        .unwrap();
    let batch = scan.try_into_batch().await.unwrap();
    let row_ids = batch[ROW_ID].as_primitive::<UInt64Type>().values();
    let scores = batch[SCORE_COL].as_primitive::<Float32Type>().values();
    row_ids
        .iter()
        .copied()
        .zip(scores.iter().map(|score| score.to_bits()))
        .collect()
}

async fn plan_text(dataset: &Dataset, query: FtsQuery) -> String {
    let mut scan = dataset.scan();
    scan.full_text_search(FullTextSearchQuery::new_query(query))
        .unwrap();
    scan.explain_plan(false).await.unwrap()
}

async fn assert_same_scores(dataset: &Dataset, left: FtsQuery, right: FtsQuery) {
    let left_scores = score_bits(dataset, left.clone()).await;
    let right_scores = score_bits(dataset, right.clone()).await;
    assert!(!left_scores.is_empty(), "fixture must match {left}");
    assert_eq!(
        left_scores, right_scores,
        "score bits differ between\n  {left}\n  {right}"
    );
}

#[tokio::test]
async fn test_per_term_must_matches_and_match_bit_for_bit() {
    let dataset = indexed_dataset().await;
    let per_term = boolean(vec![m("alpha"), m("beta"), m("gamma")], vec![]);
    let merged = m_op("alpha beta gamma", Operator::And);
    let wrapped = boolean(vec![m_op("alpha beta gamma", Operator::And)], vec![]);
    assert_same_scores(&dataset, per_term.clone(), merged).await;
    assert_same_scores(&dataset, per_term, wrapped).await;
}

#[tokio::test]
async fn test_per_term_should_matches_or_match_bit_for_bit() {
    let dataset = indexed_dataset().await;
    for terms in [vec!["alpha", "zeta"], vec!["alpha", "delta", "zeta"]] {
        let per_term = boolean(vec![], terms.iter().map(|term| m(term)).collect());
        let merged = m_op(&terms.join(" "), Operator::Or);
        let wrapped = boolean(vec![], vec![m_op(&terms.join(" "), Operator::Or)]);
        assert_same_scores(&dataset, per_term.clone(), merged).await;
        assert_same_scores(&dataset, per_term, wrapped).await;
    }
}

#[tokio::test]
async fn test_merged_must_and_should_buckets_bit_for_bit() {
    let dataset = indexed_dataset().await;
    // +a +b c
    assert_same_scores(
        &dataset,
        boolean(vec![m("alpha"), m("beta")], vec![m("delta")]),
        boolean(vec![m_op("alpha beta", Operator::And)], vec![m("delta")]),
    )
    .await;
    // +a +b c d, with both buckets merged
    assert_same_scores(
        &dataset,
        boolean(vec![m("alpha"), m("beta")], vec![m("delta"), m("zeta")]),
        boolean(
            vec![m_op("alpha beta", Operator::And)],
            vec![m_op("delta zeta", Operator::Or)],
        ),
    )
    .await;
}

#[tokio::test]
async fn test_head_flattening_bit_for_bit() {
    let dataset = indexed_dataset().await;
    let flat_must = boolean(vec![m("alpha"), m("beta"), m("gamma")], vec![m("zeta")]);
    let nested_must = boolean(
        vec![boolean(vec![m("alpha"), m("beta")], vec![]), m("gamma")],
        vec![m("zeta")],
    );
    assert_same_scores(&dataset, flat_must, nested_must).await;

    let flat_should = boolean(vec![], vec![m("alpha"), m("delta"), m("zeta")]);
    let nested_should = boolean(
        vec![],
        vec![boolean(vec![], vec![m("alpha"), m("delta")]), m("zeta")],
    );
    assert_same_scores(&dataset, flat_should, nested_should).await;

    let per_child = boolean(
        vec![
            boolean(vec![m("alpha")], vec![]),
            boolean(vec![m("beta")], vec![]),
        ],
        vec![boolean(vec![], vec![m("zeta")])],
    );
    let bare = boolean(vec![m("alpha"), m("beta")], vec![m("zeta")]);
    assert_same_scores(&dataset, per_child, bare).await;
}

#[tokio::test]
async fn test_root_unwrap_keeps_boosted_scores() {
    let dataset = indexed_dataset().await;
    let boosted: FtsQuery = MatchQuery::new("alpha delta".to_owned())
        .with_column(Some("text".to_owned()))
        .with_operator(Operator::And)
        .with_boost(2.5)
        .into();
    assert_same_scores(&dataset, boosted.clone(), boolean(vec![boosted], vec![])).await;
}

/// Must-only shapes are bit-for-bit equal in the fallback family because
/// `HashJoinExec` emits score columns in clause order. SHOULD buckets are not
/// covered: `BooleanQueryExec` sums them in batch arrival order today.
#[tokio::test]
async fn test_fallback_must_flattening_bit_for_bit() {
    let dataset = partially_indexed_dataset().await;
    let flat = boolean(vec![m("alpha"), m("beta"), m("gamma")], vec![]);
    let nested = boolean(
        vec![boolean(vec![m("alpha"), m("beta")], vec![]), m("gamma")],
        vec![],
    );
    assert!(
        plan_text(&dataset, flat.clone())
            .await
            .contains("BooleanQuery"),
        "the partial-index fixture must plan the fallback BooleanQueryExec"
    );
    assert_same_scores(&dataset, flat, nested).await;
}

/// Records whether non-head merging changes score bits on this fixture. It
/// documents why normalization only rewrites the head of a bucket; equal bits
/// here only mean the fixture does not hit f32 non-associativity.
#[tokio::test]
async fn test_non_head_merge_is_not_a_normalization_rule() {
    let dataset = indexed_dataset().await;
    let left = score_bits(
        &dataset,
        boolean(vec![m("epsilon"), m("alpha"), m("beta")], vec![]),
    )
    .await;
    let right = score_bits(
        &dataset,
        boolean(
            vec![m("epsilon"), m_op("alpha beta", Operator::And)],
            vec![],
        ),
    )
    .await;
    assert_eq!(
        left.keys().collect::<Vec<_>>(),
        right.keys().collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn test_compound_driver_metrics_are_reported() {
    let dataset = indexed_dataset().await;
    let mut scan = dataset.scan();
    scan.full_text_search(FullTextSearchQuery::new_query(boolean(
        vec![],
        vec![m("alpha"), m("delta"), m("zeta")],
    )))
    .unwrap();
    scan.limit(Some(5), None).unwrap();
    let analysis = scan.analyze_plan().await.unwrap();
    assert!(
        analysis.contains("fts_compound_leaves"),
        "compound driver metrics missing from:\n{analysis}"
    );
}
