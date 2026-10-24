// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Direct stepping for a cursor whose query is one compressed posting.
//!
//! A one-term query needs no heap: the posting itself is the candidate order.
//! The cursor only has to skip blocks whose bound cannot reach the competitive
//! floor, keep the visibility check, and decode frequencies when a positioned
//! document is actually scored.

use std::cell::OnceCell;

use lance_core::{Error, Result};

use super::super::scorer::Scorer;
use super::and_kernel::{self, LeadBound};
use super::{
    CompetitiveFloorMode, DocInfo, PostingIterator, RawDocInfo, TERMINATED_DOC_ID, WandDocuments,
    conservative_score_sum, cursor_term_score, posting_range_upper_bound,
};

/// Walk state of the single-posting route.
///
/// Holds only what a direct walk needs; the document mapping, the scorer and
/// the metric sink stay on the cursor that owns this state.
pub(super) struct SinglePosting {
    posting: PostingIterator,
    /// Whether `position` has run at least once. The first `next` starts at 0;
    /// every later one starts after `current_doc`.
    started: bool,
    /// Set once the walk was told the remaining range is over for good: no
    /// posting holds a document past the doc-id domain, so every later call
    /// must answer `None` too. Without the latch a smaller target afterwards
    /// would revive the cursor and hand out documents again, which the batched
    /// conjunction route avoids through `state.done`.
    done: bool,
    current_doc: Option<DocInfo>,
    current_document_key: Option<u64>,
    /// Computed on the first `score`; most positioned documents are never
    /// scored.
    current_score: Option<f32>,
    /// `(target, up_to, upper)` of the last `advance_shallow`.
    shallow: Option<(u64, u64, f32)>,
    global_score_upper_bound: OnceCell<Option<f32>>,
    /// The one lead-bound decision for this posting. The single route is
    /// built only for cursors, so it starts in inclusive mode; `position`
    /// re-aligns the mode if a caller ever passes the other one.
    lead_bound: LeadBound,
    comparisons: usize,
}

impl SinglePosting {
    pub(super) fn new(posting: PostingIterator) -> Self {
        Self {
            posting,
            started: false,
            done: false,
            current_doc: None,
            current_document_key: None,
            current_score: None,
            shallow: None,
            global_score_upper_bound: OnceCell::new(),
            lead_bound: LeadBound::new(CompetitiveFloorMode::Inclusive, 1),
            comparisons: 0,
        }
    }

    pub(super) fn doc(&self) -> Option<u64> {
        self.current_doc.map(|doc| doc.doc_id())
    }

    pub(super) fn document_key(&self) -> Option<u64> {
        self.current_document_key
    }

    pub(super) fn comparisons(&self) -> usize {
        self.comparisons
    }

    /// Drops the positioned document and everything derived from it.
    pub(super) fn clear_current(&mut self) {
        self.current_doc = None;
        self.current_document_key = None;
        self.current_score = None;
        self.shallow = None;
    }

    /// Where the next `next()` search starts: 0 before the first positioning,
    /// one past the current document afterwards, and nothing once the posting
    /// is exhausted.
    pub(super) fn next_target(&self) -> Option<u64> {
        match self.current_doc {
            Some(doc) => Some(doc.doc_id().saturating_add(1)),
            None if !self.started => Some(0),
            None => None,
        }
    }

    /// Positions on the first visible document at or after `target`.
    ///
    /// With a floor, a block whose conservative bound cannot reach it is
    /// skipped whole. The bound is anchored on the block that actually holds
    /// the match, so a block cursor that stopped short of `target` can never
    /// bound a document it does not contain.
    pub(super) fn position<D, S>(
        &mut self,
        mut target: u64,
        documents: &D,
        scorer: &S,
        floor: f32,
        floor_mode: CompetitiveFloorMode,
    ) -> Option<u64>
    where
        D: WandDocuments,
        S: Scorer + ?Sized,
    {
        self.clear_current();
        self.started = true;
        if self.done {
            return None;
        }
        loop {
            // Compressed postings hold u32 doc ids, so a larger target is past
            // every posting; `PostingIterator::next` would truncate it. The
            // call ends here and so does the cursor: a later smaller target
            // must not hand out documents the earlier one skipped over.
            if target > u64::from(u32::MAX) {
                self.done = true;
                return None;
            }
            let mut up_to = target;
            if floor > 0.0 {
                self.posting.shallow_next(target);
                up_to = and_kernel::posting_block_up_to(&self.posting, target);
                let upper =
                    conservative_score_sum(std::iter::once(self.posting.block_max_score(scorer)));
                // The old test was `floor_mode.rejects_upper_bound(upper, floor)`.
                // With `others = 0.0` and one clause (`factor = 1`),
                // `score_sum_cannot_compete(s, 0.0, floor, 1.0, mode)` reduces
                // to `outward_f32_upper_bound(f64::from(s))` -- exactly `s` for
                // every `f32` -- compared to the floor, so the limit is the
                // largest rejected `f32` (the floor itself in exclusive mode,
                // its predecessor in inclusive mode) and `rejects_score(upper)`
                // accepts unless `upper` was already rejected. The skipped set
                // is therefore unchanged.
                self.lead_bound.set_floor_mode(floor_mode);
                self.lead_bound.prepare(&self.posting, scorer, floor, 0.0);
                if self.lead_bound.rejects_score(upper) {
                    if up_to == TERMINATED_DOC_ID {
                        return None;
                    }
                    #[cfg(test)]
                    super::cursor_counters::single_posting_block_skipped();
                    target = up_to.saturating_add(1);
                    continue;
                }
            }
            // `next_doc_id` materializes the doc id only; the frequency stays
            // undecoded until `score`.
            self.posting.next_doc_id(target, false);
            let doc_id = self.posting.current_doc_id()?;
            if floor > 0.0 && doc_id > up_to {
                // The anchored block ends before this document: bound the
                // block that holds it before deciding anything.
                target = doc_id;
                continue;
            }
            self.comparisons += 1;
            let doc = DocInfo::Raw(RawDocInfo {
                doc_id: doc_id as u32,
                frequency: 0,
            });
            let Some(document_key) = documents.document_key(&doc) else {
                target = doc_id.saturating_add(1);
                continue;
            };
            self.current_doc = Some(doc);
            self.current_document_key = Some(document_key);
            return Some(doc_id);
        }
    }

