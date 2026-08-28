//! Turns `conflated.parquet` (produced by `crate::pipeline::conflate`)
//! into PMTiles-ready GeoJSON Lines tile layers, for visualizing the
//! *matching* step itself -- every AllThePlaces feature, matched or
//! not -- independent of whatever `crate::pipeline::edits` separately
//! decides to propose as an edit (see
//! [#709](https://github.com/alltheplaces/osm-diffs/issues/709)).
//!
//! Unlike `edits`, this doesn't need an external sort: it emits one
//! output feature per input row, so there's nothing to deduplicate or
//! merge across rows -- a straight sequential-batch, rayon-parallel-per-
//! batch scan is enough.
//!
//! [`extract_conflated_layers`] produces every layer
//! `conflated.pmtiles`' two-pass build needs (see
//! `pipeline::tiles::ZoomRange` and `pipeline::tiles::join_tiles`) in
//! one scan: the coarse overview layers (single feature per row, both
//! matched and unmatched), and the high-zoom detail layer (matched
//! rows only, up to three features per row -- see
//! [`ConflatedTileRow::to_detail_geojson_lines`] for why matched rows
//! need more than one feature there). See
//! [#775](https://github.com/alltheplaces/osm-diffs/issues/775) for
//! the full design discussion.

use crate::utils::parquet::{get_binary, get_string, get_struct, get_tags, get_u64};
use crate::{TileLayer, make_progress_bar};
use anyhow::{Context, Result};
use arrow::array::{Array, RecordBatch};
use geo::{Centroid, Distance, Haversine, MapCoordsInPlace};
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

/// The tippecanoe layer name matched features belong to, whether they
/// come from the overview pass's single-feature representation or the
/// detail pass's up-to-three-feature one. A named constant, not a
/// string literal repeated at each construction site: both
/// `ConflatedLayers.overview_matched` and `ConflatedLayers.detail`
/// are built from this same symbol, so
/// `tile-join` is guaranteed to merge them into one continuous
/// `matched` layer spanning the whole zoom range -- see
/// `DETAIL_MIN_ZOOM`'s doc comment for why that split exists at all.
/// If these ever named different strings, `tile-join` wouldn't error;
/// it would just silently produce two separately-toggleable layers
/// instead of one, so keeping this a single source of truth matters
/// more than it might look.
const MATCHED_LAYER_NAME: &str = "matched";

/// The tippecanoe layer name unmatched features belong to. Only ever
/// used by the overview pass -- unlike `MATCHED_LAYER_NAME`, nothing
/// else needs to agree with this one, but it's named for the same
/// reason: so the string exists in exactly one place.
const UNMATCHED_LAYER_NAME: &str = "unmatched";

/// Zoom level at which `conflated.pmtiles`' detail pass (fed by
/// [`ConflatedLayers::detail`]) takes over from the coarse overview
/// pass (fed by [`ConflatedLayers::overview`]). The overview pass is
/// built up to `DETAIL_MIN_ZOOM - 1`; the detail pass starts at
/// `DETAIL_MIN_ZOOM`. Keep both call sites (`pipeline::mod`'s
/// `render_conflated_overview`/`render_conflated_detail` steps) in
/// sync with this constant -- there must be neither a gap nor an
/// overlap between the two ranges, or `tile-join` either leaves a hole
/// or duplicates content at the boundary zoom.
pub(crate) const DETAIL_MIN_ZOOM: u8 = 13;

/// Highest zoom level the detail pass builds. Deliberately an explicit
/// ceiling, not tippecanoe's `-zg`/`--extend-zooms-if-still-dropping`:
/// that combination keeps adding zoom levels until every crowded
/// feature has separated into its own tile, which never happens for a
/// connector line whose two ends are nearly coincident (a *good*
/// match!) -- such a line never spatially separates from its own
/// endpoint no matter how far you zoom in. An early, unbounded
/// prototype of this feature reached z19 chasing that impossible
/// separation before an unrelated internal limit stopped it (see the
/// investigation note linked from #775). `MIN_CONNECTOR_LENGTH_METERS`
/// below heads off most such cases at the source, but this ceiling is
/// the hard backstop regardless.
pub(crate) const DETAIL_MAX_ZOOM: u8 = 16;

