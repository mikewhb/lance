// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Canonical rewriting of FTS query trees before planning.
//!
//! Equivalent query forms such as `Match("a b", And)` and
//! `Boolean{must:[Match(a), Match(b)]}` match the same documents with the
//! same scores but reach different plans. [`normalize_fts_query`] rewrites a
//! validated tree into one canonical form so equivalent forms share a plan.
//!
//! Every rule keeps each document's score bit for bit. Lance folds the
//! clauses of a bucket from `0.0` in clause order and f32 addition is not
//! associative, so rules only ever rewrite the head of a bucket:
//!
//! * E1 drops a single-clause wrapper: `Bool{must:[X]}` / `Bool{should:[X]}`
//!   becomes `X` when the other buckets are empty.
//! * E2 splices a head child that is a Boolean with only same-occur clauses
//!   into its parent bucket. `must_not` buckets are never flattened.
//! * E3 merges the leading run of exact single-term `Match` clauses of a
//!   `must` (`And`) or `should` (`Or`) bucket into one `Match`. It needs the
//!   column's index analyzer, so it only runs for columns present in the
//!   analyzer map, and never inside `Boost` subtrees.
//!
//! A root that would collapse into a `Phrase`, a multi-column `MultiMatch`
//! or a `CombinedFields` node keeps one `Bool{must:[X]}` wrapper, because
//! those roots would leave the compound scorer for plans that have not been
//! measured.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::DocumentGranularity;
use super::query::{
    BooleanQuery, BoostQuery, FtsQuery, FtsQueryNode, MatchQuery, Operator, Tokens,
    try_collect_query_tokens,
};
use super::tokenizer::document_tokenizer::LanceTokenizer;

/// Index analyzer of each row-granularity text column, keyed by column name.
pub type FtsQueryAnalyzers = HashMap<String, Box<dyn LanceTokenizer>>;

/// Rewrites a validated query into its canonical form using E1 and E2 only.
///
/// Callers must only pass trees accepted by the planner's query contract
/// (no empty Boolean, no Boolean with only `must_not`); the rewrite neither
/// reports nor repairs invalid trees.
pub fn normalize_fts_query(query: FtsQuery) -> FtsQuery {
    normalize_fts_query_with_analyzers(query, &mut FtsQueryAnalyzers::new())
}

/// Rewrites a validated query into its canonical form (E1, E2, E3).
///
/// `analyzers` must hold, for each column, the tokenizer its FTS index uses
/// to analyze query text. Columns without an analyzer are never merged.
pub fn normalize_fts_query_with_analyzers(
    query: FtsQuery,
    analyzers: &mut FtsQueryAnalyzers,
) -> FtsQuery {
    if !contains_boolean(&query) {
        return query;
    }
    let was_boolean = matches!(query, FtsQuery::Boolean(_));
    let normalized = normalize_node(query, analyzers, false);
    if was_boolean && is_restricted_root(&normalized) {
        return FtsQuery::Boolean(BooleanQuery {
            should: Vec::new(),
            must: vec![normalized],
            must_not: Vec::new(),
        });
    }
    normalized
}

#[derive(Clone, Copy)]
pub(crate) enum Bucket {
    Must,
    Should,
}

pub(crate) fn contains_boolean(query: &FtsQuery) -> bool {
    match query {
        FtsQuery::Boolean(_) => true,
        FtsQuery::Boost(query) => {
            contains_boolean(&query.positive) || contains_boolean(&query.negative)
        }
        FtsQuery::Match(_)
        | FtsQuery::Phrase(_)
        | FtsQuery::MultiMatch(_)
        | FtsQuery::CombinedFields(_) => false,
    }
}

/// Roots that must stay wrapped so the query keeps its compound plan.
pub(crate) fn is_restricted_root(query: &FtsQuery) -> bool {
    match query {
        FtsQuery::Phrase(_) | FtsQuery::CombinedFields(_) => true,
        FtsQuery::MultiMatch(_) => query.columns().len() > 1,
        FtsQuery::Match(_) | FtsQuery::Boolean(_) | FtsQuery::Boost(_) => false,
    }
}

