//! Turns `conflated.parquet` (produced by `crate::pipeline::conflate`)
//! into suggested OpenStreetMap edits, one GeoJSON tile layer per
//! category.
//!
//! Deliberately does *not* search OSM independently: `conflate()` has
//! already decided, for every AllThePlaces feature, which OSM feature
//! (if any) refers to the same real-world object -- redoing that search
//! here would mean paying for the whole candidate scan twice, and could
//! in principle disagree with conflate's answer. Instead this scans
//! `conflated.parquet` once, sequentially, and asks a
//! `crate::edit_suggesters::EditSuggester` what to change for each
//! already-matched pair.

use crate::edit_suggesters::create_edit_suggester;
use crate::{TileLayer, make_progress_bar};
use anyhow::{Context, Result};
use arrow::array::{
    Array, BinaryArray, MapArray, RecordBatch, StringArray, StructArray, UInt32Array, UInt64Array,
};
use deepsize::DeepSizeOf;
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use geo::Centroid;
use geo_traits::to_geo::ToGeoGeometry;
use indicatif::{MultiProgress, ProgressBar};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::{File, rename};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::Instant;
use wkb::reader::read_wkb;

/// One suggested change to an existing OpenStreetMap feature -- never a
/// suggestion to create a new feature (see the TODO on
/// `crate::places::Place::to_geojson`, which this mirrors for now: we
/// only ever suggest edits for AllThePlaces features `conflate()` has
/// matched to something that already exists in OSM).
///
/// `Ord`/`Eq` are implemented by hand (not derived) because they only
/// need to compare `osm_id` -- that's the only thing `write_edits`'s
/// sort-then-dedup needs, and `centroid`'s `f64`s don't have a natural
/// total order anyway.
#[derive(Debug, Clone, DeepSizeOf, Serialize, Deserialize)]
pub struct SuggestedEdit {
    pub osm_id: u64,
    pub osm_type: String, // "node" | "way" | "relation"
    pub osm_changeset: u64,
    pub osm_version: u32,
    pub tags: Vec<(String, String)>,
    /// (longitude, latitude). Just the OSM feature's centroid for now,
    /// not its full shape -- see the plan referenced in
    /// alltheplaces/osm-diffs#655 for why this is deliberately scoped
    /// down; revisit once tile layers are known to handle non-point
    /// geometry the way this pipeline needs.
    centroid: (f64, f64),
}

impl PartialEq for SuggestedEdit {
    fn eq(&self, other: &Self) -> bool {
        self.osm_id == other.osm_id
    }
}
impl Eq for SuggestedEdit {}
impl PartialOrd for SuggestedEdit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for SuggestedEdit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.osm_id.cmp(&other.osm_id)
    }
}

impl SuggestedEdit {
    pub fn to_geojson(&self) -> geojson::Feature {
        // Let's not emit coordinates with fake micrometer precision.
        let rounded_lon = (self.centroid.0 * 1e7).round() / 1e7;
        let rounded_lat = (self.centroid.1 * 1e7).round() / 1e7;
        let point = geo::point!(x: rounded_lon, y: rounded_lat);

        let mut properties: serde_json::Map<String, serde_json::Value> = self
            .tags
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        properties.insert(
            String::from("@osm_changeset"),
            serde_json::Value::from(self.osm_changeset),
        );
        properties.insert(
            String::from("@osm_version"),
            serde_json::Value::from(self.osm_version),
        );

        geojson::Feature {
            bbox: None,
            geometry: Some(geojson::Geometry::from(&point)),
            id: Some(geojson::feature::Id::Number(self.osm_id.into())),
            properties: Some(properties),
            foreign_members: None,
        }
    }
}

struct EditWriter {
    shops: SyncSender<SuggestedEdit>,
    _infrastructure: SyncSender<SuggestedEdit>,
    _trees: SyncSender<SuggestedEdit>,
}

impl EditWriter {
    fn make_layers(workdir: &Path) -> Vec<TileLayer> {
        ["shops", "infrastructure", "trees"]
            .iter()
            .map(|&s| TileLayer {
                name: String::from(s),
                path: workdir.join(String::from(s) + ".jsonl"),
            })
            .collect()
    }
}

