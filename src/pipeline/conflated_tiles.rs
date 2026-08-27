//! Turns `conflated.parquet` (produced by `crate::pipeline::conflate`)
//! into a PMTiles-ready pair of GeoJSON Lines tile layers, for
//! visualizing the *matching* step itself -- every AllThePlaces
//! feature, matched or not -- independent of whatever
//! `crate::pipeline::edits` separately decides to propose as an edit
//! (see [#709](https://github.com/alltheplaces/osm-diffs/issues/709)).
//!
//! Unlike `edits`, this doesn't need an external sort: it emits one
//! output feature per input row, so there's nothing to deduplicate or
//! merge across rows -- a straight sequential-batch, rayon-parallel-per-
//! batch scan is enough.

use crate::utils::parquet::{get_binary, get_string, get_struct, get_tags, get_u64};
use crate::{TileLayer, make_progress_bar};
use anyhow::{Context, Result};
use arrow::array::{Array, RecordBatch};
use geo::MapCoordsInPlace;
use geo_traits::to_geo::ToGeoGeometry;
use indicatif::{MultiProgress, ProgressBar};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rayon::prelude::*;
use serde_json::{Map, Value};
use std::fs::{File, rename};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use wkb::reader::read_wkb;

/// One decoded row of `conflated.parquet`, kept whether or not it has
/// an OSM match -- unlike `edits::ConflatedRow`, which only exists for
/// matched rows (nothing to suggest an edit for otherwise). `None`
/// means this row has no ATP side either -- the schema allows it even
/// though `conflate()` never actually produces such a row -- and is
/// skipped entirely, the same defensive handling `edits.rs` applies.
struct ConflatedTileRow {
    spider: String,
    atp_tags: Vec<(String, String)>,
    atp_geometry_wkb: Vec<u8>,
    osm: Option<OsmSide>,
}

struct OsmSide {
    osm_type: String,
    osm_id: u64,
    osm_tags: Vec<(String, String)>,
    osm_geometry_wkb: Vec<u8>,
}

impl ConflatedTileRow {
    fn to_geojson_line(&self) -> Result<String> {
        // Geometry: the actual OSM shape when matched (what a reviewer
        // is meant to review -- point/line/polygon, all valid within
        // one GeoJSON/MVT layer), the raw ATP-provided shape otherwise.
        // Simplest useful choice for v1 -- see
        // https://github.com/alltheplaces/osm-diffs/issues/775 for a
        // richer "union of atp_geometry + osm_geometry + a connector
        // line between their centroids" idea deliberately left for
        // later: a Mapbox Vector Tile feature can only be one geometry
        // type, so that idea needs three separate features per matched
        // row, not one, and deserves its own memory/size measurement.
        //
        // TODO(#713): once conflated.parquet carries a per-row rank/
        // importance signal, this is the natural place to also emit it
        // as a feature property (or otherwise feed it to tippecanoe),
        // so --drop-densest-as-needed can prefer important places over
        // low-priority ones at low zoom instead of today's
        // density-only heuristic.
        let geometry_wkb: &[u8] = match &self.osm {
            Some(osm) => &osm.osm_geometry_wkb,
            None => &self.atp_geometry_wkb,
        };
        let mut geometry = read_wkb(geometry_wkb)
            .context("failed to decode geometry")?
            .to_geometry();
        // Let's not emit coordinates with fake sub-millimeter
        // precision, particularly for atp_geometry, which is whatever
        // precision AllThePlaces' upstream source happened to provide
        // -- same 1e-7-degree rounding SuggestedEdit::to_geojson
        // already applies to its own point.
        geometry.map_coords_in_place(|c| geo::Coord {
            x: (c.x * 1e7).round() / 1e7,
            y: (c.y * 1e7).round() / 1e7,
        });

        let mut properties: Map<String, Value> = Map::new();
        properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        for (k, v) in &self.atp_tags {
            properties.insert(format!("atp:{k}"), Value::String(v.clone()));
        }
        if let Some(osm) = &self.osm {
            properties.insert("osm:type".to_string(), Value::String(osm.osm_type.clone()));
            properties.insert("osm:id".to_string(), Value::from(osm.osm_id));
            for (k, v) in &osm.osm_tags {
                properties.insert(format!("osm:{k}"), Value::String(v.clone()));
            }
        }

        let feature = geojson::Feature {
            bbox: None,
            geometry: Some(geojson::Geometry::from(&geometry)),
            id: None,
            properties: Some(properties),
            foreign_members: None,
        };
        let mut line = feature.to_string();
        line.push('\n');
        Ok(line)
    }
}

