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
    // alltheplaces.zip (see AtpMetadata in src/atp/fetch.rs), so it has to
    // be symlinked in too, not just the zip itself.
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
}

/// Checks `conflate()`'s output against the fixture data's known shops
/// (see `tests/test_data/zugerland.osm.pbf` / `alltheplaces.zip`) --
/// in particular that OSM features conflate with real polygon geometry,
/// not the synthetic points the old Place-based pipeline emitted (the
/// whole point of alltheplaces/osm-diffs#665).
fn assert_conflated_parquet(path: &Path) -> Result<()> {
    use arrow::array::{
        Array, BinaryArray, MapArray, RecordBatch, StringArray, StructArray, UInt64Array,
    };
    use geo_traits::to_geo::ToGeoGeometry;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let file = std::fs::File::open(path).with_context(|| format!("could not open {path:?}"))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

    let mut total_rows = 0;
    let mut matched = Vec::new();
    for batch in reader {
        let batch: RecordBatch = batch?;
        total_rows += batch.num_rows();

        let osm = batch
            .column_by_name("osm")
            .context("missing 'osm' column")?
            .as_any()
            .downcast_ref::<StructArray>()
            .context("'osm' is not a struct")?;
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
            let wkb = get("geometry")?
                .as_any()
                .downcast_ref::<BinaryArray>()
                .context("'geometry' is not binary")?
                .value(row);
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

            matched.push(ConflatedOsmSide {
                id,
                r#type,
                shop,
                is_polygon,
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

    for (osm_id, expected_shop) in [
        (608979139, "coffee"),      // Tchibo
        (737021556, "electronics"), // MediaMarkt
        (737021557, "supermarket"), // Denner
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
    // OSM base changeset/version it was suggested against -- needed to
    // detect edit conflicts later, even though nothing uploads edits
    // yet.
    for feature in &features {
        assert_eq!(feature["type"], "Feature");
        assert_eq!(feature["geometry"]["type"], "Point");
        assert!(feature["id"].is_number(), "feature has no id: {feature}");
        assert!(
            feature["properties"]["@osm_changeset"].is_number(),
            "feature has no @osm_changeset: {feature}"
        );
        assert!(
            feature["properties"]["@osm_version"].is_number(),
            "feature has no @osm_version: {feature}"
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
