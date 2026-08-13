use anyhow::{Context, Ok, Result};
use std::{
    path::Path,
    time::{Instant, SystemTime},
};
use time::UtcDateTime;

/// Target chunk size, in bytes, for external sorts that spill to disk
/// across this pipeline (see the `ext_sort` crate). Chunk sizes for
/// external sorts have grown ad hoc over time and are currently uneven
/// across call sites for no good reason; this constant exists so new
/// code has a shared default to reach for instead of picking another
/// arbitrary number. Not yet applied to existing call sites -- see
/// https://github.com/alltheplaces/osm-diffs/issues/657 for that cleanup.
pub(crate) const EXTERNAL_SORT_CHUNK_BYTES: usize = 512 * 1024 * 1024;

mod conflate;
mod edits;
mod geostats; // TODO: Move into crate::geometry?
mod osm;

// Only these three re-exported crate-wide (rather than making all of
// `osm` pub(crate)): crate::provenance needs them to assemble this
// pipeline's provenance BOM, nothing outside `pipeline` needs the rest
// of osm's API (BlobReader, Node/Way/Relation, import_osm, ...).
pub(crate) use osm::{OsmMetadata, PLANET_PBF_FILENAME, read_cached_metadata};
mod tiles;
mod upload;

pub fn run_pipeline(
    http_client: &reqwest::Client,
    workdir: &Path,
    pipeline_run_id: &str,
) -> Result<()> {
    // Captured before anything else runs, so it's a genuine start
    // time for this invocation -- embedded into the provenance BOM
    // (crate::provenance) as formulation[].workflows[].timeStart.
    let pipeline_start_time = UtcDateTime::now();

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
            pipeline_run_id,
            pipeline_start_time,
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
/// snapshot and the step's wall-clock run time -- to help diagnose
/// out-of-memory kills and resource misconfigurations, since each step
/// here (import, build tables, conflate, ...) tends to be where a
/// memory or time blowup would show up first. Logging happens whether
/// the step succeeds or fails, so a step that errors out (e.g. due to
/// an actual OOM) still gets its "end" snapshot, with an elapsed time,
/// on the way out.
///
/// A step that returns `Err` also gets its error logged here, at ERROR
/// level, with the full `anyhow` context chain -- this is the single
/// place that does so, rather than every fallible call site logging its
/// own error on top of returning it. Without this, an error would only
/// ever surface via `main()`'s default unwind on the way out of the
/// process, never in `pipeline.log` itself.
fn run_step<T>(name: &str, step: impl FnOnce() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    log_snapshot(name, "start", None);
    let result = step();
    log_snapshot(name, "end", Some(start.elapsed().as_secs_f64()));
    if let Err(e) = &result {
        log::error!(step = name; "{name} failed: {e:#}");
    }
    result
}

/// Above this fraction of the cgroup memory limit, [`log_snapshot`] logs
/// a warning in addition to its usual info-level snapshot -- an early
/// signal ahead of an actual OOM-kill, which the kernel triggers once
/// usage reaches 1.0. 85% leaves a little margin for noise while still
/// catching a real problem before it turns into a kill.
const CGROUP_WARN_THRESHOLD: f64 = 0.85;

/// Logs one [`crate::memstats`] snapshot for a pipeline step, as
/// structured fields (see `crate::logging`) rather than baked into the
/// message text, so a log consumer can query/aggregate on them directly
/// (e.g. `jq '.fields.elapsed_seconds'`) instead of parsing a string.
/// `elapsed_seconds` is `None` for the "start" record -- there's no
/// elapsed time to report yet -- and the step's wall-clock run time for
/// "end".
fn log_snapshot(name: &str, phase: &str, elapsed_seconds: Option<f64>) {
    let stats = crate::memstats::snapshot();
    log::info!(
        step = name,
        phase = phase,
        elapsed_seconds = elapsed_seconds,
        rss_bytes = stats.rss_bytes,
        rss_peak_bytes = stats.rss_peak_bytes,
        rss_anon_bytes = stats.rss_anon_bytes,
        rss_file_bytes = stats.rss_file_bytes,
        rss_shmem_bytes = stats.rss_shmem_bytes,
        cgroup_current_bytes = stats.cgroup_current_bytes,
        cgroup_max_bytes = stats.cgroup_max_bytes,
        cgroup_peak_bytes = stats.cgroup_peak_bytes;
        "{name}: {phase}"
    );
    if let Some(fraction) = stats.cgroup_usage_fraction()
        && fraction >= CGROUP_WARN_THRESHOLD
    {
        log::warn!(
            step = name,
            phase = phase,
            cgroup_usage_fraction = fraction,
            cgroup_current_bytes = stats.cgroup_current_bytes,
            cgroup_max_bytes = stats.cgroup_max_bytes;
            "{name}: {phase}: memory usage at {:.0}% of the cgroup limit \
             -- at risk of being OOM-killed",
            fraction * 100.0,
        );
    }
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
