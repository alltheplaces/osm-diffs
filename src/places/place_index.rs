use crate::{MatchMask, places::Place};
use anyhow::Context;
use arrow::array::{Array, MapArray, RecordBatch, StringArray, UInt16Array, UInt64Array};
use arrow::compute::concat_batches;
use lru::LruCache;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::statistics::Statistics;
use s2::cellid::CellID;
use std::fs::File;
use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ============================================================================
// Row-group metadata — built once from Parquet file metadata, no row I/O
// ============================================================================

#[derive(Debug, Clone)]
struct RowGroupInfo {
    index: usize,
    row_start: usize, // absolute index of the first row in this group
    row_count: usize,
    s2_min: u64,
    s2_max: u64,
}

// ============================================================================
// Index
// ============================================================================

pub struct PlaceIndex {
    file_path: PathBuf,
    /// One entry per row group, sorted by row_start (== sorted by s2_cell_id).
    groups: Vec<RowGroupInfo>,
    /// LRU cache: row_group_index → decoded RecordBatch (all columns).
    cache: Mutex<LruCache<usize, Arc<RecordBatch>>>,
}

impl PlaceIndex {
    /// Open the file and build the in-memory index from Parquet metadata.
    /// `cache_row_groups` controls how many decoded row groups are kept in RAM.
    pub fn open(path: &Path, cache_row_groups: usize) -> anyhow::Result<Arc<Self>> {
        let file = File::open(path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let metadata = builder.metadata().clone();

        let mut groups = Vec::with_capacity(metadata.num_row_groups());
        let mut row_start = 0usize;

        for i in 0..metadata.num_row_groups() {
            let rg = metadata.row_group(i);
            let row_count = rg.num_rows() as usize;

            let schema = rg.schema_descr();
            let col_idx = (0..schema.num_columns())
                .find(|&c| schema.column(c).name() == "s2_cell_id")
                .ok_or_else(|| anyhow::anyhow!("Column s2_cell_id not found"))?;

            let (s2_min, s2_max) = extract_u64_stats(rg.column(col_idx).statistics())?;

            groups.push(RowGroupInfo {
                index: i,
                row_start,
                row_count,
                s2_min,
                s2_max,
            });
            row_start += row_count;
        }

        Ok(Arc::new(Self {
            file_path: PathBuf::from(path),
            groups,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(cache_row_groups).expect("cache_row_groups must be > 0"),
            )),
        }))
    }

    /// Total number of rows across all row groups.
    pub fn total_rows(&self) -> usize {
        self.groups
            .last()
            .map(|g| g.row_start + g.row_count)
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Public query API
    // -----------------------------------------------------------------------

    /// Returns an iterator over all Places whose `s2_cell_id` falls within
    /// `range` AND whose `(place.mask & query_mask) != 0`.
    ///
    /// Candidate row groups are identified from metadata alone (zero I/O).
    /// Places are decoded lazily as the iterator is advanced.
    pub fn query(
        &self,
        range: RangeInclusive<CellID>,
        query_mask: MatchMask,
    ) -> anyhow::Result<PlaceIter> {
        let lo = *range.start();
        let hi = *range.end();

        let mut batches = Vec::new();
        for group in self.prune_row_groups(lo, hi) {
            let batch = self.load_row_group(group)?;
            if let Some(sliced) = slice_by_s2_range(&batch, lo, hi)? {
                // RecordBatch::slice is zero-copy: shifts offset+length on Arc buffers.
                batches.push(Arc::new(sliced));
            }
        }

        Ok(PlaceIter {
            batches,
            batch_idx: 0,
            row_idx: 0,
            query_mask,
        })
    }

    /// Returns an iterator over every row in the file in storage order.
    ///
    /// Row groups are loaded one at a time and bypass the LRU cache so that
    /// a sequential scan does not evict row groups that concurrent range
    /// queries are actively using.
    pub fn scan(self: &Arc<Self>) -> FullScanIter {
        FullScanIter {
            index: Arc::clone(self),
            group_idx: 0,
            current: None,
        }
    }

    // -----------------------------------------------------------------------
    // Row-group pruning (metadata only, zero I/O)
    // -----------------------------------------------------------------------

    fn prune_row_groups(&self, lo: CellID, hi: CellID) -> &[RowGroupInfo] {
        // First group whose s2_max >= lo
        let first = self.groups.partition_point(|g| g.s2_max < lo.0);
        // First group whose s2_min > hi (one past the last candidate)
        let last = self.groups.partition_point(|g| g.s2_min <= hi.0);
        &self.groups[first..last]
    }

    // -----------------------------------------------------------------------
    // Row-group loading with LRU cache
    // -----------------------------------------------------------------------

    fn load_row_group(&self, group: &RowGroupInfo) -> anyhow::Result<Arc<RecordBatch>> {
        // Fast path — already cached.
        {
            let mut cache = self.cache.lock().unwrap();
            if let Some(batch) = cache.get(&group.index) {
                return Ok(Arc::clone(batch));
            }
        } // lock released before the slow disk read

        // Slow path — read from disk, then insert into cache.
        // Two threads may race here and both read the same group; that's
        // acceptable — put() is idempotent and simply refreshes the LRU position.
        let batch = Arc::new(self.read_row_group_from_disk(group.index)?);
        {
            let mut cache = self.cache.lock().unwrap();
            cache.put(group.index, Arc::clone(&batch));
        }
        Ok(batch)
    }

    pub(crate) fn read_row_group_from_disk(&self, rg_index: usize) -> anyhow::Result<RecordBatch> {
        let file = File::open(&self.file_path)?;
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(file)?.with_row_groups(vec![rg_index]);
        let reader = builder.build()?;

        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>()?;
        let schema = batches[0].schema();
        Ok(concat_batches(&schema, &batches)?)
    }
}

