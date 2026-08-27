//! Generic helpers for decoding a single row's worth of leaf values out
//! of an Arrow [`RecordBatch`]/[`StructArray`] read back from a Parquet
//! file -- shared by every reader in this crate that walks a
//! `ParquetRecordBatchReaderBuilder`-produced batch by hand:
//! `places::reader` (flat columns, for `alltheplaces.parquet`) and
//! `pipeline::edits`/`pipeline::conflated_tiles` (nested `atp`/`osm`
//! struct columns, for `conflated.parquet`).
//!
//! [`RecordBatch::column_by_name`] and [`StructArray::column_by_name`]
//! have the identical signature -- `Option<&ArrayRef>` -- so
//! [`ColumnSource`] unifies "look up a named column, whether at the
//! schema root or nested inside a struct" behind one trait, and every
//! function below is generic over it instead of being written twice.

use anyhow::{Context, Result};
use arrow::array::{
    Array, ArrayRef, BinaryArray, MapArray, RecordBatch, StringArray, StructArray, UInt16Array,
    UInt32Array, UInt64Array,
};

/// Something a named column can be looked up in -- a [`RecordBatch`]
/// (columns at the schema root) or a [`StructArray`] (fields of a
/// nested struct column, e.g. `conflated.parquet`'s `atp`/`osm`).
pub(crate) trait ColumnSource {
    fn column_by_name(&self, name: &str) -> Option<&ArrayRef>;
}

impl ColumnSource for RecordBatch {
    fn column_by_name(&self, name: &str) -> Option<&ArrayRef> {
        RecordBatch::column_by_name(self, name)
    }
}

impl ColumnSource for StructArray {
    fn column_by_name(&self, name: &str) -> Option<&ArrayRef> {
        StructArray::column_by_name(self, name)
    }
}

fn column<'a>(src: &'a impl ColumnSource, name: &str) -> Result<&'a ArrayRef> {
    src.column_by_name(name)
        .with_context(|| format!("missing column '{name}'"))
}

/// Looks up `name` as a nested struct column -- e.g. `conflated.parquet`'s
/// top-level `osm` column, or `osm`'s own nested `modified` field. Works
/// for both a [`RecordBatch`] and a [`StructArray`] source, since a
/// struct-typed column decodes to a [`StructArray`] either way.
pub(crate) fn get_struct<'a>(src: &'a impl ColumnSource, name: &str) -> Result<&'a StructArray> {
    column(src, name)?
        .as_any()
        .downcast_ref::<StructArray>()
        .with_context(|| format!("column '{name}' is not a struct"))
}

pub(crate) fn get_string(src: &impl ColumnSource, name: &str, row: usize) -> Result<String> {
    Ok(column(src, name)?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("column '{name}' is not a string"))?
        .value(row)
        .to_owned())
}

pub(crate) fn get_u64(src: &impl ColumnSource, name: &str, row: usize) -> Result<u64> {
    Ok(column(src, name)?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .with_context(|| format!("column '{name}' is not UInt64"))?
        .value(row))
}

pub(crate) fn get_u32(src: &impl ColumnSource, name: &str, row: usize) -> Result<u32> {
    Ok(column(src, name)?
        .as_any()
        .downcast_ref::<UInt32Array>()
        .with_context(|| format!("column '{name}' is not UInt32"))?
        .value(row))
}

pub(crate) fn get_u16(src: &impl ColumnSource, name: &str, row: usize) -> Result<u16> {
    Ok(column(src, name)?
        .as_any()
        .downcast_ref::<UInt16Array>()
        .with_context(|| format!("column '{name}' is not UInt16"))?
        .value(row))
}

pub(crate) fn get_binary(src: &impl ColumnSource, name: &str, row: usize) -> Result<Vec<u8>> {
    Ok(column(src, name)?
        .as_any()
        .downcast_ref::<BinaryArray>()
        .with_context(|| format!("column '{name}' is not Binary"))?
        .value(row)
        .to_vec())
}

/// Decodes a `MAP<UTF8, UTF8>` column (e.g. `conflated.parquet`'s
/// `atp.tags`/`osm.tags`, or `alltheplaces.parquet`'s `tags`) at `row`
/// into key/value pairs, in the map's own order.
pub(crate) fn get_tags(
    src: &impl ColumnSource,
    name: &str,
    row: usize,
) -> Result<Vec<(String, String)>> {
    let col = column(src, name)?
        .as_any()
        .downcast_ref::<MapArray>()
        .with_context(|| format!("column '{name}' is not a MapArray"))?;

    let entry = col.value(row);
    let keys = entry
        .column_by_name("key")
        .with_context(|| format!("map '{name}' has no 'key' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("map '{name}' keys are not strings"))?;
    let values = entry
        .column_by_name("value")
        .with_context(|| format!("map '{name}' has no 'value' field"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("map '{name}' values are not strings"))?;

    let mut tags = Vec::with_capacity(keys.len());
    for i in 0..keys.len() {
        tags.push((keys.value(i).to_owned(), values.value(i).to_owned()));
    }
    Ok(tags)
}
