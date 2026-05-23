use std::{fs::File, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use parquet::{
    file::{
        reader::{FileReader, SerializedFileReader},
        statistics::Statistics,
    },
    schema::types::SchemaDescriptor,
};

/// A lightweight index built from Parquet row-group statistics.
///
/// Construction reads only the file footer (no data pages are decoded),
/// so it is very cheap even for large files.
///
/// # Assumptions
/// * The file is sorted by `s2_cell_id` in ascending order.
/// * Every row group has min/max statistics for `s2_cell_id`.
/// * `s2_cell_id` is a `UInt64` column (physical type `INT64` in Parquet).
pub struct S2RowGroupIndex {
    /// `ranges[i]` is the `(min_s2, max_s2)` for row group `i`.
    /// Guaranteed to be non-overlapping and sorted because the file is sorted.
    ranges: Vec<(u64, u64)>,
}

impl S2RowGroupIndex {
    /// Build the index by reading only the Parquet footer of `path`.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path)?;
        let reader = SerializedFileReader::new(file)?;
        let metadata = reader.metadata();

        // Locate the column index once, up front.
        let schema: &SchemaDescriptor = metadata.file_metadata().schema_descr();
        let col_index = schema
            .columns()
            .iter()
            .position(|cd| cd.name() == "s2_cell_id")
            .ok_or_else(|| anyhow!("Column 's2_cell_id' not found in schema"))?;

        let num_rgs = metadata.num_row_groups();
        let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(num_rgs);

        for rg in 0..num_rgs {
            let col_chunk = metadata.row_group(rg).column(col_index);

            let stats = col_chunk.statistics().with_context(|| {
                format!("Row group {rg} is missing statistics for 's2_cell_id'")
            })?;

            // UInt64 is stored as physical INT64; interpret the bits as u64.
            let (min_s2, max_s2) = extract_u64_stats(stats, rg)?;

            ranges.push((min_s2, max_s2));
        }

        Ok(Self { ranges })
    }

    /// Returns the **inclusive** row-group range `(min_rg, max_rg)` that
    /// *may* contain S2 cell IDs in `[min_s2, max_s2]`.
    ///
    /// Returns `None` if the query range does not overlap any row group.
    ///
    /// Because the file is sorted the matching row groups are always
    /// contiguous, so the result is a single closed interval.
    ///
    /// # Complexity
    /// O(log n) — two binary searches over the row-group count.
    pub fn query(&self, min_s2: u64, max_s2: u64) -> Option<(usize, usize)> {
        if self.ranges.is_empty() || min_s2 > max_s2 {
            return None;
        }

        // First row group whose max_s2 >= min_s2 (i.e. could reach into the
        // query range from below).
        //
        // `partition_point` returns the first index where the predicate is
        // false, so we negate: "rg_max < min_s2" is false ⟹ rg_max >= min_s2.
        let first = self.ranges.partition_point(|&(_, rg_max)| rg_max < min_s2);

        if first >= self.ranges.len() {
            return None; // every row group ends before the query starts
        }

        // First row group whose min_s2 > max_s2; the one before it is the
        // last overlapping row group.
        let after_last = self.ranges.partition_point(|&(rg_min, _)| rg_min <= max_s2);

        if after_last == 0 || after_last <= first {
            return None; // every row group starts after the query ends
        }

        Some((first, after_last - 1))
    }
}

/// Extract `(min, max)` as `u64` from row-group statistics.
///
/// Parquet physical type for `UInt64` is `INT64`.  The raw bytes are the
/// two's-complement bit pattern of the unsigned value, so reinterpreting
/// via `i64 as u64` (a no-op bitcast) recovers the original unsigned number.
fn extract_u64_stats(stats: &Statistics, rg: usize) -> Result<(u64, u64)> {
    match stats {
        Statistics::Int64(s) => {
            // `min_opt` / `max_opt` return `Option<&i64>`.
            let min = s.min_opt().copied().with_context(|| {
                format!("Row group {rg} is missing min statistic for 's2_cell_id'")
            })? as u64;
            let max = s.max_opt().copied().with_context(|| {
                format!("Row group {rg} is missing max statistic for 's2_cell_id'")
            })? as u64;
            Ok((min, max))
        }
        // Guard against files written with INT32 physical type (shouldn't
        // happen for UInt64, but makes the error message explicit).
        _ => bail!(
            "Row group {rg} has unexpected statistics type for 's2_cell_id' \
             (expected Int64 / UInt64 physical storage)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a synthetic index directly to test query logic without needing a
    /// real Parquet file on disk.
    fn make_index(ranges: Vec<(u64, u64)>) -> S2RowGroupIndex {
        S2RowGroupIndex { ranges }
    }

    //  Row groups: [0..10], [20..30], [40..50]
    fn sample_index() -> S2RowGroupIndex {
        make_index(vec![(0, 10), (20, 30), (40, 50)])
    }

    fn test_data_path(filename: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests");
        path.push("test_data");
        path.push("places");
        path.push(filename);
        path
    }

    #[test]
    fn load_from_path() {
        let path = test_data_path("multi_row_groups.parquet");
        let idx = S2RowGroupIndex::from_path(path).expect("cannot load test data");
        assert_eq!(idx.query(20, 50), Some((1, 2)));
    }

    #[test]
    fn exact_single_rg() {
        let idx = sample_index();
        assert_eq!(idx.query(5, 8), Some((0, 0)));
    }

    #[test]
    fn spans_all_rgs() {
        let idx = sample_index();
        assert_eq!(idx.query(0, 50), Some((0, 2)));
    }

    #[test]
    fn spans_middle_two() {
        let idx = sample_index();
        assert_eq!(idx.query(15, 35), Some((1, 1)));
        assert_eq!(idx.query(10, 40), Some((0, 2)));
    }

    #[test]
    fn gap_between_rgs_returns_adjacent() {
        // Query falls entirely in the gap [11..19] — no row group covers it,
        // but the index is conservative: it returns None only when there is
        // truly no overlap.
        let idx = sample_index();
        assert_eq!(idx.query(11, 19), None);
    }

    #[test]
    fn before_all_rgs() {
        let idx = sample_index();
        // max_s2 < first rg min
        assert_eq!(idx.query(0, 0), Some((0, 0)));
    }

    #[test]
    fn after_all_rgs() {
        let idx = sample_index();
        assert_eq!(idx.query(51, 100), None);
    }

    #[test]
    fn inverted_range_returns_none() {
        let idx = sample_index();
        assert_eq!(idx.query(30, 10), None);
    }

    #[test]
    fn empty_index() {
        let idx = make_index(vec![]);
        assert_eq!(idx.query(0, 100), None);
    }

    #[test]
    fn boundary_exact_min() {
        // Query exactly on the boundary value of a row group.
        let idx = sample_index();
        assert_eq!(idx.query(30, 30), Some((1, 1)));
        assert_eq!(idx.query(31, 31), None); // gap
        assert_eq!(idx.query(40, 40), Some((2, 2)));
    }
}
