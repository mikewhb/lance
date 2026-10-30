// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! One explicit tiered merge group for a disk FTS index.
//!
//! Default optimize, `append()`, and `merge(n)` do not enter this module.
//! A tiered call plans at most one group for each logical inverted index.
//! The task lists only that group, so the existing inverted merge publishes
//! its bitmap and per-segment deletion filter unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::FutureExt;
use lance_index::metrics::NoOpMetricsCollector;
use lance_index::optimize::{FtsTieredMergeOptions, OptimizeOptions};
use lance_index::scalar::inverted::{
    DocumentGranularity, InvertedIndex, InvertedMergeCompatibility,
};
use lance_table::format::{Fragment, IndexMetadata};
use prost::Message;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::DatasetIndexInternalExt;
use super::append::{index_field_path, tagged_segment_coverage};
use super::optimize::{OptimizeIndexMerge, OptimizeIndexTask, OptimizeIndicesPlan};
use crate::dataset::Dataset;
use crate::dataset::fragment::FileFragment;
use crate::{Error, Result};

/// Reject a tiered call that would also change the existing optimize plan.
pub(crate) fn validate_options(options: &OptimizeOptions) -> Result<()> {
    let Some(tiered) = &options.fts_tiered else {
        return Ok(());
    };
    if tiered.segments_per_tier < 2 {
        return Err(Error::invalid_input(format!(
            "fts_tiered.segments_per_tier must be >= 2, got {}",
            tiered.segments_per_tier
        )));
    }
    if tiered.min_rows_per_segment < 1 {
        return Err(Error::invalid_input(format!(
            "fts_tiered.min_rows_per_segment must be >= 1, got {}",
            tiered.min_rows_per_segment
        )));
    }
    if tiered.max_rows_per_segment <= tiered.min_rows_per_segment {
        return Err(Error::invalid_input(format!(
            "fts_tiered.max_rows_per_segment ({}) must be greater than min_rows_per_segment ({})",
            tiered.max_rows_per_segment, tiered.min_rows_per_segment
        )));
    }
    if options.num_indices_to_merge.is_some()
        || options.max_rows_per_task.is_some()
        || options.max_rows_per_segment.is_some()
        || options.max_rows_to_consolidate.is_some()
        || options.retrain
    {
        return Err(Error::invalid_input(
            "fts_tiered cannot be combined with num_indices_to_merge, max_rows_per_task, \
             max_rows_per_segment, max_rows_to_consolidate, or retrain",
        ));
    }
    Ok(())
}

/// Add at most one FTS merge task to `plan`.
///
/// An empty plan leaves unindexed fragments alone. Creating a segment for
/// new rows stays on [`OptimizeOptions::append`].
pub(crate) async fn plan_index(
    plan: &mut OptimizeIndicesPlan,
    dataset: &Dataset,
    segments: &[&IndexMetadata],
    unindexed: &[Fragment],
    options: &OptimizeOptions,
) -> Result<()> {
    let Some(tiered) = options.fts_tiered.as_ref() else {
        return Ok(());
    };
    let Some(group) = select_index_group(dataset, segments, unindexed, &tiered)
        .boxed()
        .await?
    else {
        return Ok(());
    };
    if group.segment_indexes.is_empty() {
        return Ok(());
    }

    let (plan_id, read_version) = (plan.plan_id, plan.read_version);
    let merge_group = plan.merges.len() as u32;
    let old_segments: Vec<IndexMetadata> = group
        .segment_indexes
        .iter()
        .map(|&index| (*segments[index]).clone())
        .collect();
    let reference = old_segments
        .last()
        .expect("a tiered group has a segment")
        .clone();
    let fragments = if group.absorb_unindexed {
        unindexed
            .iter()
            .map(|fragment| fragment.id as u32)
            .collect()
    } else {
        Vec::new()
    };
    plan.tasks.push(OptimizeIndexTask {
        plan_id,
        read_version,
        group: merge_group,
        fragments,
        reference,
        // The execute path selects `num_indices_to_merge` segments from this
        // list. Listing only the planned group, and asking for all of them,
        // keeps the existing per-segment filter and bitmap rules.
        num_indices_to_merge: Some(old_segments.len()),
        enable_partition_rebalancing: false,
        old_segments,
        retrain: false,
    });
    plan.merges.push(OptimizeIndexMerge {
        plan_id,
        read_version,
        group: merge_group,
        old_segments: Vec::new(),
        replaces: Vec::new(),
    });
    Ok(())
}

