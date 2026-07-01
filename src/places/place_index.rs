use arrow::array::{
    Array, MapArray, RecordBatch, StringArray, UInt16Array, UInt32Array, UInt64Array,
};
use arrow::compute::concat_batches;
use lru::LruCache;
use once_cell::sync::OnceCell;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::statistics::Statistics;
use s2::cellid::CellID;
use std::fs::File;
use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{matchers::MatchMask, places::Place};

// TODO: Replace once_cell::sync::OnceCell by std::sync::OnceLock.
// However, this needs get_or_try_init to be stabilised on std::sync::OnceLock.
// https://github.com/rust-lang/rust/issues/109737

// ============================================================================
// Cache hit/miss counters
// ============================================================================

/// Snapshot returned by `PlaceIndex::cache_stats()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

impl CacheStats {
    /// Fraction of lookups served from cache. Returns `None` if no lookups
    /// have been made yet (avoids division by zero).
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            Some(self.hits as f64 / total as f64)
        }
    }
}

// ============================================================================
// Row-group metadata — built once from Parquet file metadata, no row I/O
// ============================================================================

#[derive(Debug, Clone)]
struct RowGroupInfo {
    index: usize,
    row_start: usize,
    row_count: usize,
    s2_min: u64,
    s2_max: u64,
}

// ============================================================================
// Cache design
//
// The LRU maps  rg_index → Arc<CacheSlot>
// where         CacheSlot = OnceCell<Arc<Vec<Place>>>
//
//  outer Arc     — lets multiple threads share the same slot without holding
//                  the LRU Mutex; cloning it is a single atomic increment.
//
//  OnceCell      — guarantees the row group is decoded exactly once, even
//                  when many threads request the same cold slot concurrently.
//                  The first thread to call get_or_try_init runs the decoder;
//                  every other thread blocks inside that call until decoding
//                  finishes, then reads the result without re-doing any work.
//                  On a warm hit (cell already set) get_or_try_init is a
//                  single atomic load — no locking, no blocking.
//
//  inner Arc     — lets iterators keep Vec<Place> alive even after the LRU
//                  evicts the slot. An iterator holds its own Arc clone, so
//                  eviction only drops the LRU's copy; the data is freed only
//                  when the last iterator referencing it is dropped.
//
// Thread protocol for a cache miss:
//   1. Lock LRU (briefly) → get-or-insert an Arc<CacheSlot> → unlock.
//      The LRU Mutex is never held during disk I/O.
//   2. Call slot.get_or_try_init(decode). One thread decodes; all racing
//      threads block here and share the result when it is ready.
//   3. Clone the inner Arc<Vec<Place>> from the now-initialised cell.
// ============================================================================

type CacheSlot = OnceCell<Arc<Vec<Place>>>;

// ============================================================================
// Index
// ============================================================================