fn normalize_node(node: FtsQuery, analyzers: &mut FtsQueryAnalyzers, in_boost: bool) -> FtsQuery {
    match node {
        FtsQuery::Boolean(query) => {
            // Children first: each child comes back already canonical.
            let mut must = normalize_all(query.must, analyzers, in_boost);
            let mut should = normalize_all(query.should, analyzers, in_boost);
            let must_not = normalize_all(query.must_not, analyzers, in_boost);

            // E2. One splice is enough: the spliced child is canonical, so its
            // own head is no longer a pure same-occur Boolean.
            splice_head(&mut must, Bucket::Must);
            splice_head(&mut should, Bucket::Should);

            // E3. SHOULD is merged whether or not MUST is empty; the design's
            // D8 explains why a conditional merge would break convergence.
            if !in_boost {
                merge_head_run(&mut must, Operator::And, analyzers);
                merge_head_run(&mut should, Operator::Or, analyzers);
            }

            unwrap_single_clause(must, should, must_not)
        }
        FtsQuery::Boost(query) => FtsQuery::Boost(BoostQuery {
            positive: Box::new(normalize_node(*query.positive, analyzers, true)),
            negative: Box::new(normalize_node(*query.negative, analyzers, true)),
            negative_boost: query.negative_boost,
        }),
        leaf => leaf,
    }
}

fn normalize_all(
    children: Vec<FtsQuery>,
    analyzers: &mut FtsQueryAnalyzers,
    in_boost: bool,
) -> Vec<FtsQuery> {
    children
        .into_iter()
        .map(|child| normalize_node(child, analyzers, in_boost))
        .collect()
}

/// Columns whose analyzer E3 may need, in name order.
///
/// Purely structural and deliberately over-approximate: a column is listed
/// when at least two field-mergeable `Match` nodes on it appear outside
/// `Boost` subtrees. Listing a column only costs one analyzer lookup.
pub fn merge_candidate_columns(query: &FtsQuery) -> Vec<String> {
    fn visit(query: &FtsQuery, counts: &mut BTreeMap<String, usize>) {
        match query {
            FtsQuery::Match(query) => {
                if is_mergeable_field(query)
                    && let Some(column) = &query.column
                {
                    *counts.entry(column.clone()).or_default() += 1;
                }
            }
            FtsQuery::Boolean(query) => {
                for child in query.must.iter().chain(&query.should) {
                    visit(child, counts);
                }
            }
            FtsQuery::Boost(_)
            | FtsQuery::Phrase(_)
            | FtsQuery::MultiMatch(_)
            | FtsQuery::CombinedFields(_) => {}
        }
    }
    let mut counts = BTreeMap::new();
    visit(query, &mut counts);
    counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(column, _)| column)
        .collect()
}

/// Field-level merge conditions C1-C3: a bound column, row documents, boost
/// exactly 1 and exact matching.
fn is_mergeable_field(query: &MatchQuery) -> bool {
    query.column.is_some()
        && query.document_granularity == Some(DocumentGranularity::Row)
        && query.boost.to_bits() == 1.0_f32.to_bits()
        && query.fuzziness == Some(0)
}

/// C1-C4 between the head of a run and a later clause.
fn is_compatible(head: &MatchQuery, other: &MatchQuery) -> bool {
    is_mergeable_field(other)
        && other.column == head.column
        && other.document_granularity == head.document_granularity
        && other.max_expansions == head.max_expansions
        && other.prefix_length == head.prefix_length
}

fn analyze(tokenizer: &mut Box<dyn LanceTokenizer>, text: &str) -> Option<Tokens> {
    // A query syntax error leaves the clause unmerged; execution reports it.
    try_collect_query_tokens(text, tokenizer).ok()
}

fn token_texts(tokens: &Tokens) -> Vec<String> {
    (0..tokens.len())
        .map(|index| tokens.get_token(index).to_owned())
        .collect()
}

