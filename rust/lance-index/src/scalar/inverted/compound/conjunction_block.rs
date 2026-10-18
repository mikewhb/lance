// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Block-driven MUST conjunction. The producer owns the child scorers; the
//! state owns the per-document view. This wrapper leaves
//! `RequiredConjunctionScorer` on its classic path when the block path is
//! disabled.

use lance_core::{Error, Result};

use super::super::block::{
    BatchEvaluator, BlockBuf, BlockScorer, BlockStatus, Capacities, EvalOut, Window,
};
use super::{BoxScorer, ComposableScorer, ScoreBounds};

#[derive(Default)]
struct BlockState {
    block: BlockBuf,
    cursor: usize,
    /// Index in `block.docs` of `current`; the position lookup below is O(1)
    /// on the per-document face instead of a scan of the whole block.
    current_index: Option<usize>,
    current: Option<u64>,
    exhausted: bool,
}

impl BlockState {
    fn refill(&mut self, producer: &mut dyn BlockScorer, target: u64) -> Result<bool> {
        match producer.next_block(target, 4096, &mut self.block)? {
            BlockStatus::Data => {
                self.cursor = 0;
                self.exhausted = false;
                Ok(!self.block.docs.is_empty())
            }
            BlockStatus::Empty { up_to } => {
                self.exhausted = up_to == u64::MAX;
                self.block.up_to = up_to;
                Ok(false)
            }
            BlockStatus::Exhausted => {
                self.exhausted = true;
                Ok(false)
            }
        }
    }

    fn next(&mut self, producer: &mut dyn BlockScorer) -> Result<Option<u64>> {
        loop {
            if self.cursor < self.block.docs.len() {
                let index = self.cursor;
                self.take(index);
                return Ok(self.current);
            }
            if self.exhausted {
                return Ok(None);
            }
            let target = if self.block.up_to == 0 {
                0
            } else {
                self.block.up_to.saturating_add(1)
            };
            if !self.refill(producer, target)? && self.exhausted {
                return Ok(None);
            }
        }
    }

    fn advance(&mut self, producer: &mut dyn BlockScorer, target: u64) -> Result<Option<u64>> {
        if self.exhausted {
            return Ok(None);
        }
        if let Some(current) = self.current
            && current >= target
        {
            return Ok(Some(current));
        }
        while self.cursor < self.block.docs.len() && self.block.docs[self.cursor] < target {
            self.cursor += 1;
        }
        if self.cursor < self.block.docs.len() {
            let index = self.cursor;
            self.take(index);
            return Ok(self.current);
        }
        if self.block.up_to == u64::MAX {
            self.exhausted = true;
            return Ok(None);
        }
        // `up_to == 0` means no block has been produced yet, so `target` is
        // still the first document this state may return.
        let next = if self.block.up_to == 0 {
            target
        } else {
            target.max(self.block.up_to.saturating_add(1))
        };
        if !self.refill(producer, next)? && self.exhausted {
            return Ok(None);
        }
        self.next(producer)
    }

    fn take(&mut self, index: usize) {
        self.cursor = index + 1;
        self.current_index = Some(index);
        self.current = Some(self.block.docs[index]);
    }

    fn positioned_index(&self) -> Option<usize> {
        let index = self.current_index?;
        (self.block.docs.get(index) == Some(&self.current?)).then_some(index)
    }

    fn current_score(&self) -> Option<f32> {
        self.positioned_index()
            .and_then(|index| self.block.scores.get(index).copied())
    }

    fn current_key(&self) -> Option<u64> {
        self.positioned_index()
            .and_then(|index| self.block.keys.get(index).copied())
    }
}

struct ConjunctionProducer<'a> {
    children: Vec<BoxScorer<'a>>,
    cost_order: Vec<usize>,
    score_order: Vec<usize>,
    lead_buf: BlockBuf,
}

impl<'a> ConjunctionProducer<'a> {
    fn new(children: Vec<BoxScorer<'a>>) -> Result<Self> {
        if children.is_empty() {
            return Err(Error::internal("block conjunction requires a child"));
        }
        let mut cost_order = (0..children.len()).collect::<Vec<_>>();
        cost_order.sort_unstable_by_key(|&index| (children[index].cost(), index));
        Ok(Self {
            children,
            score_order: (0..cost_order.len()).collect(),
            cost_order,
            lead_buf: BlockBuf::default(),
        })
    }

