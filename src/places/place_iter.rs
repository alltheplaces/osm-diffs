use anyhow::{Context, Result, anyhow};
use arrow::array::{Array, MapArray, StringArray, UInt16Array, UInt64Array};
use arrow::record_batch::RecordBatch;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::fs::File;
use std::path::Path;

use super::{MatchMask, Place};

/// Lazily yields one [`Place`] per Parquet row.
///
/// Internally the file is read in Arrow record-batches;
/// rows are then decoded one at a time by [`Iterator::next`].
pub struct PlaceIter {
    reader: parquet::arrow::arrow_reader::ParquetRecordBatchReader,
    /// The batch currently being drained.
    current_batch: Option<RecordBatch>,
    /// Row index within `current_batch`.
    batch_offset: usize,
    has_osm_id: bool,
}

impl PlaceIter {
    /// Open a Parquet file for sequential scanning.
    pub fn try_new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())
            .with_context(|| format!("failed to open '{}'", path.as_ref().display()))?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)
            .context("failed to build Parquet reader")?;
        let has_osm_id = builder.schema().column_with_name("osm_id").is_some();
        let reader = builder
            .build()
            .context("failed to start reading Parquet batches")?;
        Ok(Self {
            reader,
            current_batch: None,
            batch_offset: 0,
            has_osm_id,
        })
    }
}

impl Iterator for PlaceIter {
    type Item = Result<Place>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Drain the current batch first.
            if let Some(batch) = &self.current_batch
                && self.batch_offset < batch.num_rows()
            {
                let result = decode_row(batch, self.batch_offset, self.has_osm_id);
                self.batch_offset += 1;
                return Some(result);
            }

            // Fetch the next batch from the parquet file.
            match self.reader.next() {
                Some(Ok(batch)) => {
                    self.current_batch = Some(batch);
                    self.batch_offset = 0;
                    // loop back to drain the new batch
                }
                Some(Err(e)) => return Some(Err(e).context("error reading Parquet batch")),
                None => return None, // EOF
            }
        }
    }
}

fn decode_row(batch: &RecordBatch, row: usize, has_osm_id: bool) -> Result<Place> {
    let s2_cell_id = col_as::<UInt64Array>(batch, "s2_cell_id")?.value(row);
    let osm_id = if has_osm_id {
        col_as::<UInt64Array>(batch, "osm_id")?.value(row)
    } else {
        0
    };

    let mask = MatchMask(col_as::<UInt16Array>(batch, "mask")?.value(row));
    let source = col_as::<StringArray>(batch, "source")?
        .value(row)
        .to_owned();
    let tags = decode_map_column(batch, row)?;

    Ok(Place {
        s2_cell_id,
        osm_id,
        source,
        mask,
        tags,
    })
}

/// Decode one row of the `tags` Map<Utf8, Utf8> column into `Vec<(String, String)>`.
fn decode_map_column(batch: &RecordBatch, row: usize) -> Result<Vec<(String, String)>> {
    let map_array = col_as::<MapArray>(batch, "tags")?;

    // `MapArray::value(i)` returns the StructArray slice for row `i`.
    let entries = map_array.value(row);

    // Child arrays within the struct are conventionally named "key" / "value".
    // If your file uses "keys"/"values" or something else, update these names.
    let keys = entries
        .column_by_name("key")
        .ok_or_else(|| anyhow!("map column 'tags' has no 'key' child"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("map column 'tags.key' is not Utf8"))?;

    let values = entries
        .column_by_name("value")
        .ok_or_else(|| anyhow!("map column 'tags' has no 'value' child"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("map column 'tags.value' is not Utf8"))?;

    let pairs = (0..keys.len())
        .map(|i| (keys.value(i).to_owned(), values.value(i).to_owned()))
        .collect();

    Ok(pairs)
}

/// Look up a column by name and downcast it to the concrete Arrow array type
/// `A`, returning a descriptive error on either failure.
fn col_as<'a, A: Array + 'static>(batch: &'a RecordBatch, name: &'static str) -> Result<&'a A> {
    batch
        .column_by_name(name)
        .ok_or_else(|| anyhow!("column '{name}' is missing from the batch schema"))?
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| anyhow!("column '{name}' has an unexpected Arrow type"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_place_iter_reads_parquet() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/test_data/alltheplaces.parquet"
        );

        let iter = PlaceIter::try_new(path).expect("failed to open test fixture");
        let places: Vec<Place> = iter
            .collect::<Result<Vec<_>>>()
            .expect("error while reading rows");
        assert!(!places.is_empty(), "expected at least one row");

        for place in &places {
            // s2_cell_id must be a valid S2 cell at some level (non-zero)
            assert_ne!(place.s2_cell_id, 0, "s2_cell_id should not be zero");

            // source must be non-empty
            assert!(!place.source.is_empty(), "source should not be empty");

            // each tag pair must have non-empty keys
            for (k, v) in &place.tags {
                assert!(!k.is_empty(), "tag key should not be empty");
                assert!(!v.is_empty(), "tag value should not be empty");
            }
        }
    }
}