pub struct PlaceIndex {
    file_path: Box<Path>,
    groups: Vec<RowGroupInfo>,
    cache: Mutex<LruCache<usize, Arc<CacheSlot>>>,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

impl PlaceIndex {
    /// Open the file and build the in-memory index from Parquet metadata.
    /// `cache_row_groups` is the maximum number of decoded groups kept in RAM.
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
            file_path: path.into(),
            groups,
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(cache_row_groups).expect("cache_row_groups must be > 0"),
            )),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }))
    }

    pub fn total_rows(&self) -> usize {
        self.groups
            .last()
            .map(|g| g.row_start + g.row_count)
            .unwrap_or(0)
    }

    /// Returns a snapshot of cumulative cache hit and miss counts.
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            hits: self.cache_hits.load(Ordering::Relaxed),
            misses: self.cache_misses.load(Ordering::Relaxed),
        }
    }

    // -----------------------------------------------------------------------
    // Query API
    // -----------------------------------------------------------------------

    /// Returns a sequential iterator over Places with `s2_cell_id ∈ range`
    /// and `place.mask.intersects(&query_mask)`.
    pub fn query(
        &self,
        range: RangeInclusive<CellID>,
        query_mask: MatchMask,
    ) -> anyhow::Result<PlaceIter> {
        let (segments, query_mask) = self.build_segments(range, query_mask)?;
        Ok(PlaceIter {
            segments,
            seg_idx: 0,
            row_idx: 0,
            query_mask,
        })
    }

    /// Iterates over row groups, yielding each as an Arc<Vec<Place>>.
    /// The caller processes each chunk, typically in parallel using Rayon.
    /// Rayon sees slices of places with high spatial locality because
    /// our Parquet files are spatially sorted (by s2 cell id).
    pub fn scan_row_groups(
        self: &Arc<Self>,
    ) -> impl Iterator<Item = anyhow::Result<Arc<Vec<Place>>>> + '_ {
        self.groups.iter().map(|group| self.load_row_group(group))
    }

    /// Builds segments on behalf of query().
    fn build_segments(
        &self,
        range: RangeInclusive<CellID>,
        query_mask: MatchMask,
    ) -> anyhow::Result<(Vec<Segment>, MatchMask)> {
        let lo = range.start().0;
        let hi = range.end().0;

        let mut segments: Vec<Segment> = Vec::new();
        for group in self.prune_row_groups(lo, hi) {
            let places = self.load_row_group(group)?;
            let start = places.partition_point(|p| p.s2_cell_id < lo);
            let end = places.partition_point(|p| p.s2_cell_id <= hi);
            if start < end {
                segments.push((places, start, end));
            }
        }
        Ok((segments, query_mask))
    }

    // -----------------------------------------------------------------------
    // Row-group pruning — metadata only, zero I/O
    // -----------------------------------------------------------------------

    fn prune_row_groups(&self, lo: u64, hi: u64) -> &[RowGroupInfo] {
        let first = self.groups.partition_point(|g| g.s2_max < lo);
        let last = self.groups.partition_point(|g| g.s2_min <= hi);
        &self.groups[first..last]
    }

    // -----------------------------------------------------------------------
    // Two-phase cache load — LRU Mutex never held during I/O
    // -----------------------------------------------------------------------

    fn load_row_group(&self, group: &RowGroupInfo) -> anyhow::Result<Arc<Vec<Place>>> {
        // Phase 1: hold the LRU lock only long enough to get-or-insert a slot.
        let slot: Arc<CacheSlot> = {
            let mut cache = self.cache.lock().unwrap();
            match cache.get(&group.index) {
                Some(slot) => Arc::clone(slot),
                None => {
                    let slot = Arc::new(OnceCell::new());
                    cache.put(group.index, Arc::clone(&slot));
                    slot
                }
            }
        }; // ← LRU lock released here

        // Phase 2: one thread runs the decoder; all others block until done.
        // On a warm hit this is a single atomic load — no blocking at all.
        let is_hit = slot.get().is_some();
        slot.get_or_try_init(|| self.decode_row_group_from_disk(group.index))?;

        if is_hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }

        Ok(Arc::clone(slot.get().unwrap()))
    }

    fn decode_row_group_from_disk(&self, rg_index: usize) -> anyhow::Result<Arc<Vec<Place>>> {
        let file = File::open(&self.file_path)?;
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(file)?.with_row_groups(vec![rg_index]);
        let reader = builder.build()?;

        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>()?;
        let schema = batches[0].schema();
        let batch = concat_batches(&schema, &batches)?;

        let mut places = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            places.push(extract_place(&batch, row)?);
        }
        Ok(Arc::new(places))
    }
}

// ============================================================================
// Sequential range-query iterator — yields anyhow::Result<&'iter Place>
// ============================================================================

type Segment = (Arc<Vec<Place>>, usize, usize); // (places, start, end)

pub struct PlaceIter {
    segments: Vec<Segment>,
    seg_idx: usize,
    row_idx: usize, // offset from `start` within the current segment
    query_mask: MatchMask,
}

impl<'iter> Iterator for &'iter mut PlaceIter {
    type Item = anyhow::Result<&'iter Place>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (places, start, end) = self
                .segments
                .get(self.seg_idx)
                .map(|(a, s, e)| (a.as_ref(), *s, *e))?;

            let pos = start + self.row_idx;
            if pos >= end {
                self.seg_idx += 1;
                self.row_idx = 0;
                continue;
            }

            self.row_idx += 1;

            // SAFETY: `places` is borrowed from the Arc inside `self.segments`,
            // which is owned by `self` and therefore lives for `'iter`.
            // `pos` is within [start, end) which was verified against
            // places.len() when the segment was constructed in `build_segments`.
            let place: &'iter Place = unsafe { &*(places.get_unchecked(pos) as *const Place) };

            if place.mask.intersects(&self.query_mask) {
                return Some(Ok(place));
            }
        }
    }
}

// ============================================================================
// Column extraction
// ============================================================================

// TODO: We call batch.column_by_name() for every single row; is this necessary?

