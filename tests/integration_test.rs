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

    assert_shops_jsonl(&workdir.path().join("shops.jsonl"))?;

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