    fn cost(&self) -> usize {
        self.children
            .iter()
            .map(|child| child.cost())
            .min()
            .unwrap_or(0)
    }

    fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        let mut d = target;
        loop {
            let mut moved = false;
            for &index in &self.cost_order {
                let block = self.children[index]
                    .as_block()
                    .ok_or_else(|| Error::internal("conjunction child is not a block producer"))?;
                match block.seek(d)? {
                    None => return Ok(None),
                    Some(doc) if doc > d => {
                        d = doc;
                        moved = true;
                    }
                    Some(_) => {}
                }
            }
            if !moved {
                return Ok(Some(d));
            }
        }
    }

    fn block_end_at(&mut self, target: u64) -> Result<u64> {
        let mut end = u64::MAX;
        for child in &mut self.children {
            end = end.min(
                child
                    .as_block()
                    .ok_or_else(|| Error::internal("conjunction child is not a block producer"))?
                    .block_end_at(target)?,
            );
        }
        Ok(end)
    }

    fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds> {
        let mut bounds = ScoreBounds::ZERO;
        for child in &mut self.children {
            bounds = bounds.add(
                child
                    .as_block()
                    .ok_or_else(|| Error::internal("conjunction child is not a block producer"))?
                    .upper_bound(window)?,
            );
        }
        Ok(bounds)
    }

    fn next_block(
        &mut self,
        target: u64,
        budget: usize,
        out: &mut BlockBuf,
    ) -> Result<BlockStatus> {
        let Some(d) = self.seek(target)? else {
            return Ok(BlockStatus::Exhausted);
        };
        // The intersection window ends where any clause leaves its compressed
        // block. Cap the lead's budget at that end: a lead block reaching past
        // it would advance the lead's internal slice cursor beyond documents
        // the conjunction still has to emit on the next call.
        let window_end = self.block_end_at(d)?;
        let window_len = window_end
            .saturating_sub(d)
            .saturating_add(1)
            .min(usize::MAX as u64) as usize;
        // Read the bound while every child still sits in the window's block:
        // producing the lead block and evaluating the other children move the
        // cursors past it, and a later bound would describe a different block.
        let upper = self.upper_bound(&Window {
            start: d,
            up_to: window_end,
        })?;
        let lead = self.cost_order[0];
        let status = self.children[lead]
            .as_block()
            .ok_or_else(|| Error::internal("conjunction child is not a block producer"))?
            .next_block(d, budget.min(window_len), &mut self.lead_buf)?;
        match status {
            BlockStatus::Exhausted => return Ok(BlockStatus::Exhausted),
            BlockStatus::Empty { .. } => return Ok(status),
            BlockStatus::Data => {}
        }
        let up_to = self.lead_buf.up_to.min(window_end);
        debug_assert_eq!(self.lead_buf.docs.len(), self.lead_buf.keys.len());
        // Capping the lead budget makes the lead block end at `window_end`, so
        // this truncation is a no-op for a child that honours its own window
        // bound. It exists only to keep a child that produces past `up_to` from
        // being evaluated: that would move the clause cursors beyond `up_to`
        // and lose those documents on the next call.
        let mut docs = self.lead_buf.docs.clone();
        docs.truncate(docs.partition_point(|&doc| doc <= up_to));
        debug_assert_eq!(docs.len(), self.lead_buf.docs.len());
        let mut scores = vec![0.0_f32; docs.len()];
        let mut matched = vec![true; docs.len()];
        let mut eval = EvalOut::default();
        for &index in &self.score_order {
            if index == lead {
                // Producing the lead block can leave the lead cursor on the
                // first document after the window (the OR kernel scans the
                // window), so evaluating the lead again would report no match.
                // The produced block already carries the lead's exact score.
                debug_assert!(self.lead_buf.scores.len() >= docs.len());
                for (score, lead_score) in scores.iter_mut().zip(&self.lead_buf.scores) {
                    *score += *lead_score;
                }
                continue;
            }
            self.children[index]
                .as_eval()
                .ok_or_else(|| Error::internal("conjunction child has no batch evaluator"))?
                .eval_at(&docs, &mut eval)?;
            for i in 0..docs.len() {
                scores[i] += eval.scores[i];
                matched[i] &= eval.matched[i];
            }
        }
        out.clear();
        out.start = d;
        out.up_to = up_to;
        // Composite blocks do not expose per-clause frequencies; `freqs` stays
        // empty and `clause_count` stays zero so the two cannot disagree.
        out.clause_count = 0;
        for i in 0..docs.len() {
            if matched[i] {
                out.docs.push(docs[i]);
                out.scores.push(scores[i]);
                out.keys.push(self.lead_buf.keys[i]);
                out.matched.push(true);
            }
        }
        out.upper = upper;
        if out.docs.is_empty() {
            Ok(BlockStatus::Empty { up_to })
        } else {
            Ok(BlockStatus::Data)
        }
    }
}

