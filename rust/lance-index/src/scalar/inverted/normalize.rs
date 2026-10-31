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
//!
//! A root that would collapse into a `Phrase`, a multi-column `MultiMatch`
//! or a `CombinedFields` node keeps one `Bool{must:[X]}` wrapper, because
//! those roots would leave the compound scorer for plans that have not been
//! measured.

use super::query::{BooleanQuery, BoostQuery, FtsQuery, FtsQueryNode};

/// Rewrites a validated query into its canonical form.
///
/// Callers must only pass trees accepted by the planner's query contract
/// (no empty Boolean, no Boolean with only `must_not`); the rewrite neither
/// reports nor repairs invalid trees.
pub fn normalize_fts_query(query: FtsQuery) -> FtsQuery {
    if !contains_boolean(&query) {
        return query;
    }
    let was_boolean = matches!(query, FtsQuery::Boolean(_));
    let normalized = normalize_node(query);
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

fn normalize_node(node: FtsQuery) -> FtsQuery {
    match node {
        FtsQuery::Boolean(query) => {
            // Children first: each child comes back already canonical.
            let mut must = normalize_all(query.must);
            let mut should = normalize_all(query.should);
            let must_not = normalize_all(query.must_not);

            // E2. One splice is enough: the spliced child is canonical, so its
            // own head is no longer a pure same-occur Boolean.
            splice_head(&mut must, Bucket::Must);
            splice_head(&mut should, Bucket::Should);

            unwrap_single_clause(must, should, must_not)
        }
        FtsQuery::Boost(query) => FtsQuery::Boost(BoostQuery {
            positive: Box::new(normalize_node(*query.positive)),
            negative: Box::new(normalize_node(*query.negative)),
            negative_boost: query.negative_boost,
        }),
        leaf => leaf,
    }
}

fn normalize_all(children: Vec<FtsQuery>) -> Vec<FtsQuery> {
    children.into_iter().map(normalize_node).collect()
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
    use crate::scalar::inverted::query::{MatchQuery, MultiMatchQuery, Occur, PhraseQuery};

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
