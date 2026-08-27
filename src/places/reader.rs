//! Sequentially reads a `Place`-typed Parquet file (`alltheplaces.parquet`
//! today -- the only caller is `conflate()`, reading ATP).
//!
//! No caching, no spatial index: `conflate()` scans every ATP feature
//! exactly once, in file order, so there's nothing worth caching --
//! there's no repeated access to a row that a cache would ever serve a
//! hit for.

use anyhow::{Context, Result};
use arrow::array::{Array, Int64Array, RecordBatch};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::{
    matchers::MatchMask,
    places::Place,
    utils::{
        UtcTimestamp,
        parquet::{get_binary, get_string, get_tags, get_u16, get_u64},
    },
};

pub struct PlaceReader {
    file_path: PathBuf,
    total_rows: usize,
}

impl PlaceReader {
    /// Opens the file and reads its total row count from Parquet
    /// metadata -- cheap, no row I/O.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let total_rows = ParquetRecordBatchReaderBuilder::try_new(file)?
            .metadata()
            .file_metadata()
            .num_rows() as usize;
        Ok(Self {
            file_path: path.to_path_buf(),
            total_rows,
        })
    }

    pub fn total_rows(&self) -> usize {
        self.total_rows
    }

    /// Reads every row, batch by batch (Arrow's natural row-group-ish
    /// chunking). The caller processes each batch in parallel with
    /// Rayon -- exact order within a batch doesn't matter for that. But
    /// the *rough* spatial ordering across batches (the file is
    /// S2-sorted) matters a lot: for each ATP feature, the caller
    /// queries the mmap'd `OsmFeatureIndex` for nearby OSM candidates,
    /// so processing ATP features in roughly spatial order keeps those
    /// queries clustered in nearby regions of the mmap, which is what
    /// keeps the OS page cache effective. Without it, a full-planet run
    /// on a memory-constrained machine would thrash instead of relying
    /// on the page cache the way this pipeline's design depends on.
    pub fn read_all(&self) -> Result<impl Iterator<Item = Result<Vec<Place>>>> {
        let file = File::open(&self.file_path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
        Ok(reader.map(|batch| {
            let batch = batch?;
            (0..batch.num_rows())
                .map(|row| extract_place(&batch, row))
                .collect()
        }))
    }
}

fn extract_place(batch: &RecordBatch, row: usize) -> Result<Place> {
    Ok(Place {
        s2_cell_id: get_u64(batch, "s2_cell_id", row)?,
        spider: get_string(batch, "spider", row)?,
        mask: MatchMask(get_u16(batch, "mask", row)?),
        tags: get_tags(batch, "tags", row)?,
        shape_wkb: get_binary(batch, "shape", row)?,
        fetched: get_fetched(batch, row)?,
    })
}

fn get_fetched(batch: &RecordBatch, row: usize) -> Result<UtcTimestamp> {
    let millis = batch
        .column_by_name("fetched")
        .context("missing column 'fetched'")?
        .as_any()
        .downcast_ref::<Int64Array>()
        .context("column 'fetched' is not Int64")?
        .value(row);
    let t = time::UtcDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .with_context(|| format!("invalid 'fetched' timestamp: {millis} ms"))?;
    Ok(UtcTimestamp(t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Test file with 10 places -- produced by running the real
    /// `import_atp` pipeline stage against `tests/test_data/
    /// alltheplaces.zip` (see `tests/integration_test.rs`'s own use of
    /// that same zip), not hand-crafted; this only needs the total count
    /// and that every place decodes cleanly.
    fn test_file() -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/test_data/alltheplaces.parquet");
        path
    }

    #[test]
    fn total_rows_matches_metadata() {
        let reader = PlaceReader::open(&test_file()).expect("open");
        assert_eq!(reader.total_rows(), 10);
    }

    #[test]
    fn read_all_yields_every_row_exactly_once() {
        let reader = PlaceReader::open(&test_file()).expect("open");
        let mut count = 0;
        for batch in reader.read_all().expect("read_all") {
            for place in batch.expect("batch decode") {
                assert_ne!(place.s2_cell_id, 0, "s2_cell_id should not be zero");
                assert!(!place.spider.is_empty(), "spider should not be empty");
                assert!(
                    place.fetched.unix_timestamp_millis() > 0,
                    "fetched should be a real, positive timestamp"
                );
                // Every place's shape should decode cleanly -- exact
                // values (and non-point geometry) are covered by
                // pipeline::atp's own unit tests and
                // tests/integration_test.rs, not needed again here.
                place.shape();
                for (k, v) in &place.tags {
                    assert!(!k.is_empty(), "tag key should not be empty");
                    assert!(!v.is_empty(), "tag value should not be empty");
                }
                count += 1;
            }
        }
        assert_eq!(count, reader.total_rows());
    }
}
