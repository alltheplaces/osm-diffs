use anyhow::{Ok, Result};
use assert_cmd::{Command, cargo_bin};
use predicates;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn test_pipeline() -> Result<()> {
    use std::os::unix::fs::symlink;

    let workdir = TempDir::new()?;

    let mut atp = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    atp.push("tests/test_data/alltheplaces.zip");
    symlink(&atp, workdir.path().join("alltheplaces.zip"))?;

    let mut osm = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    osm.push("tests/test_data/zugerland.osm.pbf");
    symlink(&osm, workdir.path().join("osm-planet.pbf"))?;

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