struct PlannedGroup {
    segment_indexes: Vec<usize>,
    absorb_unindexed: bool,
}

struct OpenedSegment {
    index: usize,
    uuid: Uuid,
    live_rows: u64,
    is_deletion: bool,
    legacy: bool,
    compatibility: InvertedMergeCompatibility,
}

async fn select_index_group(
    dataset: &Dataset,
    segments: &[&IndexMetadata],
    unindexed: &[Fragment],
    options: &FtsTieredMergeOptions,
) -> Result<Option<PlannedGroup>> {
    if segments.is_empty() && unindexed.is_empty() {
        return Ok(None);
    }
    if segments.is_empty() {
        return Ok(None);
    }

    let opened = open_segments(dataset, segments).boxed().await?;
    let mut classes: Vec<Vec<OpenedSegment>> = Vec::new();
    for segment in opened {
        if let Some(class) = classes
            .iter_mut()
            .find(|class| class[0].compatibility == segment.compatibility)
        {
            class.push(segment);
        } else {
            classes.push(vec![segment]);
        }
    }
    classes.sort_by(|left, right| min_uuid(left).cmp(&min_uuid(right)));

    let unindexed_rows = visible_rows_of_fragments(dataset, unindexed)
        .boxed()
        .await?;
    let has_unindexed = !unindexed.is_empty();
    for class in classes {
        if class.iter().any(|segment| segment.legacy) {
            log::warn!(
                "Skipping an FTS compatibility class during tiered compaction because it contains \
                 a legacy segment; legacy inverted indexes are still optimized without fts_tiered"
            );
            continue;
        }
        if let Some(group) = select_class_group(&class, unindexed_rows, has_unindexed, options) {
            return Ok(Some(group));
        }
    }
    Ok(None)
}

fn min_uuid(class: &[OpenedSegment]) -> Uuid {
    class
        .iter()
        .map(|segment| segment.uuid)
        .min()
        .expect("a compatibility class has a segment")
}

async fn open_segments(
    dataset: &Dataset,
    segments: &[&IndexMetadata],
) -> Result<Vec<OpenedSegment>> {
    let (field_path, _) = index_field_path(dataset, segments[0]).boxed().await?;
    let tagged = tagged_segment_coverage(dataset, segments, None)
        .boxed()
        .await?;
    let deleted_fragments = deletion_fragments(dataset);
    let mut opened = Vec::with_capacity(segments.len());
    for (index, segment) in segments.iter().enumerate() {
        let coverage = segment_coverage(dataset, segment, tagged.as_ref());
        let live_rows = bitmap_visible_rows(dataset, &coverage).boxed().await?;
        let is_deletion = !coverage.is_empty() && !coverage.is_disjoint(&deleted_fragments);
        let scalar = dataset
            .open_scalar_index_for_maintenance(&field_path, &segment.uuid, &NoOpMetricsCollector)
            .boxed()
            .await?;
        let inverted = scalar
            .as_any()
            .downcast_ref::<InvertedIndex>()
            .ok_or_else(|| {
                Error::index(format!(
                    "FTS tiered compaction expected an inverted segment {}, got {:?}",
                    segment.uuid,
                    scalar.index_type()
                ))
            })?;
        let granularity = manifest_document_granularity(segment)?;
        let mut compatibility = inverted.merge_compatibility();
        compatibility.params = compatibility.params.document_granularity(granularity);
        opened.push(OpenedSegment {
            index,
            uuid: segment.uuid,
            live_rows,
            is_deletion,
            legacy: inverted.is_legacy(),
            compatibility,
        });
    }
    Ok(opened)
}