    /// The current document's score, computed on first use.
    pub(super) fn score<D, S>(
        &mut self,
        documents: &D,
        scorer: &S,
        norm_k: Option<(&[u8], &[f32; 256])>,
    ) -> Result<f32>
    where
        D: WandDocuments,
        S: Scorer + ?Sized,
    {
        if let Some(score) = self.current_score {
            return Ok(score);
        }
        let doc = self
            .current_doc
            .ok_or_else(|| Error::internal("posting FTS scorer is not positioned on a document"))?;
        // Reading the document decodes this block's frequencies, which is the
        // only place the walk needs them.
        let posting_doc = self
            .posting
            .doc()
            .ok_or_else(|| Error::internal("posting FTS scorer is not positioned on a document"))?;
        // Same fold-from-zero as the query-order sum of a one-clause cursor, so
        // the bits match the heap route. A cache hit reads the addend instead of
        // the document length.
        let score = 0.0_f32
            + cursor_term_score(
                &self.posting,
                scorer,
                posting_doc.frequency(),
                posting_doc.doc_id() as u32,
                || documents.doc_length(&doc),
                norm_k,
            );
        self.current_score = Some(score);
        Ok(score)
    }

    /// Bounds `[target, up_to]` without moving the current document.
    ///
    /// The probe anchors the block cursor ahead when `target` is far from the
    /// current document, so the anchor is put back before returning: a later
    /// skip must bound the block that holds the document it is about to leave.
    pub(super) fn advance_shallow<S: Scorer + ?Sized>(&mut self, target: u64, scorer: &S) -> u64 {
        // Compressed postings hold u32 doc ids, so a target past that domain
        // bounds an empty range; `shallow_next` would truncate it. Report the
        // terminated range the blocked conjunction route reports for a range
        // with no documents left (`compound_shallow_bound` returns
        // `TERMINATED_DOC_ID` with an exhausted block's 0.0 bound) and leave
        // the current document where it is.
        if target > u64::from(u32::MAX) {
            self.shallow = Some((target, TERMINATED_DOC_ID, 0.0));
            return TERMINATED_DOC_ID;
        }
        let anchored_block = self.posting.block_idx;
        self.posting.shallow_next(target);
        let (up_to, upper) = match self.posting.block_first_doc() {
            // `target` falls in the gap before the anchored block: no postings.
            Some(first) if first > target => (first.saturating_sub(1).max(target), 0.0),
            _ => {
                let up_to = self.posting.block_end_doc().max(target);
                let upper = conservative_score_sum(std::iter::once(
                    self.posting.window_max_score(Some(up_to), scorer),
                ));
                (up_to, upper)
            }
        };
        self.posting.block_idx = anchored_block;
        self.shallow = Some((target, up_to, upper));
        up_to
    }

    pub(super) fn score_upper_bound<S: Scorer + ?Sized>(
        &mut self,
        up_to: u64,
        scorer: &S,
    ) -> Result<f32> {
        let (target, shallow_up_to, upper) = self.shallow.ok_or_else(|| {
            Error::internal("score bound requires advance_shallow on the posting FTS scorer")
        })?;
        if up_to < target {
            return Err(Error::internal(format!(
                "posting FTS score bound up_to={up_to} ends before its target {target}"
            )));
        }
        // Inside the reported range the cached bound is returned unchanged. A
        // wider request is widened, not clamped: the posting's block cursor
        // never parks past the shallow target, so a forward step reaches the
        // block holding `target` and the helper folds every block up to
        // `up_to`.
        if up_to <= shallow_up_to {
            return Ok(upper);
        }
        Ok(posting_range_upper_bound(
            &mut self.posting,
            target,
            up_to,
            scorer,
            false,
        ))
    }

    pub(super) fn global_score_upper_bound<S: Scorer + ?Sized>(&self, scorer: &S) -> Option<f32> {
        *self.global_score_upper_bound.get_or_init(|| {
            let upper =
                conservative_score_sum(std::iter::once(self.posting.global_upper_bound(scorer)));
            (upper.is_finite() && upper >= 0.0).then_some(upper)
        })
    }

    #[cfg(test)]
    pub(super) fn frequency_blocks_decoded(&self) -> usize {
        self.posting.frequency_blocks_decoded()
    }
}
