//! Turns `conflated.parquet` (produced by `crate::pipeline::conflate`)
//! into PMTiles-ready GeoJSON Lines tile layers, for visualizing the
//! *matching* step itself -- every AllThePlaces feature, matched or
//! not -- independent of whatever `crate::pipeline::edits` separately
//! decides to propose as an edit (see
//! [#709](https://github.com/alltheplaces/osm-diffs/issues/709)).
//!
//! Unlike `edits`, this doesn't need an external sort: every output
//! feature comes from exactly one input row (a row contributes one
//! overview feature and one-to-three detail features, never anything
//! merged across rows), so a straight sequential-batch,
//! rayon-parallel-per-batch scan is enough.
//!
//! [`extract_conflated_layers`] produces every layer
//! `conflated.pmtiles`' two-pass build needs (see
//! `pipeline::tiles::ZoomRange` and `pipeline::tiles::join_tiles`) in
//! one scan:
//!
//! - the coarse **overview** layers (`matched` / `unmatched`, one
//!   deliberately minimal feature per row -- `spider`, `matched`, and
//!   `osm:type`/`osm:id` when matched, but *no tags and no `fid`*): at
//!   z0 a single tile holds the entire planet, and every per-feature
//!   byte counts. Tags push the tile so far past tippecanoe's size
//!   limit that `--drop-densest-as-needed` guts it to a few hundred
//!   features worldwide; even `fid` alone (unique per feature, so it
//!   defeats the vector tile's columnar value dedup) costs ~20x the
//!   low-zoom density. Measured on a full planet: full tags -> ~370
//!   features visible at z0; `+fid` -> ~7k; overview as it stands ->
//!   ~40k, a workable world map.
//! - the high-zoom **detail** layers (`matched` / `unmatched`, z13+,
//!   where a tile covers a few km and per-feature bytes cost nothing):
//!   matched rows get up to three features each (see
//!   [`ConflatedTileRow::to_detail_geojson_lines`]), unmatched rows
//!   get one -- both carrying their full tag set (the tag inspection
//!   the overview can't afford) and `fid`, the row's ordinal in
//!   `conflated.parquet`.
//!
//! To go from a clicked overview feature to its full record: matched
//! features carry `osm:type`/`osm:id`, so fetch the detail tile for
//! that spot and match on those (or just open the OSM object). For
//! unmatched features -- which have no stable id, `conflated.parquet`
//! carries none for the ATP side -- match the nearest `part: "atp"`
//! detail feature of the same `spider`. See
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
/// `ConflatedLayers::overview_matched` and
/// `ConflatedLayers::detail_matched` are built from this same symbol,
/// so `tile-join` is guaranteed to merge them into one continuous
/// `matched` layer spanning the whole zoom range -- see
/// `DETAIL_MIN_ZOOM`'s doc comment for why that split exists at all.
/// If these ever named different strings, `tile-join` wouldn't error;
/// it would just silently produce two separately-toggleable layers
/// instead of one, so keeping this a single source of truth matters
/// more than it might look.
const MATCHED_LAYER_NAME: &str = "matched";

/// The tippecanoe layer name unmatched features belong to -- the
/// `unmatched` counterpart to [`MATCHED_LAYER_NAME`], and used for the
/// same reason: `ConflatedLayers::overview_unmatched` (z0..12) and
/// `ConflatedLayers::detail_unmatched` (z13+) both name it, so
/// `tile-join` stitches them into one continuous `unmatched` layer.
const UNMATCHED_LAYER_NAME: &str = "unmatched";