// ============================================================================
// Range-query iterator
// ============================================================================

pub struct PlaceIter {
    /// Batches already sliced to the queried s2_cell_id range.
    batches: Vec<Arc<RecordBatch>>,
    batch_idx: usize,
    row_idx: usize,
    /// Only yield Places where place.mask.intersects(query_mask).
    query_mask: MatchMask,
}

impl Iterator for PlaceIter {
    type Item = anyhow::Result<Place>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let batch = self.batches.get(self.batch_idx)?;

            if self.row_idx >= batch.num_rows() {
                self.batch_idx += 1;
                self.row_idx = 0;
                continue;
            }

            let row = self.row_idx;
            self.row_idx += 1;
            let batch = Arc::clone(batch);

            match extract_place(&batch, row) {
                Err(e) => return Some(Err(e)),
                Ok(place) if place.mask.intersects(&self.query_mask) => return Some(Ok(place)),
                Ok(_) => continue, // mask filtered — try the next row
            }
        }
    }
}

// ============================================================================
// Full-scan iterator
// ============================================================================

pub struct FullScanIter {
    index: Arc<PlaceIndex>,
    group_idx: usize,
    /// Active batch and our cursor within it.
    current: Option<(Arc<RecordBatch>, usize)>,
}

impl FullScanIter {
    /// Load the next row group directly from disk, bypassing the LRU cache.
    fn advance_group(&mut self) -> Option<()> {
        if self.group_idx >= self.index.groups.len() {
            return None;
        }
        let rg_index = self.index.groups[self.group_idx].index;
        let batch = self.index.read_row_group_from_disk(rg_index).ok()?;
        self.current = Some((Arc::new(batch), 0));
        self.group_idx += 1;
        Some(())
    }
}

impl Iterator for FullScanIter {
    type Item = anyhow::Result<Place>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.current {
                Some((batch, row_idx)) => {
                    if *row_idx < batch.num_rows() {
                        let row = *row_idx;
                        *row_idx += 1;
                        let batch = Arc::clone(batch);
                        return Some(extract_place(&batch, row));
                    }
                    self.current = None; // batch exhausted; fall through
                }
                None => {
                    self.advance_group()?;
                }
            }
        }
    }
}

// ============================================================================
// Column extraction
// ============================================================================

fn extract_place(batch: &RecordBatch, row: usize) -> anyhow::Result<Place> {
    Ok(Place {
        s2_cell_id: get_opt_u64(batch, "s2_cell_id", row)?.context("missing s2_cell_id")?,
        osm_id: get_opt_u64(batch, "osm_id", row)?.unwrap_or(0),
        source: get_string(batch, "source", row)?,
        mask: MatchMask(get_u16(batch, "mask", row)?),
        tags: get_tags(batch, row)?,
    })
}

fn get_opt_u64(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<Option<u64>> {
    if let Some(col) = batch.column_by_name(name) {
        Ok(Some(
            col.as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| anyhow::anyhow!("{name} is not UInt64"))?
                .value(row),
        ))
    } else {
        Ok(None)
    }
}

