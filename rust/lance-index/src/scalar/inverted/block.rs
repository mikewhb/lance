// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Shared block-execution types for the UBE (Unified Block Execution) design.
//!
//! These types are inert in C0. They describe the contract that the leaf
//! block source, the AND/OR window kernels, and the compound nodes will use.

#![allow(dead_code)]

use lance_core::Result;

use super::compound::ScoreBounds;

/// Capabilities of one leaf (or one aggregated node) for block execution.
///
/// Availability is authoritative through `ComposableScorer::as_block` /
/// `as_eval`; this struct only describes the leaf's properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Capacities {
    /// All postings are compressed.
    pub compressed: bool,
    /// At least one posting has grouped terms; block scoring is not valid.
    pub grouped: bool,
    /// The leaf is a phrase query; positions are required for membership.
    pub phrase: bool,
    /// Every leaf score is non-negative.
    pub scores_non_negative: bool,
    /// Posting block size for compressed leaves (0 when unknown).
    pub block_size: usize,
    /// The block max is a true window bound. 128-block lists have per-block
    /// maxima; 256-block lists use impacts when present.
    pub window_bounds: bool,
}

impl Capacities {
    pub(super) const NONE: Self = Self {
        compressed: false,
        grouped: false,
        phrase: false,
        scores_non_negative: false,
        block_size: 0,
        window_bounds: false,
    };

    /// Whether a leaf with these capabilities can serve block execution.
    ///
    /// Compressed short-block postings are the only source of a true window
    /// bound. Grouped expansions score their terms one by one and a phrase
    /// leaf decides membership from positions, so neither can answer a
    /// document from posting blocks alone.
    pub(super) fn supports_block(&self) -> bool {
        self.compressed && !self.grouped && !self.phrase && self.window_bounds
    }
}

/// Inclusive document range used by block kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    pub start: u64,
    pub up_to: u64,
}

/// One produced candidate block.
#[derive(Debug)]
pub(super) struct BlockBuf {
    pub start: u64,
    pub up_to: u64,
    pub upper: ScoreBounds,
    pub docs: Vec<u64>,
    pub scores: Vec<f32>,
    pub keys: Vec<u64>,
    pub matched: Vec<bool>,
    /// Document-major frequencies: `docs.len() * clause_count` entries.
    pub freqs: Vec<u32>,
    pub clause_count: usize,
}

impl Default for BlockBuf {
    fn default() -> Self {
        Self {
            start: 0,
            up_to: 0,
            upper: ScoreBounds::ZERO,
            docs: Vec::new(),
            scores: Vec::new(),
            keys: Vec::new(),
            matched: Vec::new(),
            freqs: Vec::new(),
            clause_count: 0,
        }
    }
}

impl BlockBuf {
    pub(super) fn clear(&mut self) {
        self.start = 0;
        self.up_to = 0;
        self.upper = ScoreBounds::ZERO;
        self.docs.clear();
        self.scores.clear();
        self.keys.clear();
        self.matched.clear();
        self.freqs.clear();
        self.clause_count = 0;
    }
}

/// Result of one `next_block` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockStatus {
    Data,
    Empty { up_to: u64 },
    Exhausted,
}

/// Evaluation result for a supplied candidate batch.
///
/// `scores.len() == matched.len() == docs.len()`. `exhausted` means the
/// subtree cannot produce any further candidates; it is not "this batch is
/// empty".
#[derive(Debug, Default)]
pub(super) struct EvalOut {
    pub scores: Vec<f32>,
    pub matched: Vec<bool>,
    pub exhausted: bool,
}

impl EvalOut {
    pub(super) fn clear(&mut self, len: usize) {
        self.scores.clear();
        self.scores.resize(len, 0.0);
        self.matched.clear();
        self.matched.resize(len, false);
        self.exhausted = false;
    }
}

/// A node that can score a supplied ascending candidate batch.
///
/// `scores[i]` is this subtree's total for `docs[i]`; it is written, not
/// accumulated into a caller-provided running sum. The parent adds each
/// child total once, in query order.
pub(super) trait BatchEvaluator {
    fn eval_at(&mut self, docs: &[u64], out: &mut EvalOut) -> Result<()>;
}

/// A node that can produce candidate blocks.
///
/// Implementations are explicit; there is no single-document default. A node
/// that cannot produce blocks must return `None` from
/// `ComposableScorer::as_block`.
pub(super) trait BlockScorer: BatchEvaluator {
    fn caps(&self) -> Capacities;

    /// Metadata-only positioning. Returns the first document at or after
    /// `target`, or `None` when no such document exists. `None` is not a
    /// terminal state: the node may still be reusable for a different target.
    /// Must not score or check visibility.
    fn seek(&mut self, target: u64) -> Result<Option<u64>>;

    /// Produce the next block at or after `target`.
    fn next_block(&mut self, target: u64, budget: usize, out: &mut BlockBuf)
    -> Result<BlockStatus>;

    /// Inclusive end of the block containing `target`. Metadata-only.
    fn block_end_at(&mut self, target: u64) -> Result<u64>;

    /// Conservative bound for the supplied window. Must be position-preserving.
    fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds>;
}

#[allow(dead_code)]
fn assert_block_scorer_object_safe(_: &dyn BlockScorer) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_capabilities_are_inert() {
        let caps = Capacities::NONE;
        assert!(!caps.window_bounds);
        assert!(!caps.compressed);
    }

    #[test]
    fn block_buf_default_is_empty() {
        let mut buf = BlockBuf::default();
        assert!(buf.docs.is_empty());
        assert_eq!(buf.upper, ScoreBounds::ZERO);
        buf.docs.push(1);
        buf.freqs.push(1);
        buf.clause_count = 1;
        buf.clear();
        assert!(buf.docs.is_empty());
        assert!(buf.freqs.is_empty());
        assert_eq!(buf.clause_count, 0);
    }

    #[test]
    fn eval_out_clear_matches_len() {
        let mut out = EvalOut::default();
        out.clear(3);
        assert_eq!(out.scores.len(), 3);
        assert_eq!(out.matched.len(), 3);
        assert!(!out.exhausted);
    }
}