pub fn suggest_edits(
    conflated: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<Vec<TileLayer>> {
    assert!(workdir.exists());

    let layers = EditWriter::make_layers(workdir);
    if layers.iter().all(|layer| layer.path.exists()) {
        return Ok(layers);
    }

    let num_rows = {
        // Parquet row counts live in file metadata -- cheap, no row I/O.
        let file = File::open(conflated)?;
        ParquetRecordBatchReaderBuilder::try_new(file)?
            .metadata()
            .file_metadata()
            .num_rows() as u64
    };
    let progress_bar = make_progress_bar(progress, "sugg-edit", num_rows, "conflated features");

    let mut producer_result = Ok(());
    let mut num_shop_edits = Ok(0);
    let mut num_infrastructure_edits = Ok(0);
    let mut num_tree_edits = Ok(0);
    thread::scope(|s| {
        let (shops_tx, shops_rx) = sync_channel::<SuggestedEdit>(8192);
        let (infrastructure_tx, infrastructure_rx) = sync_channel::<SuggestedEdit>(8192);
        let (trees_tx, trees_rx) = sync_channel::<SuggestedEdit>(8192);
        let writer = EditWriter {
            shops: shops_tx,
            _infrastructure: infrastructure_tx,
            _trees: trees_tx,
        };
        s.spawn(|| producer_result = produce_edits(conflated, num_rows, &progress_bar, writer));
        s.spawn(|| num_shop_edits = write_edits(shops_rx, &layers[0].path, workdir));
        s.spawn(|| {
            num_infrastructure_edits = write_edits(infrastructure_rx, &layers[1].path, workdir)
        });
        s.spawn(|| num_tree_edits = write_edits(trees_rx, &layers[2].path, workdir));
    });
    producer_result?;
    let num_edits = num_shop_edits? + num_infrastructure_edits? + num_tree_edits?;
    progress_bar.finish_with_message(format!(
        "{} conflated features → {} suggested OSM edits",
        num_rows, num_edits
    ));

    Ok(layers)
}

/// One decoded row of `conflated.parquet` that has both an ATP and an
/// OSM side -- i.e. one that `conflate()` matched. Rows with no OSM
/// match don't decode into this (nothing to suggest an edit for), but
/// still count against the caller's progress bar; see [read_conflated_rows].
struct ConflatedRow {
    atp_tags: Vec<(String, String)>,
    osm_type: String,
    osm_id: u64,
    osm_tags: Vec<(String, String)>,
    osm_changeset: u64,
    osm_version: u32,
    osm_geometry_wkb: Vec<u8>,
}

fn produce_edits(
    conflated: &Path,
    num_rows: u64,
    progress_bar: &ProgressBar,
    out: EditWriter,
) -> Result<()> {
    let start = Instant::now();
    let num_edits = AtomicU64::new(0);

    for batch in read_conflated_rows(conflated)? {
        batch?.par_iter().try_for_each(|row| {
            progress_bar.inc(1);
            let Some(row) = row else {
                return Ok(());
            };
            let Some(suggester) = create_edit_suggester(&row.atp_tags) else {
                return Ok(());
            };
            let Some(tags) = suggester.suggest_edit(&row.atp_tags, &row.osm_tags) else {
                return Ok(());
            };
            let edit = SuggestedEdit {
                osm_id: row.osm_id,
                osm_type: row.osm_type.clone(),
                osm_changeset: row.osm_changeset,
                osm_version: row.osm_version,
                tags,
                centroid: decode_centroid(&row.osm_geometry_wkb)?,
            };
            num_edits.fetch_add(1, Ordering::Relaxed);
            // TODO: Dispatch to one of {shops, infrastructure, trees}
            // once edit suggesters exist for more than shops.
            out.shops.send(edit)?;
            Ok::<(), anyhow::Error>(())
        })?;
    }

    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        conflated_rows = num_rows,
        edits_suggested = num_edits.load(Ordering::Relaxed);
        "suggest_edits: done"
    );
    Ok(())
}

fn decode_centroid(wkb: &[u8]) -> Result<(f64, f64)> {
    let geometry = read_wkb(wkb)
        .context("failed to decode OSM geometry")?
        .to_geometry();
    let centroid = geometry
        .centroid()
        .context("OSM geometry has no centroid")?;
    Ok((centroid.x(), centroid.y()))
}