fn deletion_fragments(dataset: &Dataset) -> RoaringBitmap {
    dataset
        .fragments()
        .iter()
        .filter(|fragment| fragment.deletion_file.is_some())
        .map(|fragment| fragment.id as u32)
        .collect()
}

/// Coverage used for live rows and deletion membership.
///
/// A missing bitmap is not coverage. On a tagged table the stored bitmap names
/// retired sources, so the translated reader coverage is the one that still
/// has rows.
fn segment_coverage(
    dataset: &Dataset,
    segment: &IndexMetadata,
    tagged: Option<&std::collections::HashMap<Uuid, RoaringBitmap>>,
) -> RoaringBitmap {
    if let Some(tagged) = tagged {
        return tagged.get(&segment.uuid).cloned().unwrap_or_default();
    }
    segment
        .effective_fragment_bitmap(&dataset.fragment_bitmap)
        .unwrap_or_default()
}

fn manifest_document_granularity(segment: &IndexMetadata) -> Result<DocumentGranularity> {
    let details = segment.index_details.as_ref().ok_or_else(|| {
        Error::invalid_input(format!(
            "FTS tiered compaction: segment {} is missing index details",
            segment.uuid
        ))
    })?;
    let decoded = lance_index::pbold::InvertedIndexDetails::decode(details.value.as_slice())
        .map_err(|error| {
            Error::corrupt_file_named(
                &segment.uuid.to_string(),
                format!("failed to decode InvertedIndexDetails: {error}"),
            )
        })?;
    DocumentGranularity::try_from(decoded.document_granularity)
}

async fn visible_rows_of_fragments(dataset: &Dataset, fragments: &[Fragment]) -> Result<u64> {
    let mut total = 0u64;
    for fragment in fragments {
        total = total.saturating_add(visible_rows(dataset, fragment).boxed().await?);
    }
    Ok(total)
}

async fn bitmap_visible_rows(dataset: &Dataset, bitmap: &RoaringBitmap) -> Result<u64> {
    let mut total = 0u64;
    for id in bitmap.iter() {
        let fragment = dataset
            .find_fragment(u64::from(id))
            .ok_or_else(|| {
                Error::internal(format!(
                    "FTS tiered compaction: fragment {id} is in a segment bitmap but not the dataset"
                ))
            })?
            .clone();
        total = total.saturating_add(visible_rows(dataset, &fragment).boxed().await?);
    }
    Ok(total)
}

/// Visible rows of one fragment.
///
/// [`Fragment::num_rows`] is the fast path. When it is `None` and
/// `physical_rows` is known, subtract the deletion vector's deleted count.
/// Otherwise scan the visible rows. The deletion count is not itself the
/// visible-row count, and the indexed document count is not used.
async fn visible_rows(dataset: &Dataset, fragment: &Fragment) -> Result<u64> {
    if let Some(rows) = fragment.num_rows() {
        return Ok(rows as u64);
    }
    if let Some(physical_rows) = fragment.physical_rows {
        let file_fragment = FileFragment::new(Arc::new(dataset.clone()), fragment.clone());
        let deleted = file_fragment
            .get_deletion_vector()
            .boxed()
            .await?
            .map(|vector| vector.len() as u64)
            .unwrap_or(0);
        return Ok((physical_rows as u64).saturating_sub(deleted));
    }
    Ok(dataset
        .count_rows_in_fragments(&[fragment.id as u32])
        .boxed()
        .await? as u64)
}

struct ClassGroup {
    indexes: Vec<usize>,
    absorb_unindexed: bool,
}