fn get_u16(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<u16> {
    Ok(batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<UInt16Array>()
        .ok_or_else(|| anyhow::anyhow!("{name} is not UInt16"))?
        .value(row))
}

fn get_string(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<String> {
    Ok(batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing column {name}"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("{name} is not String"))?
        .value(row)
        .to_string())
}

fn get_tags(batch: &RecordBatch, row: usize) -> anyhow::Result<Vec<(String, String)>> {
    let col = batch
        .column_by_name("tags")
        .ok_or_else(|| anyhow::anyhow!("missing column tags"))?
        .as_any()
        .downcast_ref::<MapArray>()
        .ok_or_else(|| anyhow::anyhow!("tags is not MapArray"))?;

    // col.value(row) returns a StructArray covering just this row's entries.
    let map_entry = col.value(row);
    let keys = map_entry
        .column_by_name("key")
        .ok_or_else(|| anyhow::anyhow!("map 'tags' has no 'key' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("keys of map 'tags' are not strings"))?;
    let values = map_entry
        .column_by_name("value")
        .ok_or_else(|| anyhow::anyhow!("map 'tags' has no 'value' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("values of map 'tags' are not strings"))?;

    let tags = (0..keys.len())
        .map(|i| (keys.value(i).to_owned(), values.value(i).to_owned()))
        .collect();
    Ok(tags)
}

// ============================================================================
// Binary search within a RecordBatch on the sorted s2_cell_id column
// ============================================================================

fn slice_by_s2_range(
    batch: &RecordBatch,
    lo: CellID,
    hi: CellID,
) -> anyhow::Result<Option<RecordBatch>> {
    let ids = batch
        .column_by_name("s2_cell_id")
        .ok_or_else(|| anyhow::anyhow!("s2_cell_id column missing"))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| anyhow::anyhow!("s2_cell_id is not UInt64"))?;

    let values = ids.values(); // &[u64] — zero-copy view into the Arrow buffer

    let start = values.partition_point(|&v| v < lo.0);
    let end = values.partition_point(|&v| v <= hi.0);

    if start >= end {
        return Ok(None);
    }

    // RecordBatch::slice is zero-copy: adjusts offset + length on Arc buffers.
    Ok(Some(batch.slice(start, end - start)))
}

// ============================================================================
// Parquet statistics helper
// ============================================================================

fn extract_u64_stats(stats: Option<&Statistics>) -> anyhow::Result<(u64, u64)> {
    match stats {
        // u64 has no native Parquet physical type; it is stored as INT64 bits.
        // The bit-cast `as u64` is correct: S2 cell IDs are always positive,
        // so the signed/unsigned re-interpretation preserves sort order.
        Some(Statistics::Int64(s)) => {
            let min = s
                .min_opt()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing min stat"))? as u64;
            let max = s
                .max_opt()
                .copied()
                .ok_or_else(|| anyhow::anyhow!("missing max stat"))? as u64;
            Ok((min, max))
        }
        _ => anyhow::bail!(
            "s2_cell_id has no usable Int64 statistics. \
             Re-write the Parquet file with write_statistics=True."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load a test index file with the following `(s2_cell_id, mask)`:
    /// * 5156622227156738357 MatchMask::SHOP
    /// * 5159605103934476889 MatchMask::SHOP
    /// * 5159605115004699013 MatchMask::SHOP
    /// * 5159605826311453673 MatchMask::STREET_FURNITURE
    /// * 5159607645100837135 MatchMask::STREET_FURNITURE
    /// * 5159714166772063677 MatchMask::SHOP
    fn load_test_index() -> Arc<PlaceIndex> {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/test_data/alltheplaces.parquet");
        PlaceIndex::open(&path, 8).expect("cannot open test data file")
    }

    #[test]
    fn test_query() {
        let index = load_test_index();
        let count = |lo, hi, mask| {
            index
                .query(CellID(lo)..=CellID(hi), mask)
                .expect("query failed")
                .count()
        };
        assert_eq!(count(7, 42, MatchMask::STREET_FURNITURE), 0);
        assert_eq!(
            count(0, 5159714166772063677, MatchMask::STREET_FURNITURE),
            2
        );
        assert_eq!(count(0, 5159714166772063677, MatchMask::SHOP), 5);
        assert_eq!(
            count(5159605115004699012, 5159605115004699015, MatchMask::SHOP),
            1
        );
        assert_eq!(
            count(5159605115004699012, 5159605115004699015, MatchMask::FUEL),
            0
        );
        assert_eq!(
            count(5159605115004699013, 5159714166772063677, MatchMask::SHOP),
            2
        );
    }

    #[test]
    fn test_scan() {
        let index = load_test_index();
        let places: Vec<Place> = index.scan().filter_map(|r| r.ok()).collect::<Vec<_>>();
        assert!(!places.is_empty(), "expected at least one row");
        for place in &places {
            assert_ne!(place.s2_cell_id, 0, "s2_cell_id should not be zero");
            assert!(!place.source.is_empty(), "source should not be empty");
            for (k, v) in &place.tags {
                assert!(!k.is_empty(), "tag key should not be empty");
                assert!(!v.is_empty(), "tag value should not be empty");
            }
        }
    }
}