fn extract_place(batch: &RecordBatch, row: usize) -> anyhow::Result<Place> {
    Ok(Place {
        s2_cell_id: get_u64_required(batch, "s2_cell_id", row)?,
        osm_id: get_u64_optional(batch, "osm_id", row)?.and_then(NonZeroU64::new),
        osm_changeset: get_u64_optional(batch, "osm_changeset", row)?.and_then(NonZeroU64::new),
        osm_version: get_u32_optional(batch, "osm_version", row)?.and_then(NonZeroU32::new),
        source: get_string_required(batch, "source", row)?,
        mask: MatchMask(get_u16_required(batch, "mask", row)?),
        tags: get_tags(batch, row)?,
    })
}

fn get_u32_optional(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<Option<u32>> {
    let col = match batch.column_by_name(name) {
        None => return Ok(None),
        Some(c) => c,
    };
    Ok(Some(
        col.as_any()
            .downcast_ref::<UInt32Array>()
            .ok_or_else(|| anyhow::anyhow!("column '{name}' exists but is not Int32"))?
            .value(row),
    ))
}

fn get_u64_required(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<u64> {
    Ok(batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing required column '{name}'"))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| anyhow::anyhow!("column '{name}' is not UInt64"))?
        .value(row))
}

fn get_u64_optional(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<Option<u64>> {
    let col = match batch.column_by_name(name) {
        None => return Ok(None),
        Some(c) => c,
    };
    Ok(Some(
        col.as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| anyhow::anyhow!("column '{name}' exists but is not UInt64"))?
            .value(row),
    ))
}

fn get_u16_required(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<u16> {
    Ok(batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing required column '{name}'"))?
        .as_any()
        .downcast_ref::<UInt16Array>()
        .ok_or_else(|| anyhow::anyhow!("column '{name}' is not UInt16"))?
        .value(row))
}

fn get_string_required(batch: &RecordBatch, name: &str, row: usize) -> anyhow::Result<String> {
    Ok(batch
        .column_by_name(name)
        .ok_or_else(|| anyhow::anyhow!("missing required column '{name}'"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("column '{name}' is not Utf8/String"))?
        .value(row)
        .to_owned())
}

fn get_tags(batch: &RecordBatch, row: usize) -> anyhow::Result<Vec<(String, String)>> {
    let col = batch
        .column_by_name("tags")
        .ok_or_else(|| anyhow::anyhow!("missing required column 'tags'"))?
        .as_any()
        .downcast_ref::<MapArray>()
        .ok_or_else(|| anyhow::anyhow!("column 'tags' is not a MapArray"))?;

    let map_entry = col.value(row);
    let keys = map_entry
        .column_by_name("key")
        .ok_or_else(|| anyhow::anyhow!("map 'tags' has no 'key' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("map 'tags' keys are not strings"))?;
    let values = map_entry
        .column_by_name("value")
        .ok_or_else(|| anyhow::anyhow!("map 'tags' has no 'value' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow::anyhow!("map 'tags' values are not strings"))?;

    let mut tags = Vec::with_capacity(keys.len());
    for i in 0..keys.len() {
        tags.push((keys.value(i).to_owned(), values.value(i).to_owned()));
    }
    Ok(tags)
}

// ============================================================================
// Parquet statistics helper
// ============================================================================

fn extract_u64_stats(stats: Option<&Statistics>) -> anyhow::Result<(u64, u64)> {
    match stats {
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Load a test index file with the following 7 places with `(s2_cell_id, mask)`:
    ///  - 5156616745313632255 MatchMask::SHOP
    ///  - 5156622227156738357 MatchMask::SHOP
    ///  - 5159605103934476889 MatchMask::SHOP
    ///  - 5159605115004699013 MatchMask::SHOP
    ///  - 5159605826311453673 MatchMask::STREET_FURNITURE
    ///  - 5159607645100837135 MatchMask::STREET_FURNITURE
    ///  - 5159714166772063677 MatchMask::SHOP
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
                .inspect(|p| check_place(p.as_ref().expect("iteration failed")))
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
        let stats = index.cache_stats();
        assert_eq!(stats, CacheStats { hits: 4, misses: 1 });
        assert_eq!(stats.hit_rate(), Some(0.8));
    }

    #[test]
    fn test_scan_row_groups() {
        let index = load_test_index();
        let mut place_count = 0;
        for group in index.scan_row_groups() {
            for p in group.expect("scan_row_groups failed").iter() {
                check_place(p);
                place_count += 1;
            }
        }
        assert_eq!(place_count, 7);
    }

    fn check_place(place: &Place) {
        assert_ne!(place.s2_cell_id, 0, "s2_cell_id should not be zero");
        assert!(!place.source.is_empty(), "source should not be empty");
        for (k, v) in &place.tags {
            assert!(!k.is_empty(), "tag key should not be empty");
            assert!(!v.is_empty(), "tag value should not be empty");
        }
    }
}