fn select_class_group(
    class: &[OpenedSegment],
    unindexed_rows: u64,
    has_unindexed: bool,
    options: &FtsTieredMergeOptions,
) -> Option<PlannedGroup> {
    if let Some(group) = deletion_group(class, has_unindexed, options.segments_per_tier) {
        return Some(PlannedGroup {
            segment_indexes: group,
            absorb_unindexed: true,
        });
    }
    normal_group(class, unindexed_rows, options).map(|group| PlannedGroup {
        segment_indexes: group.indexes,
        absorb_unindexed: group.absorb_unindexed,
    })
}

/// Deletion segments, smallest UUID first, at most one tier wide.
///
/// Two or more merge even when their rows exceed the ceiling. One merges
/// only when the index still has unindexed fragments. A lone deletion
/// segment with nothing new to index is left alone so a deletion file that
/// survives the rewrite cannot spin the planner.
fn deletion_group(
    class: &[OpenedSegment],
    has_unindexed: bool,
    segments_per_tier: usize,
) -> Option<Vec<usize>> {
    let mut deletion: Vec<&OpenedSegment> =
        class.iter().filter(|segment| segment.is_deletion).collect();
    if deletion.is_empty() {
        return None;
    }
    deletion.sort_by(|left, right| left.uuid.cmp(&right.uuid));
    if deletion.len() >= 2 {
        let take = deletion.len().min(segments_per_tier);
        return Some(
            deletion
                .iter()
                .take(take)
                .map(|segment| segment.index)
                .collect(),
        );
    }
    if has_unindexed {
        return Some(vec![deletion[0].index]);
    }
    None
}

/// The first normal window in the smallest full tier.
///
/// Segments at or above the ceiling are neither counted nor windowed. A tier
/// with fewer than `segments_per_tier` below-ceiling segments does not open a
/// window, so two equal segments stay put until the tier is full. Shrinking
/// drops the large end of an already-full window and does not pull in a
/// replacement.
fn normal_group(
    class: &[OpenedSegment],
    unindexed_rows: u64,
    options: &FtsTieredMergeOptions,
) -> Option<ClassGroup> {
    let ceiling = options.max_rows_per_segment;
    let mut tiers: BTreeMap<u32, Vec<&OpenedSegment>> = BTreeMap::new();
    for segment in class {
        if segment.live_rows >= ceiling {
            continue;
        }
        tiers
            .entry(tier_key(segment.live_rows, options.min_rows_per_segment))
            .or_default()
            .push(segment);
    }

    for tier in tiers.values_mut() {
        if tier.len() < options.segments_per_tier {
            continue;
        }
        tier.sort_by(|left, right| {
            left.live_rows
                .cmp(&right.live_rows)
                .then_with(|| left.uuid.cmp(&right.uuid))
        });
        let mut start = 0;
        while start + options.segments_per_tier <= tier.len() {
            let mut end = start + options.segments_per_tier;
            loop {
                if end - start < 2 {
                    break;
                }
                let window = &tier[start..end];
                let sum: u128 = window.iter().map(|segment| segment.live_rows as u128).sum();
                let max_rows = window
                    .iter()
                    .map(|segment| segment.live_rows as u128)
                    .max()
                    .expect("a window of two or more segments has a max");
                let fits = sum <= ceiling as u128;
                let balanced = sum.saturating_mul(2) >= max_rows.saturating_mul(3);
                if fits && balanced {
                    let absorb_unindexed = sum + unindexed_rows as u128 <= ceiling as u128;
                    return Some(ClassGroup {
                        indexes: window.iter().map(|segment| segment.index).collect(),
                        absorb_unindexed,
                    });
                }
                end -= 1;
            }
            start += 1;
        }
    }
    None
}

/// `floor(log2(max(live_rows, 1)))`, or the floor's tier when the segment is
/// below `min_rows_per_segment`.
fn tier_key(live_rows: u64, min_rows_per_segment: u64) -> u32 {
    let rows = if live_rows < min_rows_per_segment {
        min_rows_per_segment
    } else {
        live_rows.max(1)
    };
    floor_log2(rows)
}

fn floor_log2(n: u64) -> u32 {
    debug_assert!(n >= 1);
    63 - n.leading_zeros()
}