/// Below this length, a connector line's endpoints are close enough
/// that drawing a line between them adds no visual information to a
/// reviewer -- and is exactly the degenerate geometry
/// `DETAIL_MAX_ZOOM`'s doc comment describes. Comfortably above
/// typical GPS/geocoding noise (a few meters) and well below "this
/// match looks meaningfully offset" (tens of meters); real offsets
/// measured against a full-planet run ranged from ~0.2m (a coincident
/// match) up to the matcher's own ~400m search-radius cap. Tune if
/// real-world review turns up either false positives (connectors
/// shown for noise-level offsets) or missed ones (a genuine offset
/// silently dropped).
pub(crate) const MIN_CONNECTOR_LENGTH_METERS: f64 = 5.0;

/// Every GeoJSON Lines layer [`extract_conflated_layers`] produces,
/// grouped by which of `conflated.pmtiles`' two tippecanoe passes
/// consumes them. Named fields, not a `Vec` some caller has to index
/// positionally and remember the order of.
pub struct ConflatedLayers {
    /// Fed to the overview pass (`ZoomRange::Bounded { min: 0, max:
    /// DETAIL_MIN_ZOOM - 1 }`) together with [`Self::overview_unmatched`].
    pub overview_matched: TileLayer,
    /// Fed to the overview pass together with [`Self::overview_matched`].
    pub overview_unmatched: TileLayer,
    /// Fed to the detail pass (`ZoomRange::Bounded { min:
    /// DETAIL_MIN_ZOOM, max: DETAIL_MAX_ZOOM }`): matched rows only,
    /// up to three features each. Named `MATCHED_LAYER_NAME` -- the
    /// same layer name [`Self::overview_matched`] uses -- so
    /// `tile-join` merges them into one continuous layer; see that
    /// constant's doc comment.
    pub detail: TileLayer,
}

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
    /// The coarse overview layer's representation: one feature per
    /// row. Geometry is the actual OSM shape when matched (what a
    /// reviewer is meant to review -- point/line/polygon, all valid
    /// within one GeoJSON/MVT layer), the raw ATP-provided shape
    /// otherwise. See [`Self::to_detail_geojson_lines`] for the
    /// richer, matched-only, high-zoom-only representation.
    ///
    /// TODO(#713): once conflated.parquet carries a per-row rank/
    /// importance signal, this is the natural place to also emit it
    /// as a feature property (or otherwise feed it to tippecanoe),
    /// so --drop-densest-as-needed can prefer important places over
    /// low-priority ones at low zoom instead of today's
    /// density-only heuristic.
    fn to_geojson_line(&self) -> Result<String> {
        let geometry_wkb: &[u8] = match &self.osm {
            Some(osm) => &osm.osm_geometry_wkb,
            None => &self.atp_geometry_wkb,
        };
        let geometry = decode_and_round(geometry_wkb)?;

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
        geometry_to_geojson_line(&geometry, properties)
    }

    /// The detail layer's representation of a *matched* row -- never
    /// called for an unmatched one, since the detail pass is
    /// matched-only (an unmatched row is already a single point at
    /// every zoom, so it never benefits from higher zoom's extra
    /// detail). `osm` is passed explicitly, rather than read from
    /// `self.osm`, so that precondition is enforced by the type
    /// system instead of an internal `Option::unwrap`.
    ///
    /// Yields the ATP point and the OSM shape as two separate
    /// features -- each carrying only its own side's tags (`atp:*` or
    /// `osm:*`), so a tile inspector shows which end is which -- plus
    /// a connector line between their centroids, unless that line
    /// would be shorter than `MIN_CONNECTOR_LENGTH_METERS` (see that
    /// constant's doc comment for why). Every feature also carries a
    /// `part` property (`"atp"` | `"osm"` | `"link"`) identifying its
    /// role, and the connector line -- the only feature that isn't
    /// clearly one side or the other -- keeps both tag sets.
    fn to_detail_geojson_lines(&self, osm: &OsmSide) -> Result<Vec<String>> {
        let atp_geometry = decode_and_round(&self.atp_geometry_wkb)?;
        let osm_geometry = decode_and_round(&osm.osm_geometry_wkb)?;

        let mut atp_properties: Map<String, Value> = Map::new();
        atp_properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        atp_properties.insert("part".to_string(), Value::String("atp".to_string()));
        for (k, v) in &self.atp_tags {
            atp_properties.insert(format!("atp:{k}"), Value::String(v.clone()));
        }

        let mut osm_properties: Map<String, Value> = Map::new();
        osm_properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        osm_properties.insert("part".to_string(), Value::String("osm".to_string()));
        osm_properties.insert("osm:type".to_string(), Value::String(osm.osm_type.clone()));
        osm_properties.insert("osm:id".to_string(), Value::from(osm.osm_id));
        for (k, v) in &osm.osm_tags {
            osm_properties.insert(format!("osm:{k}"), Value::String(v.clone()));
        }

        let mut lines = vec![
            geometry_to_geojson_line(&atp_geometry, atp_properties)?,
            geometry_to_geojson_line(&osm_geometry, osm_properties)?,
        ];

        let atp_centroid = atp_geometry
            .centroid()
            .context("atp_geometry has no centroid")?;
        let osm_centroid = osm_geometry
            .centroid()
            .context("osm_geometry has no centroid")?;
        if Haversine.distance(atp_centroid, osm_centroid) >= MIN_CONNECTOR_LENGTH_METERS {
            let connector = geo::Geometry::LineString(geo::LineString::new(vec![
                atp_centroid.into(),
                osm_centroid.into(),
            ]));
            let mut link_properties: Map<String, Value> = Map::new();
            link_properties.insert("spider".to_string(), Value::String(self.spider.clone()));
            link_properties.insert("part".to_string(), Value::String("link".to_string()));
            for (k, v) in &self.atp_tags {
                link_properties.insert(format!("atp:{k}"), Value::String(v.clone()));
            }
            link_properties.insert("osm:type".to_string(), Value::String(osm.osm_type.clone()));
            link_properties.insert("osm:id".to_string(), Value::from(osm.osm_id));
            for (k, v) in &osm.osm_tags {
                link_properties.insert(format!("osm:{k}"), Value::String(v.clone()));
            }
            lines.push(geometry_to_geojson_line(&connector, link_properties)?);
        }

        Ok(lines)
    }
}

