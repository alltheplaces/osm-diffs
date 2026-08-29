use anyhow::{Context, Ok, Result};
use assert_cmd::{Command, cargo_bin};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[test]
fn test_pipeline() -> Result<()> {
    use std::os::unix::fs::symlink;

    let workdir = TempDir::new()?;

    let mut atp = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    atp.push("tests/test_data/alltheplaces.zip");
    symlink(&atp, workdir.path().join("alltheplaces.zip"))?;

    // fetch_atp() requires the metadata sidecar alongside a pre-existing
    // alltheplaces.zip (see AtpMetadata in src/pipeline/atp/fetch.rs), so
    // it has to be symlinked in too, not just the zip itself.
    let mut atp_meta = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    atp_meta.push("tests/test_data/alltheplaces.meta.json");
    symlink(&atp_meta, workdir.path().join("alltheplaces.meta.json"))?;

    let mut osm = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    osm.push("tests/test_data/zugerland.osm.pbf");
    symlink(&osm, workdir.path().join("planet-latest.osm.pbf"))?;

    // fetch_planet() requires the metadata sidecar alongside a
    // pre-existing planet-latest.osm.pbf (see OsmMetadata in
    // src/pipeline/osm/mod.rs), analogous to alltheplaces.meta.json
    // above -- otherwise it would try to download a fresh copy from
    // OSM's torrent.
    let mut osm_meta = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    osm_meta.push("tests/test_data/planet-latest.osm.pbf.meta.json");
    symlink(
        &osm_meta,
        workdir.path().join("planet-latest.osm.pbf.meta.json"),
    )?;

    Command::new(cargo_bin!("osm-diffs"))
        .arg("run")
        .arg("--workdir")
        .arg(workdir.path())
        .assert()
        .success();

    assert_conflated_parquet(&workdir.path().join("conflated.parquet"))?;
    assert_shops_jsonl(&workdir.path().join("shops.jsonl"))?;
    assert_conflated_tile_layers(workdir.path())?;
    assert!(
        workdir.path().join("conflated.pmtiles").exists(),
        "conflated.pmtiles was not produced"
    );

    Ok(())
}

/// One decoded `osm` side of a `conflated.parquet` row -- just the
/// fields this test needs, not a general-purpose reader (this is a
/// black-box integration test; it can't reach `pipeline::edits`'s own
/// private column-extraction code, so this is a small, deliberately
/// narrow one of its own).
struct ConflatedOsmSide {
    id: u64,
    r#type: String,
    shop: Option<String>,
    is_polygon: bool,
    version: u32,
    modified_timestamp_millis: i64,
    way_members_count: usize,
    atp_spider: String,
    atp_fetched_millis: i64,
}