pub fn extract_conflated_layers(
    conflated: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<Vec<TileLayer>> {
    assert!(workdir.exists());

    let layers = vec![
        TileLayer {
            name: String::from("matched"),
            path: workdir.join("matched.jsonl"),
        },
        TileLayer {
            name: String::from("unmatched"),
            path: workdir.join("unmatched.jsonl"),
        },
    ];
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
    let progress_bar = make_progress_bar(progress, "confl-tiles", num_rows, "conflated features");

    let mut matched_tmp = PathBuf::from(&layers[0].path);
    matched_tmp.add_extension("tmp");
    let mut unmatched_tmp = PathBuf::from(&layers[1].path);
    unmatched_tmp.add_extension("tmp");
    let mut matched_writer = BufWriter::with_capacity(32768, File::create(&matched_tmp)?);
    let mut unmatched_writer = BufWriter::with_capacity(32768, File::create(&unmatched_tmp)?);

    let (num_matched, num_unmatched) = extract_rows(
        conflated,
        &progress_bar,
        &mut matched_writer,
        &mut unmatched_writer,
    )?;
    matched_writer.flush()?;
    unmatched_writer.flush()?;
    rename(&matched_tmp, &layers[0].path)?;
    rename(&unmatched_tmp, &layers[1].path)?;

    progress_bar.finish_with_message(format!(
        "{} conflated features → {} matched, {} unmatched",
        num_rows, num_matched, num_unmatched
    ));

    Ok(layers)
}

fn extract_rows(
    conflated: &Path,
    progress_bar: &ProgressBar,
    matched_writer: &mut impl Write,
    unmatched_writer: &mut impl Write,
) -> Result<(u64, u64)> {
    let start = Instant::now();
    let mut num_matched = 0u64;
    let mut num_unmatched = 0u64;

    let file = File::open(conflated)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    for batch in reader {
        let batch = batch?;
        // Decoding + GeoJSON serialization is the CPU-heavy part of
        // this loop; parallelize it per batch with Rayon, same as
        // edits.rs's produce_edits, then write out sequentially --
        // there's nothing to deduplicate or sort here, unlike edits.rs,
        // so no channels/external sort are needed, just an ordered
        // in-memory Vec per batch.
        let lines: Vec<Option<(bool, String)>> = (0..batch.num_rows())
            .into_par_iter()
            .map(|row| -> Result<Option<(bool, String)>> {
                let Some(row) = extract_conflated_tile_row(&batch, row)? else {
                    return Ok(None);
                };
                let matched = row.osm.is_some();
                Ok(Some((matched, row.to_geojson_line()?)))
            })
            .collect::<Result<Vec<_>>>()?;

        for line in lines {
            progress_bar.inc(1);
            let Some((matched, line)) = line else {
                continue;
            };
            if matched {
                matched_writer.write_all(line.as_bytes())?;
                num_matched += 1;
            } else {
                unmatched_writer.write_all(line.as_bytes())?;
                num_unmatched += 1;
            }
        }
    }

    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        matched = num_matched,
        unmatched = num_unmatched;
        "extract_conflated_layers: done"
    );
    Ok((num_matched, num_unmatched))
}

/// `None` means this row has no ATP side -- the schema allows it even
/// though `conflate()` never actually produces such a row -- nothing to
/// visualize, but still a real row the caller should count against its
/// progress bar. Unlike `edits::extract_conflated_row`, a `None` `osm`
/// is kept, not skipped: an unmatched ATP feature is exactly what this
/// module exists to show.
fn extract_conflated_tile_row(batch: &RecordBatch, row: usize) -> Result<Option<ConflatedTileRow>> {
    let atp = get_struct(batch, "atp")?;
    if atp.is_null(row) {
        return Ok(None);
    }
    let fetched = get_struct(atp, "fetched")?;

    let osm = get_struct(batch, "osm")?;
    let osm = if osm.is_null(row) {
        None
    } else {
        Some(OsmSide {
            osm_type: get_string(osm, "type", row)?,
            osm_id: get_u64(osm, "id", row)?,
            osm_tags: get_tags(osm, "tags", row)?,
            // Top-level, not nested inside `osm` -- GeoParquet 2.0
            // requires geometry columns to live at the schema root
            // (see `pipeline::conflate::writer::GEO_METADATA_KEY`'s
            // doc comment).
            osm_geometry_wkb: get_binary(batch, "osm_geometry", row)?,
        })
    };

    Ok(Some(ConflatedTileRow {
        spider: get_string(fetched, "spider", row)?,
        atp_tags: get_tags(atp, "tags", row)?,
        atp_geometry_wkb: get_binary(batch, "atp_geometry", row)?,
        osm,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wkb::writer::{WriteOptions, write_geometry};

    fn encode_point(lon: f64, lat: f64) -> Vec<u8> {
        let point = geo::point!(x: lon, y: lat);
        let mut buf = Vec::new();
        write_geometry(&mut buf, &point, &WriteOptions::default()).expect("encode point");
        buf
    }

    #[test]
    fn unmatched_row_uses_atp_geometry_and_only_atp_tags() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![("shop".to_string(), "bakery".to_string())],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None,
        };
        let line = row.to_geojson_line().expect("to_geojson_line");
        assert!(line.ends_with('\n'));
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid json");

        assert_eq!(value["type"], "Feature");
        assert_eq!(value["geometry"]["type"], "Point");
        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.5, 47.2])
        );
        assert_eq!(value["properties"]["spider"], "acme");
        assert_eq!(value["properties"]["atp:shop"], "bakery");
        assert!(value["properties"].get("osm:type").is_none());
    }

    #[test]
    fn matched_row_uses_osm_geometry_and_both_tag_sets() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![("shop".to_string(), "bakery".to_string())],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: Some(OsmSide {
                osm_type: "node".to_string(),
                osm_id: 42,
                osm_tags: vec![("shop".to_string(), "bakery".to_string())],
                osm_geometry_wkb: encode_point(8.50001, 47.20001),
            }),
        };
        let line = row.to_geojson_line().expect("to_geojson_line");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid json");

        // The OSM shape, not the ATP one -- that's the whole point of
        // preferring it once matched.
        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.50001, 47.20001])
        );
        assert_eq!(value["properties"]["osm:id"], 42);
        assert_eq!(value["properties"]["osm:type"], "node");
        assert_eq!(value["properties"]["osm:shop"], "bakery");
        assert_eq!(value["properties"]["atp:shop"], "bakery");
    }

    #[test]
    fn geometry_coordinates_are_rounded_to_seven_decimal_places() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![],
            atp_geometry_wkb: encode_point(8.123_456_789, 47.987_654_321),
            osm: None,
        };
        let line = row.to_geojson_line().expect("to_geojson_line");
        let value: Value = serde_json::from_str(line.trim_end()).expect("valid json");

        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.1234568, 47.9876543])
        );
    }
}
