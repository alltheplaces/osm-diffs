use anyhow::{Context, Ok, Result};
use std::{path::Path, time::SystemTime};

mod conflate;
mod edits;
mod geostats; // TODO: Move into crate::geometry?
mod osm;
mod tiles;
mod upload;

pub fn run_pipeline(http_client: &reqwest::Client, workdir: &Path) -> Result<()> {
    if !workdir.exists() {
        std::fs::create_dir(workdir)?;
    }
    crate::logging::init(workdir)?;

    geostats::init()?;
    let progress = indicatif::MultiProgress::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let atp = run_step("import_atp", || {
        runtime.block_on(crate::atp::import_atp(http_client, &progress, workdir))
    })?;
    let coverage = run_step("build_coverage", || {
        crate::coverage::build_coverage(&atp, &progress, workdir)
    })?;
    let (osm_parquet, osm_store) = run_step("import_osm", || {
        osm::import_osm(&coverage, &progress, workdir)
    })?;
    let _conflated = run_step("conflate", || {
        conflate::conflate(
            &atp,
            &coverage,
            &osm_parquet,
            &*osm_store,
            &progress,
            workdir,
        )
    })?;
    let edits = run_step("suggest_edits", || {
        edits::suggest_edits(&coverage, &atp, &osm_parquet, &progress, workdir)
    })?;
    let tiles = run_step("render_tiles", || {
        tiles::render_tiles(&edits, &progress, workdir)
    })?;
    run_step("upload_tiles", || upload::upload_tiles(&tiles, &progress))?;

    Ok(())
}

/// Runs one top-level pipeline step, logging a [`crate::memstats`]
/// snapshot before and after it -- to help diagnose out-of-memory kills
/// and resource misconfigurations, since each step here (import, build
/// tables, conflate, ...) tends to be where a memory blowup would show
/// up first. Logging happens whether the step succeeds or fails, so a
/// step that errors out (e.g. due to an actual OOM) still gets its
/// "end" snapshot on the way out.
fn run_step<T>(name: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
    log::info!("{name}: start {}", crate::memstats::snapshot());
    let result = step();
    log::info!("{name}: end {}", crate::memstats::snapshot());
    result
}

/// Returns the highest (most recent) last modification time among all paths.
/// If any path does not exist or cannot be accessed, an error is returned.
fn last_modified(paths: &[&Path]) -> Result<SystemTime> {
    if !paths.is_empty() {
        let mut last = modified(paths[0])?;
        for path in &paths[1..] {
            last = last.max(modified(path)?);
        }
        Ok(last)
    } else {
        anyhow::bail!("paths should not be empty")
    }
}

fn modified(path: &Path) -> Result<SystemTime> {
    std::fs::metadata(path)
        .with_context(|| format!("Failed to get metadata for path: {:?}", path))?
        .modified()
        .with_context(|| format!("Failed to get modification time for path: {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ops::Add, time::Duration};
    use tempfile::NamedTempFile;

    #[test]
    fn test_last_modified() -> Result<()> {
        let f0 = NamedTempFile::new()?;
        let f2 = NamedTempFile::new()?;
        let f7 = NamedTempFile::new()?;

        let t0 = SystemTime::now();
        let t2 = t0.add(Duration::new(2, 0));
        let t7 = t0.add(Duration::new(7, 0));

        f0.as_file().set_modified(t0)?;
        f2.as_file().set_modified(t2)?;
        f7.as_file().set_modified(t7)?;

        assert!(last_modified(&[]).is_err());
        assert!(last_modified(&[Path::new("/no/such/file")]).is_err());

        assert_eq!(last_modified(&[f0.path()])?, t0);
        assert_eq!(last_modified(&[f0.path(), f2.path()])?, t2);
        assert_eq!(last_modified(&[f2.path(), f0.path()])?, t2);
        assert_eq!(last_modified(&[f0.path(), f2.path(), f7.path()])?, t7);
        assert_eq!(last_modified(&[f0.path(), f7.path(), f2.path()])?, t7);
        assert_eq!(last_modified(&[f7.path(), f2.path(), f0.path()])?, t7);

        Ok(())
    }

    #[test]
    fn test_run_step_returns_the_closures_value() -> Result<()> {
        let value = run_step("test-step", || Ok(42))?;
        assert_eq!(value, 42);
        Ok(())
    }

    #[test]
    fn test_run_step_propagates_the_closures_error() {
        let result: Result<()> = run_step("test-step", || anyhow::bail!("boom"));
        assert!(result.is_err());
    }
}