/// Checks `conflate()`'s output against the fixture data's known shops
/// (see `tests/test_data/zugerland.osm.pbf` / `alltheplaces.zip`).
fn assert_conflated_parquet(path: &Path) -> Result<()> {
    use arrow::array::{
        Array, BinaryArray, ListArray, MapArray, RecordBatch, StringArray, StructArray,
        TimestampMillisecondArray, UInt32Array, UInt64Array,
    };
    use geo_traits::to_geo::ToGeoGeometry;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).with_context(|| format!("could not open {path:?}"))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

    let get_child_struct = |s: &StructArray, name: &str| -> Result<StructArray> {
        Ok(s.column_by_name(name)
            .with_context(|| format!("missing field '{name}'"))?
            .as_any()
            .downcast_ref::<StructArray>()
            .with_context(|| format!("field '{name}' is not a struct"))?
            .clone())
    };

    let mut total_rows = 0;
    let mut matched = Vec::new();
    for batch in reader {
        let batch: RecordBatch = batch?;
        total_rows += batch.num_rows();

        let atp = batch
            .column_by_name("atp")
            .context("missing 'atp' column")?
            .as_any()
            .downcast_ref::<StructArray>()
            .context("'atp' is not a struct")?;
        let osm = batch
            .column_by_name("osm")
            .context("missing 'osm' column")?
            .as_any()
            .downcast_ref::<StructArray>()
            .context("'osm' is not a struct")?;
        // Top-level, not nested inside `osm` -- GeoParquet 2.0 requires
        // geometry columns to live at the schema root (see
        // `pipeline::conflate::writer::GEO_METADATA_KEY`'s doc comment).
        let osm_geometry = batch
            .column_by_name("osm_geometry")
            .context("missing 'osm_geometry' column")?
            .as_any()
            .downcast_ref::<BinaryArray>()
            .context("'osm_geometry' is not binary")?;
        for row in 0..batch.num_rows() {
            if osm.is_null(row) {
                continue;
            }
            let get = |name: &str| osm.column_by_name(name).context("missing field");
            let id = get("id")?
                .as_any()
                .downcast_ref::<UInt64Array>()
                .context("'id' is not UInt64")?
                .value(row);
            let r#type = get("type")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("'type' is not a string")?
                .value(row)
                .to_string();
            let wkb = osm_geometry.value(row);
            let is_polygon = matches!(
                wkb::reader::read_wkb(wkb)?.to_geometry(),
                geo::Geometry::Polygon(_)
            );
            let tags = get("tags")?
                .as_any()
                .downcast_ref::<MapArray>()
                .context("'tags' is not a map")?
                .value(row);
            let keys = tags
                .column_by_name("key")
                .context("map has no 'key' field")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("map keys are not strings")?;
            let values = tags
                .column_by_name("value")
                .context("map has no 'value' field")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("map values are not strings")?;
            let shop = (0..keys.len())
                .find(|&i| keys.value(i) == "shop")
                .map(|i| values.value(i).to_string());

            let modified = get_child_struct(osm, "modified")?;
            let version = modified
                .column_by_name("version")
                .context("modified has no 'version' field")?
                .as_any()
                .downcast_ref::<UInt32Array>()
                .context("'version' is not UInt32")?
                .value(row);
            let modified_timestamp_millis = modified
                .column_by_name("timestamp")
                .context("modified has no 'timestamp' field")?
                .as_any()
                .downcast_ref::<TimestampMillisecondArray>()
                .context("'timestamp' is not a millisecond timestamp")?
                .value(row);

            let way_members_count = osm
                .column_by_name("way_members")
                .context("missing 'way_members' field")?
                .as_any()
                .downcast_ref::<ListArray>()
                .context("'way_members' is not a list")?
                .value(row)
                .len();

            let fetched = get_child_struct(atp, "fetched")?;
            let atp_spider = fetched
                .column_by_name("spider")
                .context("fetched has no 'spider' field")?
                .as_any()
                .downcast_ref::<StringArray>()
                .context("'spider' is not a string")?
                .value(row)
                .to_string();
            let atp_fetched_millis = fetched
                .column_by_name("timestamp")
                .context("fetched has no 'timestamp' field")?
                .as_any()
                .downcast_ref::<TimestampMillisecondArray>()
                .context("'timestamp' is not a millisecond timestamp")?
                .value(row);

            matched.push(ConflatedOsmSide {
                id,
                r#type,
                shop,
                is_polygon,
                version,
                modified_timestamp_millis,
                way_members_count,
                atp_spider,
                atp_fetched_millis,
            });
        }
    }

    assert_eq!(
        total_rows, 6,
        "expected 6 conflated rows (matched + unmatched)"
    );
    assert_eq!(
        matched.len(),
        3,
        "expected 3 ATP features matched to an OSM feature"
    );

    // (osm_id, shop, version, modified timestamp (Unix seconds -- OSM's
    // own edit timestamps never carry sub-second precision), way_members
    // count, atp spider, atp fetched (Unix *milliseconds* -- unlike
    // modified, AllThePlaces' own spider:collection_time does carry
    // sub-second precision, e.g. Denner's below is really
    // ...952.804399 in the source GeoJSON, so this pins genuine
    // sub-second digits, not just a round number)) -- pinned via a real
    // run's output, cross-checked with DuckDB against the fixture data
    // directly, not just re-derived from this same code.
    for (osm_id, expected_shop, version, ts_secs, way_members, spider, fetched_millis) in [
        (
            608979139,
            "coffee",
            3,
            1674832913,
            5,
            "tchibo",
            1780317635860,
        ), // Tchibo
        (
            737021556,
            "electronics",
            10,
            1740858960,
            7,
            "mediamarkt",
            1779653420273,
        ), // MediaMarkt
        (
            737021557,
            "supermarket",
            5,
            1740858960,
            5,
            "denner_ch",
            1780209952804,
        ), // Denner
    ] {
        let row = matched
            .iter()
            .find(|r| r.id == osm_id)
            .unwrap_or_else(|| panic!("expected a conflated row for OSM way/{osm_id}"));
        assert_eq!(row.r#type, "way");
        assert_eq!(row.shop.as_deref(), Some(expected_shop));
        assert!(
            row.is_polygon,
            "expected way/{osm_id} to carry real polygon geometry, not a synthetic point"
        );
        assert_eq!(row.version, version, "way/{osm_id} osm.modified.version");
        assert_eq!(
            row.modified_timestamp_millis,
            ts_secs * 1000,
            "way/{osm_id} osm.modified.timestamp"
        );
        assert_eq!(
            row.way_members_count, way_members,
            "way/{osm_id} osm.way_members length"
        );
        assert_eq!(row.atp_spider, spider, "way/{osm_id} atp.fetched.spider");
        assert_eq!(
            row.atp_fetched_millis, fetched_millis,
            "way/{osm_id} atp.fetched.timestamp"
        );
    }

    Ok(())
}