/// E3: merge the longest valid leading run of exact `Match` clauses.
///
/// The head may already hold several terms when its operator equals the
/// bucket's; every later clause must analyze to exactly one term (C6). The
/// run's terms must be distinct (C7), and re-analyzing the joined text must
/// give the concatenated terms with strictly increasing positions (C8), so the
/// merged leaf folds the same scores in the same order.
fn merge_head_run(
    clauses: &mut Vec<FtsQuery>,
    operator: Operator,
    analyzers: &mut FtsQueryAnalyzers,
) {
    let Some(FtsQuery::Match(head)) = clauses.first() else {
        return;
    };
    if !is_mergeable_field(head) {
        return;
    }
    let Some(tokenizer) = head
        .column
        .as_deref()
        .and_then(|column| analyzers.get_mut(column))
    else {
        return;
    };
    let Some(head_tokens) = analyze(tokenizer, &head.terms) else {
        return;
    };
    let head_ok = head_tokens.len() == 1 || (head_tokens.len() >= 2 && head.operator == operator);
    if !head_ok {
        return;
    }

    let mut run_terms = vec![token_texts(&head_tokens)];
    for clause in &clauses[1..] {
        let FtsQuery::Match(next) = clause else {
            break;
        };
        if !is_compatible(head, next) {
            break;
        }
        let Some(tokens) = analyze(tokenizer, &next.terms) else {
            break;
        };
        if tokens.len() != 1 {
            break;
        }
        run_terms.push(token_texts(&tokens));
    }

    // C7 and C8 are not monotone in the run length, so try the longest first.
    let mut chosen = None;
    for len in (2..=run_terms.len()).rev() {
        let expected = run_terms[..len].iter().flatten().collect::<Vec<_>>();
        let mut seen = HashSet::with_capacity(expected.len());
        if !expected.iter().all(|term| seen.insert(term.as_str())) {
            continue;
        }
        let joined = clauses[..len]
            .iter()
            .map(|clause| match clause {
                FtsQuery::Match(query) => query.terms.as_str(),
                _ => unreachable!("the run holds only Match clauses"),
            })
            .collect::<Vec<_>>()
            .join(" ");
        let Some(merged) = analyze(tokenizer, &joined) else {
            continue;
        };
        let same_terms = merged.len() == expected.len()
            && (0..merged.len()).all(|index| merged.get_token(index) == expected[index].as_str());
        let increasing =
            (1..merged.len()).all(|index| merged.position(index - 1) < merged.position(index));
        if same_terms && increasing {
            chosen = Some((len, joined));
            break;
        }
    }
    let Some((len, terms)) = chosen else {
        return;
    };
    let merged = MatchQuery {
        terms,
        operator,
        boost: 1.0,
        fuzziness: Some(0),
        ..head.clone()
    };
    clauses.splice(0..len, [FtsQuery::Match(merged)]);
}

/// E1: a Boolean with one scoring clause and no `must_not` scores `0.0 + x`.
pub(crate) fn unwrap_single_clause(
    mut must: Vec<FtsQuery>,
    mut should: Vec<FtsQuery>,
    must_not: Vec<FtsQuery>,
) -> FtsQuery {
    if must_not.is_empty() {
        if must.len() == 1 && should.is_empty() {
            return must.pop().expect("checked length");
        }
        if must.is_empty() && should.len() == 1 {
            return should.pop().expect("checked length");
        }
    }
    FtsQuery::Boolean(BooleanQuery {
        should,
        must,
        must_not,
    })
}

/// True when `query` holds only clauses of `bucket`'s occur, at least one.
fn is_pure(query: &BooleanQuery, bucket: Bucket) -> bool {
    if !query.must_not.is_empty() {
        return false;
    }
    match bucket {
        Bucket::Must => query.should.is_empty() && !query.must.is_empty(),
        Bucket::Should => query.must.is_empty() && !query.should.is_empty(),
    }
}