impl BatchEvaluator for ConjunctionProducer<'_> {
    fn eval_at(&mut self, docs: &[u64], out: &mut EvalOut) -> Result<()> {
        out.clear(docs.len());
        out.matched.fill(true);
        let mut eval = EvalOut::default();
        for &index in &self.score_order {
            self.children[index]
                .as_eval()
                .ok_or_else(|| Error::internal("conjunction child has no batch evaluator"))?
                .eval_at(docs, &mut eval)?;
            for i in 0..docs.len() {
                out.scores[i] += eval.scores[i];
                out.matched[i] &= eval.matched[i];
            }
        }
        Ok(())
    }
}

impl BlockScorer for ConjunctionProducer<'_> {
    fn caps(&self) -> Capacities {
        Capacities::NONE
    }
    fn seek(&mut self, target: u64) -> Result<Option<u64>> {
        ConjunctionProducer::seek(self, target)
    }
    fn next_block(
        &mut self,
        target: u64,
        budget: usize,
        out: &mut BlockBuf,
    ) -> Result<BlockStatus> {
        ConjunctionProducer::next_block(self, target, budget, out)
    }
    fn block_end_at(&mut self, target: u64) -> Result<u64> {
        ConjunctionProducer::block_end_at(self, target)
    }
    fn upper_bound(&mut self, window: &Window) -> Result<ScoreBounds> {
        ConjunctionProducer::upper_bound(self, window)
    }
}

pub(super) struct BlockConjunctionScorer<'a> {
    producer: ConjunctionProducer<'a>,
    state: BlockState,
    current: Option<u64>,
}

impl<'a> BlockConjunctionScorer<'a> {
    pub(super) fn try_new(children: Vec<BoxScorer<'a>>) -> Result<Self> {
        Ok(Self {
            producer: ConjunctionProducer::new(children)?,
            state: BlockState::default(),
            current: None,
        })
    }
}

impl ComposableScorer for BlockConjunctionScorer<'_> {
    fn doc(&self) -> Option<u64> {
        self.current
    }
    fn document_key(&self) -> Option<u64> {
        self.state.current_key()
    }
    fn next(&mut self) -> Result<Option<u64>> {
        self.current = self.state.next(&mut self.producer)?;
        Ok(self.current)
    }
    fn advance(&mut self, target: u64) -> Result<Option<u64>> {
        self.current = self.state.advance(&mut self.producer, target)?;
        Ok(self.current)
    }
    fn cost(&self) -> usize {
        self.producer.cost()
    }
    fn score(&mut self) -> Result<f32> {
        self.state
            .current_score()
            .ok_or_else(|| Error::internal("block conjunction is not positioned"))
    }
    fn advance_shallow(&mut self, target: u64) -> Result<u64> {
        Ok(self.state.block.up_to.max(target))
    }
    fn score_bounds(&mut self, up_to: u64) -> Result<ScoreBounds> {
        if up_to <= self.state.block.up_to {
            Ok(self.state.block.upper)
        } else {
            Ok(ScoreBounds::UNBOUNDED)
        }
    }
    fn global_score_upper_bound(&self) -> Option<f32> {
        None
    }
    fn set_min_competitive_score(&mut self, _min_score: f32) -> Result<()> {
        Ok(())
    }
    fn current_score_upper_bound(&mut self) -> Result<Option<f32>> {
        Ok(self.state.current_score())
    }
    fn matches(&mut self) -> Result<bool> {
        Ok(true)
    }
    fn scores_non_negative(&self) -> bool {
        true
    }
    fn as_eval(&mut self) -> Option<&mut dyn BatchEvaluator> {
        Some(&mut self.producer)
    }
    fn as_block(&mut self) -> Option<&mut dyn BlockScorer> {
        Some(&mut self.producer)
    }
}