/// Checks `suggest_edits`'s output against the fixture data's known
/// shops (see `tests/test_data/zugerland.osm.pbf` /
/// `alltheplaces.zip`) -- pins concrete expected values, not just "the
/// file has some content", so a regression in tag-diffing or GeoJSON
/// output actually gets caught, not just a crash.
fn assert_shops_jsonl(path: &Path) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("could not read {path:?}"))?;
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "expected 3 suggested shop edits, got:\n{content}"
    );

    let features: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect::<Result<_>>()?;

    // Every row is a well-formed GeoJSON Point feature, carrying the
    // OSM base version it was suggested against -- needed to detect
    // edit conflicts later (OSM's own edit API uses the version number,
    // not the changeset, for this), even though nothing uploads edits
    // yet. Deliberately no changeset here -- see alltheplaces/osm-diffs#730.
    for feature in &features {
        assert_eq!(feature["type"], "Feature");
        assert_eq!(feature["geometry"]["type"], "Point");
        assert!(feature["id"].is_number(), "feature has no id: {feature}");
        assert!(
            feature["properties"]["@osm_version"].is_number(),
            "feature has no @osm_version: {feature}"
        );
        assert!(
            feature["properties"].get("@osm_changeset").is_none(),
            "feature should not carry @osm_changeset: {feature}"
        );
    }

    // One specific, known edit: this OSM feature's opening_hours and
    // phone differ from AllThePlaces' in the fixture data.
    let edit = features
        .iter()
        .find(|f| f["id"] == 737021556)
        .expect("expected a suggested edit for OSM feature 737021556");
    assert_eq!(
        edit["properties"]["opening_hours"],
        "Mo-Th 09:00-19:00; Fr 09:00-21:00; Sa 08:00-17:00"
    );
    assert_eq!(edit["properties"]["phone"], "+41 848 544 455");
    // Only the tags that actually differ (and are on the trustworthy
    // allowlist -- see PoiEditSuggester) should be suggested, nothing else.
    assert_eq!(
        edit["properties"]
            .as_object()
            .expect("properties should be an object")
            .keys()
            .filter(|k| !k.starts_with('@'))
            .count(),
        2,
        "unexpected extra tags in suggested edit: {edit}"
    );

    Ok(())
}