/// Zoom level at which `conflated.pmtiles`' detail pass (fed by
/// [`ConflatedLayers::detail_matched`] / [`ConflatedLayers::detail_unmatched`])
/// takes over from the coarse overview pass (fed by
/// [`ConflatedLayers::overview_matched`] / [`ConflatedLayers::overview_unmatched`]).
/// The overview pass is built up to `DETAIL_MIN_ZOOM - 1`; the detail
/// pass starts at `DETAIL_MIN_ZOOM`. Keep both call sites (`pipeline::mod`'s
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
    /// DETAIL_MIN_ZOOM, max: DETAIL_MAX_ZOOM }`): matched rows, up to
    /// three features each. Named `MATCHED_LAYER_NAME` -- the same
    /// layer name [`Self::overview_matched`] uses -- so `tile-join`
    /// merges them into one continuous layer; see that constant's doc
    /// comment.
    pub detail_matched: TileLayer,
    /// Fed to the detail pass together with [`Self::detail_matched`]:
    /// unmatched rows, one full-tag ATP point each. Named
    /// `UNMATCHED_LAYER_NAME`, matching [`Self::overview_unmatched`],
    /// for the same `tile-join` reason.
    pub detail_unmatched: TileLayer,
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
    /// The coarse overview layer's representation: one deliberately
    /// minimal feature per row. Geometry is the actual OSM shape when
    /// matched (what a reviewer is meant to review -- point/line/
    /// polygon, all valid within one GeoJSON/MVT layer), the raw
    /// ATP-provided shape otherwise.
    ///
    /// Properties are just `spider`, `matched`, and -- matched rows
    /// only -- `osm:type`/`osm:id`. **No tags, and no `fid`**: see this
    /// module's doc comment for why every low-zoom byte counts, and how
    /// a viewer gets from one of these features to the full record in
    /// the z13+ detail layer ([`Self::to_detail_geojson_lines`] /
    /// [`Self::to_unmatched_detail_geojson_line`]).
    ///
    /// TODO(#713): once conflated.parquet carries a per-row rank/
    /// importance signal, this is the natural place to also emit it
    /// as a feature property (or otherwise feed it to tippecanoe),
    /// so --drop-densest-as-needed can prefer important places over
    /// low-priority ones at low zoom instead of today's
    /// density-only heuristic.
    fn to_overview_geojson_line(&self) -> Result<String> {
        let geometry_wkb: &[u8] = match &self.osm {
            Some(osm) => &osm.osm_geometry_wkb,
            None => &self.atp_geometry_wkb,
        };
        let geometry = decode_and_round(geometry_wkb)?;

        let mut properties: Map<String, Value> = Map::new();
        properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        properties.insert("matched".to_string(), Value::Bool(self.osm.is_some()));
        if let Some(osm) = &self.osm {
            properties.insert("osm:type".to_string(), Value::String(osm.osm_type.clone()));
            properties.insert("osm:id".to_string(), Value::from(osm.osm_id));
        }
        geometry_to_geojson_line(&geometry, properties)
    }

    /// The detail layer's representation of a *matched* row -- see
    /// [`Self::to_unmatched_detail_geojson_line`] for the unmatched
    /// case. `osm` is passed explicitly, rather than read from
    /// `self.osm`, so that precondition is enforced by the type system
    /// instead of an internal `Option::unwrap`.
    ///
    /// Yields the ATP point and the OSM shape as two separate
    /// features -- each carrying only its own side's tags (`atp:*` or
    /// `osm:*`), so a tile inspector shows which end is which -- plus
    /// a connector line between their centroids, unless that line
    /// would be shorter than `MIN_CONNECTOR_LENGTH_METERS` (see that
    /// constant's doc comment for why). Every feature carries `fid`
    /// (this row's ordinal in `conflated.parquet` -- shared by all of a
    /// row's detail features, so a viewer can group the up-to-three
    /// back together, and a cross-reference into `conflated.parquet`)
    /// and a `part` property (`"atp"` | `"osm"` | `"link"`) identifying
    /// its role; the connector line -- the only feature that isn't
    /// clearly one side or the other -- keeps both tag sets.
    fn to_detail_geojson_lines(&self, osm: &OsmSide, fid: u64) -> Result<Vec<String>> {
        let atp_geometry = decode_and_round(&self.atp_geometry_wkb)?;
        let osm_geometry = decode_and_round(&osm.osm_geometry_wkb)?;

        let mut atp_properties: Map<String, Value> = Map::new();
        atp_properties.insert("fid".to_string(), Value::from(fid));
        atp_properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        atp_properties.insert("part".to_string(), Value::String("atp".to_string()));
        for (k, v) in &self.atp_tags {
            atp_properties.insert(format!("atp:{k}"), Value::String(v.clone()));
        }

        let mut osm_properties: Map<String, Value> = Map::new();
        osm_properties.insert("fid".to_string(), Value::from(fid));
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
            link_properties.insert("fid".to_string(), Value::from(fid));
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

    /// The detail layer's representation of an *unmatched* row: a
    /// single ATP point carrying its full `atp:*` tag set -- the tag
    /// inspection the minimal overview layer deliberately can't afford
    /// (see [`Self::to_overview_geojson_line`]), reachable one zoom
    /// step in. `part` is `"atp"`, matching the matched detail's ATP
    /// feature; `fid` is this row's ordinal in `conflated.parquet`.
    /// The clicked overview feature carries no id, so a viewer finds
    /// this one by nearest-of-same-`spider` -- see the module doc.
    fn to_unmatched_detail_geojson_line(&self, fid: u64) -> Result<String> {
        let geometry = decode_and_round(&self.atp_geometry_wkb)?;

        let mut properties: Map<String, Value> = Map::new();
        properties.insert("fid".to_string(), Value::from(fid));
        properties.insert("spider".to_string(), Value::String(self.spider.clone()));
        properties.insert("part".to_string(), Value::String("atp".to_string()));
        for (k, v) in &self.atp_tags {
            properties.insert(format!("atp:{k}"), Value::String(v.clone()));
        }
        geometry_to_geojson_line(&geometry, properties)
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
        detail_matched: TileLayer {
            name: String::from(MATCHED_LAYER_NAME),
            path: workdir.join("matched-detail.jsonl"),
        },
        detail_unmatched: TileLayer {
            name: String::from(UNMATCHED_LAYER_NAME),
            path: workdir.join("unmatched-detail.jsonl"),
        },
    };
    let all_layers = [
        &layers.overview_matched,
        &layers.overview_unmatched,
        &layers.detail_matched,
        &layers.detail_unmatched,
    ];
    if all_layers.iter().all(|layer| layer.path.exists()) {
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

    // Each layer is written to a sibling `.tmp` file, then renamed into
    // place once the whole scan succeeds -- so a crash mid-scan can't
    // leave a half-written layer that the memoization check above would
    // then trust.
    let open_tmp = |layer: &TileLayer| -> Result<(PathBuf, BufWriter<File>)> {
        let mut tmp = PathBuf::from(&layer.path);
        tmp.add_extension("tmp");
        let writer = BufWriter::with_capacity(32768, File::create(&tmp)?);
        Ok((tmp, writer))
    };
    let (overview_matched_tmp, mut overview_matched_w) = open_tmp(&layers.overview_matched)?;
    let (overview_unmatched_tmp, mut overview_unmatched_w) = open_tmp(&layers.overview_unmatched)?;
    let (detail_matched_tmp, mut detail_matched_w) = open_tmp(&layers.detail_matched)?;
    let (detail_unmatched_tmp, mut detail_unmatched_w) = open_tmp(&layers.detail_unmatched)?;

    let counts = extract_rows(
        conflated,
        &progress_bar,
        Writers {
            overview_matched: &mut overview_matched_w,
            overview_unmatched: &mut overview_unmatched_w,
            detail_matched: &mut detail_matched_w,
            detail_unmatched: &mut detail_unmatched_w,
        },
    )?;

    for mut writer in [
        overview_matched_w,
        overview_unmatched_w,
        detail_matched_w,
        detail_unmatched_w,
    ] {
        writer.flush()?;
    }
    rename(&overview_matched_tmp, &layers.overview_matched.path)?;
    rename(&overview_unmatched_tmp, &layers.overview_unmatched.path)?;
    rename(&detail_matched_tmp, &layers.detail_matched.path)?;
    rename(&detail_unmatched_tmp, &layers.detail_unmatched.path)?;

    progress_bar.finish_with_message(format!(
        "{num_rows} conflated features → {} matched / {} unmatched rows, \
         {} matched-detail / {} unmatched-detail features",
        counts.matched,
        counts.unmatched,
        counts.matched_detail_features,
        counts.unmatched_detail_features
    ));

    Ok(layers)
}

/// The four sinks [`extract_rows`] writes to, one per [`ConflatedLayers`]
/// field -- a struct rather than four positional `&mut impl Write`
/// parameters so a call site can't transpose two of them.
struct Writers<'a> {
    overview_matched: &'a mut dyn Write,
    overview_unmatched: &'a mut dyn Write,
    detail_matched: &'a mut dyn Write,
    detail_unmatched: &'a mut dyn Write,
}

