use arrow::array::{Array, MapArray, RecordBatch, StringArray, UInt16Array, UInt64Array};
use arrow::compute::concat_batches;
use lru::LruCache;

// TODO: Replace OnceCell by OnceLock when https://github.com/rust-lang/rust/issues/109737 is done.
use once_cell::sync::OnceCell;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::statistics::Statistics;
use rayon::prelude::*;
use s2::cellid::CellID;
use std::fs::File;
use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{MatchMask, places::Place};

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
// Cache design  (see earlier comments for full explanation)
//
// LRU maps  rg_index → Arc<OnceCell<Arc<Vec<Place>>>>
// ============================================================================

// TODO: Replace OnceCell by OnceLock after the try_ methods are stable.
// https://github.com/rust-lang/rust/issues/109737
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
    // Sequential API
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

    /// Returns a sequential iterator over every Place in the file.
    #[allow(unused)] // TODO: Remove attribute once we use it.
    pub fn scan(self: &Arc<Self>) -> FullScanIter {
        FullScanIter {
            index: Arc::clone(self),
            group_idx: 0,
            current: None,
        }
    }

    // -----------------------------------------------------------------------
    // Parallel API
    // -----------------------------------------------------------------------

    /// Parallel version of `query`. Each segment (row-group slice) is
    /// processed independently by Rayon.
    #[allow(unused)] // TODO: Remove attribute once we use it.
    pub fn query_par(
        &self,
        range: RangeInclusive<CellID>,
        query_mask: MatchMask,
    ) -> anyhow::Result<ParPlaceIter> {
        let (segments, query_mask) = self.build_segments(range, query_mask)?;
        Ok(ParPlaceIter {
            segments,
            query_mask,
        })
    }

    /// Parallel version of `scan`. Row groups are loaded concurrently
    /// (each decoded once via the shared cache) and iterated in parallel.
    #[allow(unused)] // TODO: Remove attribute once we use it.
    pub fn scan_par(self: &Arc<Self>) -> ParPlaceIter {
        let segments: Vec<Segment> = self
            .groups
            .iter()
            .filter_map(|group| {
                let places = self.load_row_group(group).ok()?;
                let end = places.len();
                Some((places, 0, end))
            })
            .collect();
        ParPlaceIter {
            segments,
            query_mask: MatchMask(0xFFFF),
        }
    }

    // -----------------------------------------------------------------------
    // Shared segment-building logic used by both query() and query_par()
    // -----------------------------------------------------------------------

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

            let abs = start + self.row_idx;
            if abs >= end {
                self.seg_idx += 1;
                self.row_idx = 0;
                continue;
            }

            self.row_idx += 1;

            // SAFETY: `places` is borrowed from the Arc inside `self.segments`,
            // which is owned by `self` and therefore lives for `'iter`.
            // `abs` is within [start, end) which was verified against
            // places.len() when the segment was constructed in `build_segments`.
            let place: &'iter Place = unsafe { &*(places.get_unchecked(abs) as *const Place) };

            if place.mask.intersects(&self.query_mask) {
                return Some(Ok(place));
            }
        }
    }
}

// ============================================================================
// Parallel iterator — owns the segments, yields &Place refs
// ============================================================================
//
// `ParPlaceIter` owns the `Vec<Segment>` (each element holds an `Arc<Vec<Place>>`),
// so the Places are guaranteed live for `'self`. The `par_iter` method exposes
// them as a `ParallelIterator<Item = &Place>` using the same raw-pointer lifetime
// extension as the sequential `PlaceIter`.

#[allow(unused)] // TODO: Remove attribute once we use it.
pub struct ParPlaceIter {
    segments: Vec<Segment>,
    query_mask: MatchMask,
}

impl ParPlaceIter {
    /// Returns a Rayon `ParallelIterator` over matching `&Place` references.
    /// Splits across segments (row-group slices); Rayon further subdivides
    /// within each slice as it sees fit.
    #[allow(unused)] // TODO: Remove attribute once we use it.
    pub fn par_iter(&self) -> impl ParallelIterator<Item = &Place> {
        let query_mask = self.query_mask;
        self.segments
            .par_iter()
            .flat_map(|(places, start, end)| {
                // SAFETY: `places` is an Arc<Vec<Place>> stored in `self.segments`.
                // `self` lives for the duration of the returned iterator (the caller
                // holds `&self`). The slice [*start..*end] was bounds-checked in
                // `build_segments`. No mutable access to the data exists.
                let slice: &[Place] =
                    unsafe { std::slice::from_raw_parts(places.as_ptr().add(*start), end - start) };
                slice.par_iter()
            })
            .filter(move |p| p.mask.intersects(&query_mask))
    }
}

// ============================================================================
// Sequential full-scan iterator — yields anyhow::Result<&'iter Place>
// ============================================================================

pub struct FullScanIter {
    index: Arc<PlaceIndex>,
    group_idx: usize,
    current: Option<(Arc<Vec<Place>>, usize)>,
}

impl FullScanIter {
    fn advance_group(&mut self) -> Option<()> {
        if self.group_idx >= self.index.groups.len() {
            return None;
        }
        let group = &self.index.groups[self.group_idx];
        let places = self.index.load_row_group(group).ok()?;
        self.current = Some((places, 0));
        self.group_idx += 1;
        Some(())
    }
}

impl<'iter> Iterator for &'iter mut FullScanIter {
    type Item = anyhow::Result<&'iter Place>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match &mut self.current {
                Some((places, idx)) => {
                    if *idx < places.len() {
                        let i = *idx;
                        *idx += 1;
                        // SAFETY: same argument as in PlaceIter.
                        let place: &'iter Place =
                            unsafe { &*(places.get_unchecked(i) as *const Place) };
                        return Some(Ok(place));
                    }
                    self.current = None;
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
        s2_cell_id: get_u64_required(batch, "s2_cell_id", row)?,
        osm_id: get_u64_optional(batch, "osm_id", row)?.unwrap_or(0),
        source: get_string_required(batch, "source", row)?,
        mask: MatchMask(get_u16_required(batch, "mask", row)?),
        tags: get_tags(batch, row)?,
    })
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
    fn test_query_par() {
        let index = load_test_index();
        let count = |lo, hi, mask| {
            let results = index
                .query_par(CellID(lo)..=CellID(hi), mask)
                .expect("query_par failed");
            results
                .par_iter()
                .inspect(|place| check_place(place))
                .count()
        };
        assert_eq!(count(7, 42, MatchMask::STREET_FURNITURE), 0);
        assert_eq!(count(0, 5159714166772063677, MatchMask::SHOP), 5);
        let stats = index.cache_stats();
        assert_eq!(stats, CacheStats { hits: 0, misses: 1 });
        assert_eq!(stats.hit_rate(), Some(0.0));
    }

    #[test]
    fn test_scan() {
        let index = load_test_index();
        let num_places = index
            .scan()
            .inspect(|p| check_place(p.as_ref().expect("scan failed")))
            .count();
        assert_eq!(num_places, 7);
        assert_eq!(index.cache_stats(), CacheStats { hits: 0, misses: 1 });
        assert_eq!(index.cache_stats().hit_rate(), Some(0.0));
    }

    #[test]
    fn test_scan_par() {
        let index = load_test_index();
        let num_places = index
            .scan_par()
            .par_iter()
            .inspect(|p| {
                check_place(&p);
            })
            .count();
        assert_eq!(num_places, 7);
        assert_eq!(index.cache_stats(), CacheStats { hits: 0, misses: 1 });
        assert_eq!(index.cache_stats().hit_rate(), Some(0.0));
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
