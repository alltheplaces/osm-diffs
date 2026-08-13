use anyhow::{Ok, Result};
use assert_cmd::{Command, cargo_bin};
use std::path::PathBuf;
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