/// Decodes WKB and rounds coordinates to 1e-7 degrees (about 1cm at
/// the equator) -- same precision `SuggestedEdit::to_geojson` already
/// applies to its own point, so we're not emitting fake sub-millimeter
/// precision, particularly for `atp_geometry`, which is whatever
/// precision AllThePlaces' upstream source happened to provide.
fn decode_and_round(wkb: &[u8]) -> Result<geo::Geometry<f64>> {
    let mut geometry = read_wkb(wkb)
        .context("failed to decode geometry")?
        .to_geometry();
    geometry.map_coords_in_place(|c| geo::Coord {
        x: (c.x * 1e7).round() / 1e7,
        y: (c.y * 1e7).round() / 1e7,
    });
    Ok(geometry)
}

fn geometry_to_geojson_line(
    geometry: &geo::Geometry<f64>,
    properties: Map<String, Value>,
) -> Result<String> {
    let feature = geojson::Feature {
        bbox: None,
        geometry: Some(geojson::Geometry::from(geometry)),
        id: None,
        properties: Some(properties),
        foreign_members: None,
    };
    let mut line = feature.to_string();
    line.push('\n');
    Ok(line)
}

/// Scans `conflated.parquet` once and writes every GeoJSON Lines layer
/// `conflated.pmtiles`' overview and detail passes need -- see
/// [`ConflatedLayers`]. One pass, not two: every row's decode work
/// (WKB, tags, geometry rounding) happens once, even though matched
/// rows contribute to both the overview and detail layers.
pub fn extract_conflated_layers(
    conflated: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<ConflatedLayers> {
    assert!(workdir.exists());

    let layers = ConflatedLayers {
        overview_matched: TileLayer {
            name: String::from(MATCHED_LAYER_NAME),
            path: workdir.join("matched.jsonl"),
        },
        overview_unmatched: TileLayer {
            name: String::from(UNMATCHED_LAYER_NAME),
            path: workdir.join("unmatched.jsonl"),
        },
        detail: TileLayer {
            name: String::from(MATCHED_LAYER_NAME),
            path: workdir.join("matched-detail.jsonl"),
        },
    };
    if [
        &layers.overview_matched,
        &layers.overview_unmatched,
        &layers.detail,
    ]
    .iter()
    .all(|layer| layer.path.exists())
    {
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

    let mut matched_tmp = PathBuf::from(&layers.overview_matched.path);
    matched_tmp.add_extension("tmp");
    let mut unmatched_tmp = PathBuf::from(&layers.overview_unmatched.path);
    unmatched_tmp.add_extension("tmp");
    let mut detail_tmp = PathBuf::from(&layers.detail.path);
    detail_tmp.add_extension("tmp");
    let mut matched_writer = BufWriter::with_capacity(32768, File::create(&matched_tmp)?);
    let mut unmatched_writer = BufWriter::with_capacity(32768, File::create(&unmatched_tmp)?);
    let mut detail_writer = BufWriter::with_capacity(32768, File::create(&detail_tmp)?);

    let counts = extract_rows(
        conflated,
        &progress_bar,
        &mut matched_writer,
        &mut unmatched_writer,
        &mut detail_writer,
    )?;
    matched_writer.flush()?;
    unmatched_writer.flush()?;
    detail_writer.flush()?;
    rename(&matched_tmp, &layers.overview_matched.path)?;
    rename(&unmatched_tmp, &layers.overview_unmatched.path)?;
    rename(&detail_tmp, &layers.detail.path)?;

    progress_bar.finish_with_message(format!(
        "{} conflated features → {} matched, {} unmatched, {} detail features",
        num_rows, counts.matched, counts.unmatched, counts.detail_features
    ));

    Ok(layers)
}

struct RowCounts {
    matched: u64,
    unmatched: u64,
    detail_features: u64,
}

fn extract_rows(
    conflated: &Path,
    progress_bar: &ProgressBar,
    matched_writer: &mut impl Write,
    unmatched_writer: &mut impl Write,
    detail_writer: &mut impl Write,
) -> Result<RowCounts> {
    let start = Instant::now();
    let mut counts = RowCounts {
        matched: 0,
        unmatched: 0,
        detail_features: 0,
    };

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
        type RowResult = Option<(bool, String, Vec<String>)>;
        let rows: Vec<RowResult> = (0..batch.num_rows())
            .into_par_iter()
            .map(|row| -> Result<RowResult> {
                let Some(row) = extract_conflated_tile_row(&batch, row)? else {
                    return Ok(None);
                };
                let overview_line = row.to_geojson_line()?;
                let detail_lines = match &row.osm {
                    Some(osm) => row.to_detail_geojson_lines(osm)?,
                    None => Vec::new(),
                };
                Ok(Some((row.osm.is_some(), overview_line, detail_lines)))
            })
            .collect::<Result<Vec<_>>>()?;

        for entry in rows {
            progress_bar.inc(1);
            let Some((matched, overview_line, detail_lines)) = entry else {
                continue;
            };
            if matched {
                matched_writer.write_all(overview_line.as_bytes())?;
                counts.matched += 1;
                for line in detail_lines {
                    detail_writer.write_all(line.as_bytes())?;
                    counts.detail_features += 1;
                }
            } else {
                unmatched_writer.write_all(overview_line.as_bytes())?;
                counts.unmatched += 1;
            }
        }
    }

    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        matched = counts.matched,
        unmatched = counts.unmatched,
        detail_features = counts.detail_features;
        "extract_conflated_layers: done"
    );
    Ok(counts)
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
    use indicatif::ProgressDrawTarget;
    use tempfile::TempDir;
    use wkb::writer::{WriteOptions, write_geometry};

    fn hidden_progress() -> MultiProgress {
        MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
    }

    #[test]
    fn memoized_layers_share_the_matched_layer_name() {
        // Exercises the real extract_conflated_layers/ConflatedLayers
        // construction, not just the constant in isolation: pre-create
        // all three output files so the memoization check short-circuits
        // before ever touching `conflated` (a path that doesn't exist),
        // then confirm the overview's matched layer and the detail
        // layer really do carry the identical tippecanoe layer name --
        // the whole reason ConflatedLayers/MATCHED_LAYER_NAME exist: if
        // a future edit ever let these drift apart, tile-join wouldn't
        // error, it would just silently produce two layers instead of
        // one continuous one.
        let workdir = TempDir::new().expect("tempdir");
        for name in ["matched.jsonl", "unmatched.jsonl", "matched-detail.jsonl"] {
            std::fs::write(workdir.path().join(name), b"").expect("write placeholder");
        }

        let layers = extract_conflated_layers(
            Path::new("/no/such/conflated.parquet"),
            &hidden_progress(),
            workdir.path(),
        )
        .expect("memoized path should not touch `conflated` at all");

        assert_eq!(layers.overview_matched.name, layers.detail.name);
        assert_eq!(layers.detail.name, MATCHED_LAYER_NAME);
    }

    fn encode_point(lon: f64, lat: f64) -> Vec<u8> {
        let point = geo::point!(x: lon, y: lat);
        let mut buf = Vec::new();
        write_geometry(&mut buf, &point, &WriteOptions::default()).expect("encode point");
        buf
    }

    fn parse(line: &str) -> Value {
        assert!(line.ends_with('\n'));
        serde_json::from_str(line.trim_end()).expect("valid json")
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
        let value = parse(&line);

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
        let value = parse(&line);

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
        let value = parse(&line);

        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.1234568, 47.9876543])
        );
    }

    fn osm_side_far_from(atp_lon: f64, atp_lat: f64) -> OsmSide {
        // Roughly 50m north -- comfortably above MIN_CONNECTOR_LENGTH_METERS.
        OsmSide {
            osm_type: "way".to_string(),
            osm_id: 99,
            osm_tags: vec![("shop".to_string(), "supermarket".to_string())],
            osm_geometry_wkb: encode_point(atp_lon, atp_lat + 0.00045),
        }
    }

    #[test]
    fn detail_yields_atp_and_osm_features_with_only_their_own_tags() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![("name".to_string(), "Acme ATP".to_string())],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None, // unused; osm side passed explicitly below
        };
        let osm = osm_side_far_from(8.5, 47.2);
        let lines = row
            .to_detail_geojson_lines(&osm)
            .expect("to_detail_geojson_lines");
        let features: Vec<Value> = lines.iter().map(|l| parse(l)).collect();

        let atp = features
            .iter()
            .find(|f| f["properties"]["part"] == "atp")
            .expect("expected an atp feature");
        assert_eq!(atp["properties"]["atp:name"], "Acme ATP");
        assert!(
            atp["properties"].get("osm:type").is_none(),
            "atp feature should carry no osm:* properties: {atp}"
        );

        let osm_feature = features
            .iter()
            .find(|f| f["properties"]["part"] == "osm")
            .expect("expected an osm feature");
        assert_eq!(osm_feature["properties"]["osm:shop"], "supermarket");
        assert_eq!(osm_feature["properties"]["osm:id"], 99);
        assert!(
            osm_feature["properties"].get("atp:name").is_none(),
            "osm feature should carry no atp:* properties: {osm_feature}"
        );

        let link = features
            .iter()
            .find(|f| f["properties"]["part"] == "link")
            .expect("expected a link feature -- offset is well above the threshold");
        assert_eq!(link["properties"]["atp:name"], "Acme ATP");
        assert_eq!(link["properties"]["osm:shop"], "supermarket");
    }

    #[test]
    fn detail_omits_the_connector_when_offset_is_below_threshold() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None,
        };
        // Same coordinates as the ATP point -- a coincident match.
        let osm = OsmSide {
            osm_type: "node".to_string(),
            osm_id: 7,
            osm_tags: vec![],
            osm_geometry_wkb: encode_point(8.5, 47.2),
        };
        let lines = row
            .to_detail_geojson_lines(&osm)
            .expect("to_detail_geojson_lines");
        assert_eq!(
            lines.len(),
            2,
            "expected only atp+osm, no link, for a coincident match: {lines:?}"
        );
        let features: Vec<Value> = lines.iter().map(|l| parse(l)).collect();
        assert!(features.iter().all(|f| f["properties"]["part"] != "link"));
    }
}
