// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Shared block-execution types for the UBE (Unified Block Execution) design.
//!
//! These types are inert in C0. They describe the contract that the leaf
//! block source, the AND/OR window kernels, and the compound nodes will use.

use lance_core::Result;

use super::compound::ScoreBounds;
use super::wand::PostingIterator;

/// Capabilities of one leaf (or one aggregated node) for block execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Capacities {
    /// The node can produce candidate blocks.
    pub block_producer: bool,
    /// The node can evaluate an externally supplied candidate batch.
    pub batch_eval: bool,
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
        block_producer: false,
        batch_eval: false,
        compressed: false,
        grouped: false,
        phrase: false,
        scores_non_negative: false,
        block_size: 0,
        window_bounds: false,
    };
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
#[derive(Debug)]
pub(super) struct EvalOut {
    pub scores: Vec<f32>,
    pub matched: Vec<bool>,
    pub exhausted: bool,
}

impl Default for EvalOut {
    fn default() -> Self {
        Self {
            scores: Vec::new(),
            matched: Vec::new(),
            exhausted: false,
        }
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
    /// `target`, or `None` when exhausted. Must not score or check visibility.
    fn seek(&mut self, target: u64) -> Result<Option<u64>>;

    /// Produce the next block at or after `target`.
    fn next_block(&mut self, target: u64, budget: usize, out: &mut BlockBuf)
    -> Result<BlockStatus>;

    /// Inclusive end of the block containing `target`. Metadata-only.
    fn block_end_at(&mut self, target: u64) -> Result<u64>;

    /// Conservative bound for the supplied window. Must be position-preserving.
    fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds>;
}

/// Flat posting view used by block kernels.
///
/// The classic `Wand` keeps postings in heap/lead/tail form. Block execution
/// uses a stable flat list plus explicit score/cost orders.
pub(super) struct BlockPostingSet {
    pub postings: Vec<Box<PostingIterator>>,
    pub score_order: Vec<usize>,
    pub cost_order: Vec<usize>,
    pub weights: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_capabilities_are_inert() {
        let caps = Capacities::NONE;
        assert!(!caps.block_producer);
        assert!(!caps.batch_eval);
        assert!(!caps.window_bounds);
    }

    #[test]
    fn block_buf_default_is_empty() {
        let mut buf = BlockBuf::default();
        assert!(buf.docs.is_empty());
        assert_eq!(buf.upper, ScoreBounds::ZERO);
        buf.docs.push(1);
        buf.clear();
        assert!(buf.docs.is_empty());
    }
}