/// Checks `extract_conflated_layers`'s four output layers against the
/// same fixture data `assert_conflated_parquet` checks: 3 matched + 3
/// unmatched ATP features (see `assert_conflated_parquet`'s own
/// "expected 6 conflated rows" / "expected 3 ATP features matched"
/// assertions above). The overview layers are deliberately minimal
/// (no tags); the detail layers carry the full tag set, keyed back to
/// the overview by `fid`.
fn assert_conflated_tile_layers(workdir: &Path) -> Result<()> {
    let read_features = |name: &str| -> Result<Vec<serde_json::Value>> {
        let path = workdir.join(name);
        let content =
            std::fs::read_to_string(&path).with_context(|| format!("could not read {path:?}"))?;
        content
            .lines()
            .map(|line| Ok(serde_json::from_str(line)?))
            .collect()
    };

    let overview_matched = read_features("matched.jsonl")?;
    let overview_unmatched = read_features("unmatched.jsonl")?;
    let detail_matched = read_features("matched-detail.jsonl")?;
    let detail_unmatched = read_features("unmatched-detail.jsonl")?;
    assert_eq!(
        overview_matched.len(),
        3,
        "expected 3 overview-matched features, got:\n{overview_matched:#?}"
    );
    assert_eq!(
        overview_unmatched.len(),
        3,
        "expected 3 overview-unmatched features, got:\n{overview_unmatched:#?}"
    );

    // Overview features are minimal: spider + matched, plus
    // osm:type/osm:id when matched, and *no* tags and *no* fid (every
    // low-zoom byte counts).
    for feature in overview_matched.iter().chain(&overview_unmatched) {
        assert_eq!(feature["type"], "Feature");
        assert!(
            feature["properties"].get("fid").is_none(),
            "overview feature must not carry fid: {feature}"
        );
        assert!(
            feature["properties"]["spider"].is_string(),
            "overview feature has no spider: {feature}"
        );
        let taglike: Vec<&String> = feature["properties"]
            .as_object()
            .expect("properties should be an object")
            .keys()
            .filter(|k| {
                (k.starts_with("atp:") || k.starts_with("osm:"))
                    && k.as_str() != "osm:type"
                    && k.as_str() != "osm:id"
            })
            .collect();
        assert!(
            taglike.is_empty(),
            "overview feature must carry no tags, found {taglike:?}: {feature}"
        );
    }
    for feature in &overview_matched {
        assert_eq!(feature["properties"]["matched"], true);
    }
    for feature in &overview_unmatched {
        assert_eq!(feature["properties"]["matched"], false);
        assert!(
            feature["properties"].get("osm:type").is_none(),
            "unmatched overview feature should carry no osm:* properties: {feature}"
        );
    }

    // The detail layers carry the tags the overview drops. Matched rows
    // contribute 2-3 features each (atp / osm / optional link); unmatched
    // rows one.
    assert!(
        detail_matched.len() >= 6 && detail_matched.len() <= 9,
        "expected 2-3 matched-detail features per matched row, got {}:\n{detail_matched:#?}",
        detail_matched.len()
    );
    assert_eq!(
        detail_unmatched.len(),
        3,
        "expected 3 unmatched-detail features, got:\n{detail_unmatched:#?}"
    );
    for feature in detail_matched.iter().chain(&detail_unmatched) {
        assert!(
            ["atp", "osm", "link"].contains(&feature["properties"]["part"].as_str().unwrap_or("")),
            "detail feature has no valid part: {feature}"
        );
        assert!(
            feature["properties"]["fid"].is_u64(),
            "detail feature has no fid: {feature}"
        );
    }

    // Same known match `assert_shops_jsonl` checks above: MediaMarkt,
    // matched to OSM way/737021556, a polygon. Its overview feature is
    // the bare OSM shape; the tags live on the detail features. The
    // overview carries no fid, so the join to detail is on osm:id (the
    // matched-feature join key a viewer would use too).
    let media_markt = overview_matched
        .iter()
        .find(|f| f["properties"]["osm:id"] == 737021556)
        .expect("expected an overview feature for OSM feature 737021556");
    assert_eq!(media_markt["properties"]["osm:type"], "way");
    assert_eq!(media_markt["geometry"]["type"], "Polygon");
    let media_markt_osm = detail_matched
        .iter()
        .find(|f| f["properties"]["osm:id"] == 737021556 && f["properties"]["part"] == "osm")
        .expect("expected a matched-detail osm feature for OSM way/737021556");
    let fid = &media_markt_osm["properties"]["fid"];
    assert!(
        fid.is_u64(),
        "detail feature must carry an fid: {media_markt_osm}"
    );
    // The atp side of the same match shares that fid.
    let media_markt_atp = detail_matched
        .iter()
        .find(|f| &f["properties"]["fid"] == fid && f["properties"]["part"] == "atp")
        .expect("expected the matched-detail atp feature sharing MediaMarkt's fid");
    assert_eq!(media_markt_atp["properties"]["atp:shop"], "electronics");

    Ok(())
}

#[test]
fn test_no_subcommand() {
    Command::new(cargo_bin!("osm-diffs"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("no subcommand given"));
}

#[test]
fn test_version_flag() {
    // Asserts against CARGO_PKG_VERSION (rather than a hardcoded string) so
    // this doesn't need updating every time cut-release.sh bumps the version.
    Command::new(cargo_bin!("osm-diffs"))
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_help_flag() {
    Command::new(cargo_bin!("osm-diffs"))
        .arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("run"))
        .stdout(predicates::str::contains("--version"));
}