/// Sequentially reads `conflated.parquet` batch by batch (Arrow's
/// natural row-group-ish chunking) -- no caching, no spatial index,
/// since every row is visited exactly once, in file order. The caller
/// processes each batch in parallel with Rayon; batches preserve
/// spatial locality (the file is S2-sorted) even though nothing here
/// relies on that ordering itself.
fn read_conflated_rows(
    path: &Path,
) -> Result<impl Iterator<Item = Result<Vec<Option<ConflatedRow>>>>> {
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    Ok(reader.map(|batch| {
        let batch = batch?;
        (0..batch.num_rows())
            .map(|row| extract_conflated_row(&batch, row))
            .collect()
    }))
}

/// `None` means this row has no OSM match (or, in principle, no ATP
/// side either -- the schema allows it even though `conflate()` never
/// actually produces such a row) -- nothing to suggest an edit for, but
/// still a real row the caller should count against its progress bar.
fn extract_conflated_row(batch: &RecordBatch, row: usize) -> Result<Option<ConflatedRow>> {
    let atp = get_struct(batch, "atp")?;
    if atp.is_null(row) {
        return Ok(None);
    }
    let osm = get_struct(batch, "osm")?;
    if osm.is_null(row) {
        return Ok(None);
    }

    Ok(Some(ConflatedRow {
        atp_tags: get_tags(atp, "tags", row)?,
        osm_type: get_string(osm, "type", row)?,
        osm_id: get_u64(osm, "id", row)?,
        osm_tags: get_tags(osm, "tags", row)?,
        osm_changeset: get_u64(osm, "changeset", row)?,
        osm_version: get_u32(osm, "version", row)?,
        osm_geometry_wkb: get_binary(osm, "geometry", row)?,
    }))
}

fn get_struct<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StructArray> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing column '{name}'"))?
        .as_any()
        .downcast_ref::<StructArray>()
        .with_context(|| format!("column '{name}' is not a struct"))
}

fn get_string(s: &StructArray, name: &str, row: usize) -> Result<String> {
    Ok(s.column_by_name(name)
        .with_context(|| format!("missing field '{name}'"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("field '{name}' is not a string"))?
        .value(row)
        .to_owned())
}

fn get_u64(s: &StructArray, name: &str, row: usize) -> Result<u64> {
    Ok(s.column_by_name(name)
        .with_context(|| format!("missing field '{name}'"))?
        .as_any()
        .downcast_ref::<UInt64Array>()
        .with_context(|| format!("field '{name}' is not UInt64"))?
        .value(row))
}

fn get_u32(s: &StructArray, name: &str, row: usize) -> Result<u32> {
    Ok(s.column_by_name(name)
        .with_context(|| format!("missing field '{name}'"))?
        .as_any()
        .downcast_ref::<UInt32Array>()
        .with_context(|| format!("field '{name}' is not UInt32"))?
        .value(row))
}

fn get_binary(s: &StructArray, name: &str, row: usize) -> Result<Vec<u8>> {
    Ok(s.column_by_name(name)
        .with_context(|| format!("missing field '{name}'"))?
        .as_any()
        .downcast_ref::<BinaryArray>()
        .with_context(|| format!("field '{name}' is not binary"))?
        .value(row)
        .to_vec())
}

fn get_tags(s: &StructArray, name: &str, row: usize) -> Result<Vec<(String, String)>> {
    let col = s
        .column_by_name(name)
        .with_context(|| format!("missing field '{name}'"))?
        .as_any()
        .downcast_ref::<MapArray>()
        .with_context(|| format!("field '{name}' is not a map"))?;

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

fn write_edits(edits: Receiver<SuggestedEdit>, path: &Path, workdir: &Path) -> Result<u64> {
    let mut tmp_path = PathBuf::from(&path);
    tmp_path.add_extension("tmp");
    let mut writer = BufWriter::with_capacity(32768, File::create(&tmp_path)?);

    let sorter: ExternalSorter<SuggestedEdit, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
            .build()?;

    let num_edits = AtomicU64::new(0);
    let sorted = sorter.sort(edits.iter().map(|x| {
        num_edits.fetch_add(1, Ordering::Relaxed);
        std::io::Result::Ok(x)
    }))?;
    let mut last_osm_id = None;
    for edit in sorted {
        let edit = edit?;
        // Only emit one single edit per OSM ID.
        if Some(edit.osm_id) == last_osm_id {
            continue;
        }
        last_osm_id = Some(edit.osm_id);
        let mut line = edit.to_geojson().to_string();
        line.push('\n');
        writer.write_all(line.as_ref())?;
    }
    writer.flush()?;
    rename(&tmp_path, path)?;

    Ok(num_edits.load(Ordering::SeqCst))
}
