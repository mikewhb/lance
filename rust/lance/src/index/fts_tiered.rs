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

#[cfg(test)]
impl OpenedSegment {
    fn test_uuid(&self) -> Uuid {
        self.uuid
    }

    fn test_live_rows(&self) -> u64 {
        self.live_rows
    }
}

#[cfg(test)]
fn opened_for_test(index: usize, live_rows: u64, is_deletion: bool, uuid: Uuid) -> OpenedSegment {
    use lance_index::scalar::inverted::{
        InvertedListFormatVersion, PostingTailCodec, TokenSetFormat,
    };

    OpenedSegment {
        index,
        uuid,
        live_rows,
        is_deletion,
        legacy: false,
        compatibility: InvertedMergeCompatibility {
            params: lance_index::scalar::inverted::InvertedIndexParams::default(),
            token_set_format: TokenSetFormat::Fst,
            format_version: InvertedListFormatVersion::V2,
            posting_tail_codec: PostingTailCodec::VarintDelta,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::{Arc, Mutex};

    use arrow_array::{Int32Array, RecordBatch, RecordBatchIterator, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use lance_core::utils::tempfile::TempStrDir;
    use lance_core::utils::testing::{ProxyObjectStore, ProxyObjectStorePolicy};
    use lance_index::IndexType;
    use lance_index::optimize::{FtsTieredMergeOptions, OptimizeOptions};
    use lance_index::pbold::InvertedIndexDetails;
    use lance_index::scalar::inverted::builder::InvertedIndexBuilder;
    use lance_index::scalar::inverted::{
        InvertedIndexParams, InvertedListFormatVersion, TokenSetFormat,
    };
    use lance_index::scalar::lance_format::LanceIndexStore;
    use lance_index::scalar::registry::{TrainingCriteria, TrainingOrdering};
    use lance_index::scalar::{FullTextSearchQuery, index_files_to_table};
    use lance_io::object_store::WrappingObjectStore;
    use lance_table::format::IndexMetadata;
    use uuid::Uuid;

    use super::super::append::select_segments_to_merge;
    use super::super::optimize::plan_optimize_indices;
    use super::super::{DatasetIndexExt, DatasetIndexInternalExt, load_all_indices};
    use super::{floor_log2, opened_for_test, select_class_group};
    use crate::Dataset;
    use crate::dataset::UpdateBuilder;
    use crate::dataset::WriteParams;
    use crate::dataset::index::LanceIndexStoreExt;
    use crate::index::create::CreateIndexBuilder;

    fn tiered(factor: usize, min_rows: u64, max_rows: u64) -> OptimizeOptions {
        OptimizeOptions::default()
            .fts_tiered(FtsTieredMergeOptions::new(min_rows, max_rows).segments_per_tier(factor))
    }

    fn params() -> InvertedIndexParams {
        InvertedIndexParams::default().lower_case(false)
    }

    async fn write_text(
        uri: &str,
        rows: usize,
        rows_per_fragment: usize,
        stable: bool,
        text_of: impl Fn(i32) -> String,
    ) -> Dataset {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("text", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from_iter_values(0..rows as i32)),
                Arc::new(StringArray::from_iter_values(
                    (0..rows as i32).map(&text_of),
                )),
            ],
        )
        .unwrap();
        Dataset::write(
            RecordBatchIterator::new(vec![Ok(batch)], schema),
            uri,
            Some(WriteParams {
                max_rows_per_file: rows_per_fragment,
                enable_stable_row_ids: stable,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
    }

    async fn index_fragments(
        dataset: &mut Dataset,
        fragment_ids: &[u32],
        parameters: &InvertedIndexParams,
        replace: bool,
    ) {
        let mut segments = Vec::new();
        for id in fragment_ids {
            segments.push(
                CreateIndexBuilder::new(dataset, &["text"], IndexType::Inverted, parameters)
                    .name("text_idx".to_string())
                    .fragments(vec![*id])
                    .replace(replace)
                    .execute_uncommitted()
                    .await
                    .unwrap(),
            );
        }
        dataset
            .commit_existing_index_segments("text_idx", "text", segments)
            .await
            .unwrap();
    }

    async fn index_every_fragment(dataset: &mut Dataset, parameters: &InvertedIndexParams) {
        let ids: Vec<u32> = dataset
            .get_fragments()
            .iter()
            .map(|fragment| fragment.id() as u32)
            .collect();
        index_fragments(dataset, &ids, parameters, false).await;
    }

    async fn text_segments(dataset: &Dataset) -> Vec<IndexMetadata> {
        load_all_indices(dataset)
            .await
            .unwrap()
            .iter()
            .filter(|index| index.name == "text_idx")
            .cloned()
            .collect()
    }

    async fn text_uuids(dataset: &Dataset) -> BTreeSet<Uuid> {
        text_segments(dataset)
            .await
            .into_iter()
            .map(|index| index.uuid)
            .collect()
    }

    async fn fts_hits(dataset: &Dataset, token: &str) -> usize {
        let mut scan = dataset.scan();
        scan.full_text_search(FullTextSearchQuery::new(token.to_owned()))
            .unwrap();
        scan.count_rows().await.unwrap() as usize
    }

    fn planned_group(
        plan: &super::super::optimize::OptimizeIndicesPlan,
    ) -> (BTreeSet<Uuid>, Vec<u32>) {
        assert!(
            plan.tasks.len() <= 1,
            "one logical FTS index plans at most one group, got {}",
            plan.tasks.len()
        );
        match plan.tasks.first() {
            Some(task) => (
                task.old_segments
                    .iter()
                    .map(|segment| segment.uuid)
                    .collect(),
                task.fragments.clone(),
            ),
            None => (BTreeSet::new(), Vec::new()),
        }
    }

    #[test]
    fn floor_log2_matches_the_disk_tier_key() {
        assert_eq!(floor_log2(1), 0);
        assert_eq!(floor_log2(2), 1);
        assert_eq!(floor_log2(1_000), 9);
        assert_eq!(floor_log2(8_000), 12);
    }

    #[test]
    fn tiered_option_defaults_off_and_ignores_the_split_row_bound() {
        let options = OptimizeOptions::default();
        assert!(options.fts_tiered.is_none());
        let options = options.fts_tiered(FtsTieredMergeOptions::new(10, 80));
        assert!(options.max_rows_per_segment.is_none());
        let tiered = options.fts_tiered.as_ref().unwrap();
        assert_eq!(tiered.segments_per_tier, 8);
        assert_eq!(tiered.max_rows_per_segment, 80);
    }

    #[test]
    fn a_short_tier_does_not_open_a_window_and_a_full_tier_does() {
        let options = FtsTieredMergeOptions::new(1, 1_000_000).segments_per_tier(8);
        let seven: Vec<_> = (0..7)
            .map(|index| opened_for_test(index, 1_000, false, Uuid::from_u128(index as u128)))
            .collect();
        assert!(select_class_group(&seven, 0, false, &options).is_none());

        let eight: Vec<_> = (0..8)
            .map(|index| opened_for_test(index, 1_000, false, Uuid::from_u128(index as u128)))
            .collect();
        let group = select_class_group(&eight, 0, false, &options).unwrap();
        assert_eq!(group.segment_indexes, (0..8).collect::<Vec<_>>());
        assert!(group.absorb_unindexed);

        let nine: Vec<_> = (0..9)
            .map(|index| {
                opened_for_test(index, 1_000, false, Uuid::from_u128((10 - index) as u128))
            })
            .collect();
        let group = select_class_group(&nine, 0, false, &options).unwrap();
        let selected: BTreeSet<_> = group
            .segment_indexes
            .iter()
            .map(|&index| nine[index].test_uuid())
            .collect();
        let smallest: BTreeSet<_> = (2..10).map(Uuid::from_u128).collect();
        assert_eq!(selected, smallest);
    }

    #[test]
    fn the_window_includes_its_start_and_keeps_searching_after_a_one_point_five_miss() {
        // Every row is below the floor, so the class is one tier. The first
        // start's full window and its shrink both fail 1.5. The next start,
        // still inside this full tier, is the group.
        let options = FtsTieredMergeOptions::new(128, 10_000).segments_per_tier(3);
        let rows = [1_u64, 100, 10, 40, 40, 40];
        let class: Vec<_> = rows
            .into_iter()
            .enumerate()
            .map(|(index, live_rows)| {
                opened_for_test(index, live_rows, false, Uuid::from_u128(index as u128))
            })
            .collect();
        let group = select_class_group(&class, 0, false, &options).unwrap();
        let selected: Vec<_> = group
            .segment_indexes
            .iter()
            .map(|&index| class[index].test_live_rows())
            .collect();
        assert_eq!(selected, vec![10, 40, 40]);
    }

    #[test]
    fn shrinking_drops_the_large_end_without_refilling() {
        // The large segment stays under the ceiling and shares the floor tier,
        // so the full window is real and then loses its large end.
        let options = FtsTieredMergeOptions::new(9_001, 100_000).segments_per_tier(8);
        let class: Vec<_> = (0..8)
            .map(|index| {
                let rows = if index == 7 { 9_000 } else { 100 };
                opened_for_test(index, rows, false, Uuid::from_u128(index as u128))
            })
            .collect();
        let group = select_class_group(&class, 0, false, &options).unwrap();
        assert_eq!(group.segment_indexes.len(), 7);
        assert!(!group.segment_indexes.contains(&7));
    }

    #[test]
    fn ceiling_segments_are_not_counted_and_deletion_groups_ignore_it() {
        let options = FtsTieredMergeOptions::new(1, 1_000).segments_per_tier(8);
        let mut class: Vec<_> = (0..8)
            .map(|index| opened_for_test(index, 100, false, Uuid::from_u128(index as u128)))
            .collect();
        class.push(opened_for_test(8, 1_000, false, Uuid::from_u128(100)));
        let group = select_class_group(&class, 0, false, &options).unwrap();
        assert_eq!(group.segment_indexes, (0..8).collect::<Vec<_>>());

        let deletions: Vec<_> = (0..2)
            .map(|index| opened_for_test(index, 5_000, true, Uuid::from_u128(index as u128)))
            .collect();
        let group = select_class_group(&deletions, 0, false, &options).unwrap();
        assert_eq!(group.segment_indexes, vec![0, 1]);
        assert!(group.absorb_unindexed);

        let lone = vec![opened_for_test(0, 10, true, Uuid::from_u128(1))];
        assert!(select_class_group(&lone, 0, false, &options).is_none());
        let group = select_class_group(&lone, 10, true, &options).unwrap();
        assert_eq!(group.segment_indexes, vec![0]);
        assert!(group.absorb_unindexed);
    }

    #[tokio::test]
    async fn invalid_tier_options_are_rejected_before_planning() {
        let dir = TempStrDir::default();
        let dataset = write_text(dir.as_str(), 1, 1, false, |_| "a".to_string()).await;
        let cases = vec![
            tiered(1, 1, 10),
            tiered(8, 0, 10),
            tiered(8, 10, 10),
            OptimizeOptions::append().fts_tiered(FtsTieredMergeOptions::new(1, 10)),
            OptimizeOptions::merge(2).fts_tiered(FtsTieredMergeOptions::new(1, 10)),
            OptimizeOptions::retrain().fts_tiered(FtsTieredMergeOptions::new(1, 10)),
            OptimizeOptions::consolidate(10).fts_tiered(FtsTieredMergeOptions::new(1, 10)),
            OptimizeOptions::default()
                .max_rows_per_segment(Some(10))
                .fts_tiered(FtsTieredMergeOptions::new(1, 100)),
            OptimizeOptions::default()
                .max_rows_per_task(Some(10))
                .fts_tiered(FtsTieredMergeOptions::new(1, 100)),
        ];
        for options in cases {
            let error = plan_optimize_indices(&dataset, &options).await.unwrap_err();
            assert!(
                error.to_string().contains("fts_tiered"),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn default_optimize_keeps_its_selection_and_data_commits_do_not_merge() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 6, 2, false, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        let schema = Arc::new(arrow_schema::Schema::from(dataset.schema()));
        let extra = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![6, 7])),
                Arc::new(StringArray::from(vec!["extra", "extra"])),
            ],
        )
        .unwrap();
        let before_data = text_uuids(&dataset).await;
        dataset
            .append(RecordBatchIterator::new(vec![Ok(extra)], schema), None)
            .await
            .unwrap();
        assert_eq!(text_uuids(&dataset).await, before_data);

        let indices = load_all_indices(&dataset).await.unwrap();
        let indexed: Vec<&IndexMetadata> = indices
            .iter()
            .filter(|index| index.name == "text_idx")
            .collect();
        let selected = select_segments_to_merge(&dataset, &indexed, &OptimizeOptions::default());
        let selected_uuids: BTreeSet<_> = selected.iter().map(|index| index.uuid).collect();
        dataset
            .optimize_indices(&OptimizeOptions::default())
            .await
            .unwrap();
        let after = text_uuids(&dataset).await;
        let removed: BTreeSet<_> = before_data.difference(&after).copied().collect();
        assert_eq!(removed, selected_uuids);
        assert!(!selected_uuids.is_empty());

        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 4, 2, false, |id| format!("tok{id}")).await;
        index_every_fragment(&mut dataset, &parameters).await;
        let schema = Arc::new(arrow_schema::Schema::from(dataset.schema()));
        let extra = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![4, 5])),
                Arc::new(StringArray::from(vec!["more", "more"])),
            ],
        )
        .unwrap();
        dataset
            .append(RecordBatchIterator::new(vec![Ok(extra)], schema), None)
            .await
            .unwrap();
        let before = text_uuids(&dataset).await;
        dataset
            .optimize_indices(&OptimizeOptions::append())
            .await
            .unwrap();
        let after = text_uuids(&dataset).await;
        assert!(before.is_subset(&after));
        assert_eq!(after.len(), before.len() + 1);
    }

    #[tokio::test]
    async fn tiered_waits_for_a_full_tier_and_does_not_index_new_rows_alone() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 2, 2, false, |_| "only".to_string()).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        let schema = Arc::new(arrow_schema::Schema::from(dataset.schema()));
        let extra = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int32Array::from(vec![2, 3])),
                Arc::new(StringArray::from(vec!["new", "new"])),
            ],
        )
        .unwrap();
        dataset
            .append(RecordBatchIterator::new(vec![Ok(extra)], schema), None)
            .await
            .unwrap();
        let version = dataset.version().version;
        let before = text_uuids(&dataset).await;
        dataset
            .optimize_indices(&tiered(8, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, before);
        assert!(
            !dataset
                .unindexed_fragments("text_idx")
                .await
                .unwrap()
                .is_empty()
        );

        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 16, 2, false, |id| format!("row{id}")).await;
        let ids: Vec<u32> = dataset
            .get_fragments()
            .iter()
            .map(|fragment| fragment.id() as u32)
            .collect();
        index_fragments(&mut dataset, &ids[..7], &parameters, false).await;
        let version = dataset.version().version;
        let seven = text_uuids(&dataset).await;
        dataset
            .optimize_indices(&tiered(8, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, seven);
        // The name already exists. replace only lets the builder emit another
        // segment; the commit keeps every disjoint segment already present.
        index_fragments(&mut dataset, &ids[7..], &parameters, true).await;
        dataset
            .optimize_indices(&tiered(8, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(text_segments(&dataset).await.len(), 1);
    }

    #[tokio::test]
    async fn two_updated_segments_use_their_own_allow_lists_and_then_stop() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 4, 2, true, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        for (predicate, replacement) in [("id = 0", "'new0'"), ("id = 2", "'new2'")] {
            let updated = UpdateBuilder::new(Arc::new(dataset.clone()))
                .update_where(predicate)
                .unwrap()
                .set("text", replacement)
                .unwrap()
                .build()
                .unwrap()
                .execute()
                .await
                .unwrap();
            dataset = updated.new_dataset.as_ref().clone();
        }
        dataset
            .optimize_indices(&tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(fts_hits(&dataset, "tok0").await, 0);
        assert_eq!(fts_hits(&dataset, "tok2").await, 0);
        assert_eq!(fts_hits(&dataset, "tok1").await, 1);
        assert_eq!(fts_hits(&dataset, "tok3").await, 1);
        assert_eq!(fts_hits(&dataset, "new0").await, 1);
        assert_eq!(fts_hits(&dataset, "new2").await, 1);
        let version = dataset.version().version;
        let uuids = text_uuids(&dataset).await;
        let plan = plan_optimize_indices(&dataset, &tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert!(plan.tasks.is_empty());
        dataset
            .optimize_indices(&tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, uuids);
    }

    #[tokio::test]
    async fn one_stable_segment_absorbs_its_update_once() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 2, 2, true, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        let updated = UpdateBuilder::new(Arc::new(dataset.clone()))
            .update_where("id = 0")
            .unwrap()
            .set("text", "'new0'")
            .unwrap()
            .build()
            .unwrap()
            .execute()
            .await
            .unwrap();
        dataset = updated.new_dataset.as_ref().clone();
        dataset
            .optimize_indices(&tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(fts_hits(&dataset, "tok0").await, 0);
        assert_eq!(fts_hits(&dataset, "tok1").await, 1);
        assert_eq!(fts_hits(&dataset, "new0").await, 1);
        let version = dataset.version().version;
        let uuids = text_uuids(&dataset).await;
        dataset
            .optimize_indices(&tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, uuids);
        assert_eq!(text_segments(&dataset).await.len(), 1);
    }

    #[tokio::test]
    async fn a_lone_deletion_segment_is_not_rewritten() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 2, 2, false, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        dataset.delete("id = 0").await.unwrap();
        let version = dataset.version().version;
        let uuids = text_uuids(&dataset).await;
        dataset
            .optimize_indices(&tiered(2, 1, 1_000))
            .await
            .unwrap();
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, uuids);
    }

    #[tokio::test]
    async fn nine_deletion_segments_merge_the_eight_smallest_uuids() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 18, 2, false, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        dataset.delete("id % 2 = 0").await.unwrap();
        let before = text_uuids(&dataset).await;
        assert_eq!(before.len(), 9);
        dataset
            .optimize_indices(&tiered(8, 1, 1_000_000))
            .await
            .unwrap();
        let after = text_uuids(&dataset).await;
        let mut ordered: Vec<_> = before.iter().copied().collect();
        ordered.sort();
        let kept = ordered[8];
        assert!(after.contains(&kept));
        for uuid in &ordered[..8] {
            assert!(
                !after.contains(uuid),
                "merged the larger uuid and kept {uuid}"
            );
        }
        assert_eq!(after.len(), 2);
    }

    async fn arrow_segment(
        dataset: &Dataset,
        fragment_id: u32,
        parameters: &InvertedIndexParams,
    ) -> IndexMetadata {
        let uuid = Uuid::new_v4();
        let store = LanceIndexStore::from_dataset_for_new(dataset, &uuid).unwrap();
        let resolved = crate::index::scalar::inverted::resolve_fts_field(
            dataset.schema(),
            "text",
            parameters.get_document_granularity(),
        )
        .unwrap();
        let criteria = TrainingCriteria::new(TrainingOrdering::None).with_row_id();
        let stream = crate::index::scalar::load_fts_training_data(
            dataset,
            &resolved,
            &criteria,
            None,
            true,
            Some(vec![fragment_id]),
        )
        .await
        .unwrap();
        let mut builder = InvertedIndexBuilder::new_with_fragment_mask(
            parameters.clone(),
            Some(u64::from(fragment_id) << 32),
        )
        .with_token_set_format(TokenSetFormat::Arrow);
        let files = builder.update(stream, &store, None).await.unwrap();
        let details = InvertedIndexDetails::try_from(parameters).unwrap();
        IndexMetadata {
            uuid,
            name: "text_idx".to_string(),
            fields: vec![dataset.schema().field("text").unwrap().id],
            covering_fields: vec![],
            dataset_version: dataset.manifest.version,
            fragment_bitmap: Some(std::iter::once(fragment_id).collect()),
            index_details: Some(Arc::new(prost_types::Any::from_msg(&details).unwrap())),
            index_version: parameters.resolved_format_version().index_version() as i32,
            created_at: Some(chrono::Utc::now()),
            base_id: None,
            files: Some(index_files_to_table(files)),
            clustering_id: None,
            metadata: HashMap::new(),
        }
    }

    /// Put segments with different inverted details into one index.
    ///
    /// `commit_existing_index_segments` rejects that mix, and an existing test
    /// pins the rejection. Tiered compaction still has to group classes when
    /// they are already in the manifest, so this fixture commits the operation
    /// directly.
    async fn commit_mixed_segments(dataset: &mut Dataset, segments: Vec<IndexMetadata>) {
        // Uncommitted inverted builds leave staged partition files. The public
        // commit finalizes them, then rejects this mix, so finalize here and
        // publish the manifest directly.
        let mut ready = Vec::with_capacity(segments.len());
        for mut segment in segments {
            crate::index::scalar::inverted::finalize_segment_files_if_needed(dataset, &segment)
                .await
                .unwrap();
            let index_dir = dataset.indices_dir().join(segment.uuid.to_string());
            let mut files = lance_table::format::list_index_files_with_sizes(
                dataset.object_store.as_ref(),
                &index_dir,
            )
            .await
            .unwrap();
            files
                .retain(|file| !lance_index::scalar::inverted::builder::is_staged_path(&file.path));
            segment.files = Some(files);
            ready.push(segment);
        }
        let transaction = crate::dataset::transaction::TransactionBuilder::new(
            dataset.manifest.version,
            crate::dataset::transaction::Operation::CreateIndex {
                new_indices: ready,
                removed_indices: Vec::new(),
            },
        )
        .build();
        dataset
            .apply_commit(transaction, &Default::default(), &Default::default())
            .await
            .unwrap();
    }

    async fn assert_one_class_merges(dataset: &mut Dataset, classes: [Vec<Uuid>; 2]) {
        let mins = [
            *classes[0].iter().min().unwrap(),
            *classes[1].iter().min().unwrap(),
        ];
        let first = if mins[0] < mins[1] { 0 } else { 1 };
        let plan = plan_optimize_indices(dataset, &tiered(2, 1, 1_000_000))
            .await
            .unwrap();
        let (selected, _) = planned_group(&plan);
        assert_eq!(selected, classes[first].iter().copied().collect());
        let again = plan_optimize_indices(dataset, &tiered(2, 1, 1_000_000))
            .await
            .unwrap();
        assert_eq!(planned_group(&again).0, selected);
        dataset
            .optimize_indices(&tiered(2, 1, 1_000_000))
            .await
            .unwrap();
        let after = text_uuids(dataset).await;
        for uuid in &classes[first] {
            assert!(!after.contains(uuid));
        }
        for uuid in &classes[1 - first] {
            assert!(after.contains(uuid));
        }
        assert_eq!(after.len(), 3);
    }

    #[tokio::test]
    async fn one_call_merges_only_the_class_with_the_smallest_uuid() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 8, 2, false, |id| format!("tok{id}")).await;
        let ids: Vec<u32> = dataset
            .get_fragments()
            .iter()
            .map(|fragment| fragment.id() as u32)
            .collect();

        let with_position = params().with_position(true);
        let without_position = params().with_position(false);
        let mut positioned = Vec::new();
        let mut plain = Vec::new();
        for id in &ids[..2] {
            positioned.push(
                CreateIndexBuilder::new(
                    &mut dataset,
                    &["text"],
                    IndexType::Inverted,
                    &with_position,
                )
                .name("text_idx".to_string())
                .fragments(vec![*id])
                .execute_uncommitted()
                .await
                .unwrap(),
            );
        }
        for id in &ids[2..4] {
            plain.push(
                CreateIndexBuilder::new(
                    &mut dataset,
                    &["text"],
                    IndexType::Inverted,
                    &without_position,
                )
                .name("text_idx".to_string())
                .fragments(vec![*id])
                .execute_uncommitted()
                .await
                .unwrap(),
            );
        }
        let position_uuids: Vec<_> = positioned.iter().map(|segment| segment.uuid).collect();
        let plain_uuids: Vec<_> = plain.iter().map(|segment| segment.uuid).collect();
        positioned.extend(plain);
        commit_mixed_segments(&mut dataset, positioned).await;
        assert_one_class_merges(&mut dataset, [position_uuids, plain_uuids]).await;

        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 8, 2, false, |id| format!("tok{id}")).await;
        let ids: Vec<u32> = dataset
            .get_fragments()
            .iter()
            .map(|fragment| fragment.id() as u32)
            .collect();
        let v1 = params().format_version(InvertedListFormatVersion::V1);
        let v2 = params().format_version(InvertedListFormatVersion::V2);
        let mut older = Vec::new();
        let mut newer = Vec::new();
        for id in &ids[..2] {
            older.push(
                CreateIndexBuilder::new(&mut dataset, &["text"], IndexType::Inverted, &v1)
                    .name("text_idx".to_string())
                    .fragments(vec![*id])
                    .execute_uncommitted()
                    .await
                    .unwrap(),
            );
        }
        for id in &ids[2..4] {
            newer.push(
                CreateIndexBuilder::new(&mut dataset, &["text"], IndexType::Inverted, &v2)
                    .name("text_idx".to_string())
                    .fragments(vec![*id])
                    .execute_uncommitted()
                    .await
                    .unwrap(),
            );
        }
        let older_uuids: Vec<_> = older.iter().map(|segment| segment.uuid).collect();
        let newer_uuids: Vec<_> = newer.iter().map(|segment| segment.uuid).collect();
        older.extend(newer);
        commit_mixed_segments(&mut dataset, older).await;
        assert_one_class_merges(&mut dataset, [older_uuids, newer_uuids]).await;

        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 8, 2, false, |id| format!("tok{id}")).await;
        let ids: Vec<u32> = dataset
            .get_fragments()
            .iter()
            .map(|fragment| fragment.id() as u32)
            .collect();
        let fst = params();
        let mut fst_segments = Vec::new();
        for id in &ids[..2] {
            fst_segments.push(
                CreateIndexBuilder::new(&mut dataset, &["text"], IndexType::Inverted, &fst)
                    .name("text_idx".to_string())
                    .fragments(vec![*id])
                    .execute_uncommitted()
                    .await
                    .unwrap(),
            );
        }
        let mut arrow_segments = Vec::new();
        for id in &ids[2..4] {
            arrow_segments.push(arrow_segment(&dataset, *id, &fst).await);
        }
        let fst_uuids: Vec<_> = fst_segments.iter().map(|segment| segment.uuid).collect();
        let arrow_uuids: Vec<_> = arrow_segments.iter().map(|segment| segment.uuid).collect();
        fst_segments.extend(arrow_segments);
        commit_mixed_segments(&mut dataset, fst_segments).await;
        assert_one_class_merges(&mut dataset, [fst_uuids, arrow_uuids]).await;
    }

    #[tokio::test]
    async fn non_tagged_bitmap_is_the_selected_coverage_and_plans_are_stable() {
        let dir = TempStrDir::default();
        let mut dataset = write_text(dir.as_str(), 6, 2, false, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        let before = text_segments(&dataset).await;
        assert_eq!(before.len(), 3);
        let options = tiered(2, 1, 1_000);
        let first = plan_optimize_indices(&dataset, &options).await.unwrap();
        let second = plan_optimize_indices(&dataset, &options).await.unwrap();
        let (selected, unindexed) = planned_group(&first);
        assert_eq!(planned_group(&second).0, selected);
        assert_eq!(planned_group(&second).1, unindexed);
        assert!(unindexed.is_empty());
        assert_eq!(selected.len(), 2);
        let mut ordered = before.clone();
        ordered.sort_by_key(|segment| segment.uuid);
        let expected: roaring::RoaringBitmap = ordered[..2]
            .iter()
            .filter_map(|segment| segment.fragment_bitmap.clone())
            .fold(roaring::RoaringBitmap::new(), |acc, bitmap| acc | bitmap);
        let kept = ordered[2].uuid;
        dataset.optimize_indices(&options).await.unwrap();
        let after = text_segments(&dataset).await;
        assert!(after.iter().any(|segment| segment.uuid == kept));
        let new_segment = after.iter().find(|segment| segment.uuid != kept).unwrap();
        assert_eq!(new_segment.fragment_bitmap.clone().unwrap(), expected);
        let kept_bitmap = after
            .iter()
            .find(|segment| segment.uuid == kept)
            .unwrap()
            .fragment_bitmap
            .clone()
            .unwrap();
        assert!(kept_bitmap.is_disjoint(&expected));
    }

    #[derive(Debug)]
    struct FailPuts {
        policy: Arc<Mutex<ProxyObjectStorePolicy>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FailPuts {
        fn new(marker: &'static str) -> Self {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let mut policy = ProxyObjectStorePolicy::new();
            policy.set_before_policy(
                "fail_puts",
                Arc::new(move |method, path| {
                    let call = format!("{method} {}", path.as_ref());
                    recorded.lock().unwrap().push(call);
                    if method.starts_with("put") && path.as_ref().contains(marker) {
                        Err(object_store::Error::Generic {
                            store: "fts-tiered-test",
                            source: format!("injected failure for {method} {}", path.as_ref())
                                .into(),
                        }
                        .into())
                    } else {
                        Ok(())
                    }
                }),
            );
            Self {
                policy: Arc::new(Mutex::new(policy)),
                calls,
            }
        }
    }

    impl WrappingObjectStore for FailPuts {
        fn wrap(
            &self,
            _storage_prefix: &str,
            original: Arc<dyn object_store::ObjectStore>,
        ) -> Arc<dyn object_store::ObjectStore> {
            Arc::new(ProxyObjectStore::new(original, self.policy.clone()))
        }

        fn wrap_paginated(
            &self,
            _store_prefix: &str,
            _original: Arc<dyn object_store::list::PaginatedListStore>,
        ) -> Option<Arc<dyn object_store::list::PaginatedListStore>> {
            None
        }
    }

    async fn assert_failure_keeps_the_dataset(marker: &'static str) {
        // Local files are renamed into place and never call object-store put.
        // An in-memory store does, so the wrapper can fail the merge write or
        // the manifest write.
        let uri = format!("memory://fts-tiered-fail-{}", Uuid::new_v4());
        let mut dataset = write_text(&uri, 4, 2, false, |id| format!("tok{id}")).await;
        let parameters = params();
        index_every_fragment(&mut dataset, &parameters).await;
        let version = dataset.version().version;
        let versions_before: BTreeSet<_> = dataset
            .object_store
            .read_dir(dataset.versions_dir())
            .await
            .unwrap()
            .into_iter()
            .collect();
        let uuids = text_uuids(&dataset).await;
        let hits = fts_hits(&dataset, "tok0").await;
        let wrapper = Arc::new(FailPuts::new(marker));
        let calls = wrapper.calls.clone();
        let mut wrapped =
            dataset.with_object_store_wrappers([wrapper as Arc<dyn WrappingObjectStore>]);
        let result = wrapped.optimize_indices(&tiered(2, 1, 1_000)).await;
        let error = match result {
            Ok(()) => panic!(
                "optimize succeeded; object-store calls were {:?}",
                calls.lock().unwrap()
            ),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("injected failure"),
            "optimize failed for a different reason: {error}; calls {:?}",
            calls.lock().unwrap()
        );
        // A fresh open of a memory URI builds an empty store. Read the store
        // this dataset already holds: a committed manifest would be a new file.
        let versions_after: BTreeSet<_> = dataset
            .object_store
            .read_dir(dataset.versions_dir())
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(versions_after, versions_before);
        assert_eq!(dataset.version().version, version);
        assert_eq!(text_uuids(&dataset).await, uuids);
        assert_eq!(fts_hits(&dataset, "tok0").await, hits);
    }

    #[tokio::test]
    async fn merge_and_commit_failures_leave_the_dataset_unchanged() {
        assert_failure_keeps_the_dataset("_indices/").await;
        assert_failure_keeps_the_dataset("_versions").await;
    }
}