#[derive(Default)]
struct RowCounts {
    matched: u64,
    unmatched: u64,
    matched_detail_features: u64,
    unmatched_detail_features: u64,
}

fn extract_rows(
    conflated: &Path,
    progress_bar: &ProgressBar,
    writers: Writers,
) -> Result<RowCounts> {
    let start = Instant::now();
    let mut counts = RowCounts::default();

    let file = File::open(conflated)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    // `fid` is the row's ordinal in `conflated.parquet` -- carried onto
    // every detail feature (not the overview: see the module doc for
    // why every low-zoom byte counts) so a viewer can group a matched
    // row's up-to-three detail features and cross-reference the parquet.
    // Batches arrive in file order, so a running offset plus the
    // in-batch index reproduces that ordinal without a shared counter.
    let mut batch_offset: u64 = 0;
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
                let fid = batch_offset + row as u64;
                let Some(tile_row) = extract_conflated_tile_row(&batch, row)? else {
                    return Ok(None);
                };
                let overview_line = tile_row.to_overview_geojson_line()?;
                let detail_lines = match &tile_row.osm {
                    Some(osm) => tile_row.to_detail_geojson_lines(osm, fid)?,
                    None => vec![tile_row.to_unmatched_detail_geojson_line(fid)?],
                };
                Ok(Some((tile_row.osm.is_some(), overview_line, detail_lines)))
            })
            .collect::<Result<Vec<_>>>()?;
        batch_offset += batch.num_rows() as u64;

        for entry in rows {
            progress_bar.inc(1);
            let Some((matched, overview_line, detail_lines)) = entry else {
                continue;
            };
            if matched {
                writers
                    .overview_matched
                    .write_all(overview_line.as_bytes())?;
                counts.matched += 1;
                for line in detail_lines {
                    writers.detail_matched.write_all(line.as_bytes())?;
                    counts.matched_detail_features += 1;
                }
            } else {
                writers
                    .overview_unmatched
                    .write_all(overview_line.as_bytes())?;
                counts.unmatched += 1;
                for line in detail_lines {
                    writers.detail_unmatched.write_all(line.as_bytes())?;
                    counts.unmatched_detail_features += 1;
                }
            }
        }
    }

    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        matched = counts.matched,
        unmatched = counts.unmatched,
        matched_detail_features = counts.matched_detail_features,
        unmatched_detail_features = counts.unmatched_detail_features;
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
    fn memoized_layers_carry_the_right_tippecanoe_layer_names() {
        // Exercises the real extract_conflated_layers/ConflatedLayers
        // construction, not just the constants in isolation: pre-create
        // all four output files so the memoization check short-circuits
        // before ever touching `conflated` (a path that doesn't exist),
        // then confirm each overview layer and its detail counterpart
        // carry the identical tippecanoe layer name -- the whole reason
        // MATCHED_LAYER_NAME / UNMATCHED_LAYER_NAME exist: if a future
        // edit ever let a pair drift apart, tile-join wouldn't error, it
        // would just silently produce two half-zoom-range layers instead
        // of one continuous one.
        let workdir = TempDir::new().expect("tempdir");
        for name in [
            "matched.jsonl",
            "unmatched.jsonl",
            "matched-detail.jsonl",
            "unmatched-detail.jsonl",
        ] {
            std::fs::write(workdir.path().join(name), b"").expect("write placeholder");
        }

        let layers = extract_conflated_layers(
            Path::new("/no/such/conflated.parquet"),
            &hidden_progress(),
            workdir.path(),
        )
        .expect("memoized path should not touch `conflated` at all");

        assert_eq!(layers.overview_matched.name, layers.detail_matched.name);
        assert_eq!(layers.detail_matched.name, MATCHED_LAYER_NAME);
        assert_eq!(layers.overview_unmatched.name, layers.detail_unmatched.name);
        assert_eq!(layers.detail_unmatched.name, UNMATCHED_LAYER_NAME);
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
    fn overview_unmatched_row_is_the_atp_point_with_no_tags() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![("shop".to_string(), "bakery".to_string())],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None,
        };
        let value = parse(&row.to_overview_geojson_line().expect("overview line"));

        assert_eq!(value["type"], "Feature");
        assert_eq!(value["geometry"]["type"], "Point");
        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.5, 47.2])
        );
        assert_eq!(value["properties"]["spider"], "acme");
        assert_eq!(value["properties"]["matched"], false);
        // The overview carries no tags, and no fid -- every low-zoom
        // byte counts (see the module doc).
        assert!(value["properties"].get("fid").is_none());
        assert!(value["properties"].get("atp:shop").is_none());
        assert!(value["properties"].get("osm:type").is_none());
    }

    #[test]
    fn overview_matched_row_is_the_osm_shape_with_only_the_osm_id() {
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
        let value = parse(&row.to_overview_geojson_line().expect("overview line"));
        let props = &value["properties"];

        // The OSM shape, not the ATP one -- that's the whole point of
        // preferring it once matched.
        assert_eq!(
            value["geometry"]["coordinates"],
            Value::from(vec![8.50001, 47.20001])
        );
        assert_eq!(props["matched"], true);
        assert!(props.get("fid").is_none());
        // osm:type / osm:id are kept -- they double as the join key to
        // the detail layer and a click-through to the OSM object; every
        // other tag is not.
        assert_eq!(props["osm:id"], 42);
        assert_eq!(props["osm:type"], "node");
        assert!(props.get("osm:shop").is_none());
        assert!(props.get("atp:shop").is_none());
    }

    #[test]
    fn overview_geometry_coordinates_are_rounded_to_seven_decimal_places() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![],
            atp_geometry_wkb: encode_point(8.123_456_789, 47.987_654_321),
            osm: None,
        };
        let value = parse(&row.to_overview_geojson_line().expect("overview line"));

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
    fn matched_detail_features_carry_their_own_tags_and_a_shared_fid() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![("name".to_string(), "Acme ATP".to_string())],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None, // unused; osm side passed explicitly below
        };
        let osm = osm_side_far_from(8.5, 47.2);
        let lines = row
            .to_detail_geojson_lines(&osm, 55)
            .expect("to_detail_geojson_lines");
        let features: Vec<Value> = lines.iter().map(|l| parse(l)).collect();

        // Every detail feature shares the row's fid -- that's the join
        // key back to the clicked overview feature.
        assert!(
            features.iter().all(|f| f["properties"]["fid"] == 55),
            "every detail feature must carry fid=55: {features:?}"
        );

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
    fn matched_detail_omits_the_connector_when_offset_is_below_threshold() {
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
            .to_detail_geojson_lines(&osm, 1)
            .expect("to_detail_geojson_lines");
        assert_eq!(
            lines.len(),
            2,
            "expected only atp+osm, no link, for a coincident match: {lines:?}"
        );
        let features: Vec<Value> = lines.iter().map(|l| parse(l)).collect();
        assert!(features.iter().all(|f| f["properties"]["part"] != "link"));
    }

    #[test]
    fn unmatched_detail_is_a_single_full_tag_atp_point() {
        let row = ConflatedTileRow {
            spider: "acme".to_string(),
            atp_tags: vec![
                ("name".to_string(), "Acme Foods".to_string()),
                ("addr:city".to_string(), "Zürich".to_string()),
                ("opening_hours".to_string(), "24/7".to_string()),
            ],
            atp_geometry_wkb: encode_point(8.5, 47.2),
            osm: None,
        };
        let value = parse(
            &row.to_unmatched_detail_geojson_line(88)
                .expect("unmatched detail line"),
        );
        let props = &value["properties"];

        assert_eq!(value["geometry"]["type"], "Point");
        assert_eq!(props["fid"], 88);
        assert_eq!(props["spider"], "acme");
        assert_eq!(props["part"], "atp");
        // Full tags here -- not trimmed the way the overview would be:
        // z13+ tiles have the room, and this is the only place an
        // unmatched row's tags are inspectable at all.
        assert_eq!(props["atp:name"], "Acme Foods");
        assert_eq!(props["atp:addr:city"], "Zürich");
        assert_eq!(props["atp:opening_hours"], "24/7");
    }
}