/// E2: splice a pure same-occur Boolean at the head of `clauses`.
pub(crate) fn splice_head(clauses: &mut Vec<FtsQuery>, bucket: Bucket) {
    let splice =
        matches!(clauses.first(), Some(FtsQuery::Boolean(inner)) if is_pure(inner, bucket));
    if !splice {
        return;
    }
    let FtsQuery::Boolean(inner) = clauses.remove(0) else {
        unreachable!("checked that the head is a Boolean");
    };
    let inner_clauses = match bucket {
        Bucket::Must => inner.must,
        Bucket::Should => inner.should,
    };
    clauses.splice(0..0, inner_clauses);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scalar::inverted::query::{MultiMatchQuery, Occur, PhraseQuery};
    use crate::scalar::inverted::tokenizer::InvertedIndexParams;

    fn exact(terms: &str) -> MatchQuery {
        MatchQuery::new(terms.to_owned())
            .with_column(Some("text".to_owned()))
            .with_document_granularity(DocumentGranularity::Row)
    }

    fn em(terms: &str) -> FtsQuery {
        exact(terms).into()
    }

    fn em_op(terms: &str, operator: Operator) -> FtsQuery {
        exact(terms).with_operator(operator).into()
    }

    fn analyzers() -> FtsQueryAnalyzers {
        let mut analyzers = FtsQueryAnalyzers::new();
        analyzers.insert(
            "text".to_owned(),
            InvertedIndexParams::default().build().unwrap(),
        );
        analyzers
    }

    fn e3(query: FtsQuery) -> FtsQuery {
        normalize_fts_query_with_analyzers(query, &mut analyzers())
    }

    #[test]
    fn merges_per_term_buckets_into_one_match() {
        assert_eq!(
            e3(must(vec![em("alpha"), em("beta"), em("gamma")])),
            em_op("alpha beta gamma", Operator::And)
        );
        assert_eq!(
            e3(should(vec![em("alpha"), em("beta")])),
            em_op("alpha beta", Operator::Or)
        );
        // +a +b c d: both buckets merge, the node stays a Boolean.
        assert_eq!(
            e3(mixed(
                vec![em("alpha"), em("beta")],
                vec![em("gamma"), em("delta")],
                vec![]
            )),
            mixed(
                vec![em_op("alpha beta", Operator::And)],
                vec![em_op("gamma delta", Operator::Or)],
                vec![]
            )
        );
    }

    #[test]
    fn merges_through_nested_heads() {
        // Inner bucket merges first; the multi-term head keeps merging outside.
        assert_eq!(
            e3(must(vec![must(vec![em("alpha"), em("beta")]), em("gamma")])),
            em_op("alpha beta gamma", Operator::And)
        );
        // Converges with the conditional-merge counterexample from review.
        assert_eq!(
            e3(mixed(
                vec![em("alpha")],
                vec![should(vec![em("beta"), em("gamma")])],
                vec![]
            )),
            e3(mixed(
                vec![em("alpha")],
                vec![em("beta"), em("gamma")],
                vec![]
            ))
        );
    }

    #[test]
    fn refuses_unsafe_merges() {
        // Non-head runs, fuzzy, boosted, repeated and stop-word-free checks.
        let phrase: FtsQuery = PhraseQuery::new("alpha beta".to_owned())
            .with_column(Some("text".to_owned()))
            .into();
        let head_phrase = must(vec![phrase, em("alpha"), em("beta")]);
        assert_eq!(e3(head_phrase.clone()), head_phrase);

        let fuzzy: FtsQuery = exact("alpha").with_fuzziness(Some(1)).into();
        let with_fuzzy = must(vec![fuzzy, em("beta")]);
        assert_eq!(e3(with_fuzzy.clone()), with_fuzzy);

        let boosted: FtsQuery = exact("alpha").with_boost(2.0).into();
        let with_boost = must(vec![boosted, em("beta")]);
        assert_eq!(e3(with_boost.clone()), with_boost);

        let repeated = must(vec![em("alpha"), em("alpha")]);
        assert_eq!(e3(repeated.clone()), repeated);

        // A multi-term head with the other operator cannot absorb more terms.
        let or_head = must(vec![em_op("alpha beta", Operator::Or), em("gamma")]);
        assert_eq!(e3(or_head.clone()), or_head);

        // Without an analyzer for the column, E3 does nothing.
        let unknown = must(vec![em("alpha"), em("beta")]);
        assert_eq!(normalize_fts_query(unknown.clone()), unknown);
    }

    #[test]
    fn does_not_merge_inside_boost() {
        let query: FtsQuery =
            BoostQuery::new(must(vec![em("alpha"), em("beta")]), em("gamma"), Some(0.5)).into();
        assert_eq!(e3(query.clone()), query);
    }

    #[test]
    fn lists_candidate_columns() {
        assert_eq!(
            merge_candidate_columns(&must(vec![em("alpha"), em("beta")])),
            vec!["text".to_owned()]
        );
        assert!(merge_candidate_columns(&must(vec![em("alpha")])).is_empty());
        let boosted: FtsQuery =
            BoostQuery::new(must(vec![em("alpha"), em("beta")]), em("gamma"), Some(0.5)).into();
        assert!(merge_candidate_columns(&boosted).is_empty());
    }

    fn m(terms: &str) -> FtsQuery {
        MatchQuery::new(terms.to_owned())
            .with_column(Some("text".to_owned()))
            .into()
    }

    fn must(children: Vec<FtsQuery>) -> FtsQuery {
        BooleanQuery::new(children.into_iter().map(|child| (Occur::Must, child))).into()
    }

    fn should(children: Vec<FtsQuery>) -> FtsQuery {
        BooleanQuery::new(children.into_iter().map(|child| (Occur::Should, child))).into()
    }

    fn mixed(must: Vec<FtsQuery>, should: Vec<FtsQuery>, must_not: Vec<FtsQuery>) -> FtsQuery {
        FtsQuery::Boolean(BooleanQuery {
            should,
            must,
            must_not,
        })
    }

    #[test]
    fn unwraps_single_clause_wrappers() {
        assert_eq!(normalize_fts_query(must(vec![m("a b")])), m("a b"));
        assert_eq!(normalize_fts_query(should(vec![m("a b")])), m("a b"));
        assert_eq!(
            normalize_fts_query(must(vec![must(vec![must(vec![m("a")])])])),
            m("a")
        );
        assert_eq!(
            normalize_fts_query(must(vec![must(vec![m("a")]), should(vec![m("b")])])),
            must(vec![m("a"), m("b")])
        );
    }

    #[test]
    fn keeps_wrapper_with_must_not() {
        let query = mixed(vec![m("a")], vec![], vec![m("x")]);
        assert_eq!(normalize_fts_query(query.clone()), query);
        let nested = mixed(vec![m("a")], vec![], vec![should(vec![m("x")])]);
        assert_eq!(normalize_fts_query(nested), query);
    }

    #[test]
    fn flattens_only_the_head_of_a_bucket() {
        assert_eq!(
            normalize_fts_query(must(vec![must(vec![m("a"), m("b")]), m("c")])),
            must(vec![m("a"), m("b"), m("c")])
        );
        assert_eq!(
            normalize_fts_query(should(vec![should(vec![m("a"), m("b")]), m("c")])),
            should(vec![m("a"), m("b"), m("c")])
        );
        // a + (b + c) is a different bracket class.
        let non_head = must(vec![m("a"), must(vec![m("b"), m("c")])]);
        assert_eq!(normalize_fts_query(non_head.clone()), non_head);
        // A should group inside must is not same-occur.
        let mixed_occur = must(vec![should(vec![m("a"), m("b")]), m("c")]);
        assert_eq!(normalize_fts_query(mixed_occur.clone()), mixed_occur);
    }

    #[test]
    fn never_flattens_must_not_or_impure_children() {
        let impure = must(vec![mixed(vec![m("a")], vec![], vec![m("x")]), m("c")]);
        assert_eq!(normalize_fts_query(impure.clone()), impure);
    }

    #[test]
    fn keeps_restricted_roots_wrapped_in_must() {
        let phrase: FtsQuery = PhraseQuery::new("a b".to_owned())
            .with_column(Some("text".to_owned()))
            .into();
        assert_eq!(
            normalize_fts_query(should(vec![phrase.clone()])),
            must(vec![phrase.clone()])
        );
        assert_eq!(
            normalize_fts_query(must(vec![should(vec![phrase.clone()])])),
            must(vec![phrase.clone()])
        );
        assert_eq!(normalize_fts_query(phrase.clone()), phrase);

        let multi_column: FtsQuery = MultiMatchQuery {
            match_queries: vec![
                MatchQuery::new("a".to_owned()).with_column(Some("title".to_owned())),
                MatchQuery::new("a".to_owned()).with_column(Some("body".to_owned())),
            ],
        }
        .into();
        assert_eq!(
            normalize_fts_query(must(vec![multi_column.clone()])),
            must(vec![multi_column])
        );
    }

    #[test]
    fn recurses_into_boost() {
        let query: FtsQuery = BoostQuery::new(
            must(vec![must(vec![m("a")])]),
            should(vec![m("b")]),
            Some(0.5),
        )
        .into();
        let expected: FtsQuery = BoostQuery::new(m("a"), m("b"), Some(0.5)).into();
        assert_eq!(normalize_fts_query(query), expected);
    }

    #[test]
    fn is_idempotent() {
        let queries = [
            must(vec![must(vec![m("a"), m("b")]), should(vec![m("c")])]),
            mixed(
                vec![must(vec![m("a")]), m("b")],
                vec![should(vec![m("c"), m("d")])],
                vec![m("x")],
            ),
            should(vec![should(vec![should(vec![m("a")]), m("b")]), m("c")]),
        ];
        for query in queries {
            let once = normalize_fts_query(query);
            assert_eq!(normalize_fts_query(once.clone()), once);
        }
    }
}
