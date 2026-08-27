use anyhow::{Context, Ok, Result};
use std::{
    path::Path,
    time::{Instant, SystemTime},
};
use time::UtcDateTime;

/// Target chunk size, in bytes, for external sorts that spill to disk
/// across this pipeline (see the `ext_sort` crate) -- a single, named
/// knob to bump if a chunk size ever turns out wrong, instead of hunting
/// down ad hoc magic numbers scattered across call sites (see
/// https://github.com/alltheplaces/osm-diffs/issues/657, resolved by
/// consolidating every call site onto this constant). A chunk is also a
/// unit of concurrently-open file descriptors: `ext_sort` keeps every
/// spilled chunk's file open at once during its final merge, so a
/// smaller chunk size means more, smaller chunks and more open files for
/// the same input -- raise this if that ever runs into an OS file
/// descriptor limit again, as happened during the pr665 Hetzner shakedown.
///
/// This budget is shared among however many external sorts run
/// *concurrently*, not handed out per sorter: several call sites run
/// more than one external sort at the same time (e.g. `prune.rs`'s
/// per-stage producer/writer pipelines, each running a few sorts on
/// sibling threads within one `thread::scope` -- though not every thread
/// in such a scope runs a sort; some just move data between channels).
/// Each such call site divides this constant by however many sorts run
/// alongside it, so their combined peak memory stays within one
/// `EXTERNAL_SORT_CHUNK_BYTES` rather than multiplying it. A call site
/// that's the only external sort in its scope uses the full value.
pub(crate) const EXTERNAL_SORT_CHUNK_BYTES: usize = 512 * 1024 * 1024;

mod atp;
mod conflate;
mod conflated_tiles;
mod edits;
mod logging;
mod memstats;
mod osm;
mod provenance;

// Only these re-exported at the pipeline level (rather than making all
// of `atp`/`osm` pub(crate)): `provenance` needs them to assemble this
// pipeline's provenance BOM, nothing else needs the rest of atp's/osm's
// API (import_atp, BlobReader, Node/Way/Relation, import_osm, ...).
// atp's own `read_cached_metadata` is re-exported under a different
// name here since osm already has one of its own.
// decode_feature_id/osm_type_str are for pipeline::conflate::writer,
// which decodes the same `Feature.id` encoding osm::assemble/prune
// produce -- see decode_feature_id's own doc comment.
pub(crate) use atp::{AtpMetadata, read_cached_metadata as read_cached_atp_metadata};
pub(crate) use osm::{
    OsmMetadata, PLANET_PBF_FILENAME, decode_feature_id, osm_type_str, read_cached_metadata,
};
mod tiles;
mod upload;

pub fn run_pipeline(
    http_client: &reqwest::Client,
    workdir: &Path,
    pipeline_run_id: &str,
) -> Result<()> {
    // Captured before anything else runs, so it's a genuine start
    // time for this invocation -- embedded into the provenance BOM
    // (pipeline::provenance) as formulation[].workflows[].timeStart, and
    // reused below as pipeline.log's own upload key, so a run's log and
    // its data output can always be tied back together.
    let pipeline_start_time = UtcDateTime::now();

    if !workdir.exists() {
        std::fs::create_dir(workdir)?;
    }
    logging::init(workdir)?;

    let progress = indicatif::MultiProgress::new();
    let result = run_pipeline_steps(
        http_client,
        workdir,
        pipeline_run_id,
        pipeline_start_time,
        &progress,
    );

    // Upload the log regardless of whether the run above succeeded --
    // a failed run's log is exactly the one you want archived for
    // debugging, not just a successful one's. `run_step` (below) has
    // already logged whichever step failed and why, so that's already
    // in the uploaded file; this just adds one more line making the
    // overall outcome obvious without having to find that line first.
    if let Err(e) = &result {
        log::error!("pipeline run failed: {e:#}");
    }
    if let Err(upload_err) = upload::upload_logs(workdir, pipeline_start_time, &progress) {
        log::error!("failed to upload pipeline.log: {upload_err:#}");
    }

    result
}

fn run_pipeline_steps(
    http_client: &reqwest::Client,
    workdir: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
    progress: &indicatif::MultiProgress,
) -> Result<()> {
    crate::geometry::init_geospatial_stats()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let atp = run_step("import_atp", || {
        runtime.block_on(atp::import_atp(http_client, progress, workdir))
    })?;
    // Not consumed by anything yet -- see atp::collect_wikidata_ids for
    // what this is for.
    let _wikidata_ids = run_step("collect_wikidata_ids", || {
        atp::collect_wikidata_ids(&atp, workdir)
    })?;
    // `osm_features` is scoped to just these two steps, not bound at
    // this function's top level: it's an `OsmFeatureIndex`, holding the
    // pipeline's largest mmap'd tables (coordinates, features, the
    // inverted index -- tens of GB on a full-planet run). Nothing after
    // `conflate()` needs it, so it should drop -- unmapping those tables
    // -- right here, instead of pinning that address space for the rest
    // of the pipeline's steps (`suggest_edits`, `render_tiles`, and
    // beyond) for no reason.
    let conflated = {
        let osm_features = run_step("import_osm", || {
            osm::import_osm(http_client, progress, workdir)
        })?;
        run_step("conflate", || {
            conflate::conflate(
                &atp,
                &osm_features,
                progress,
                workdir,
                pipeline_run_id,
                pipeline_start_time,
            )
        })?
    };
    run_step("upload_conflated", || {
        upload::upload_conflated(&conflated, progress)
    })?;

    // Two independent branches off `conflated`, neither depending on
    // the other: `conflated.pmtiles` (every ATP feature, matched or
    // not -- see conflated_tiles's own doc comment) visualizes the
    // *matching* step itself, while `diffed-places.pmtiles` below
    // visualizes only what `suggest_edits` separately decided to
    // propose.
    let conflated_layers = run_step("extract_conflated_layers", || {
        conflated_tiles::extract_conflated_layers(&conflated, progress, workdir)
    })?;
    let conflated_tiles_out = run_step("render_conflated_tiles", || {
        tiles::render_tiles(&conflated_layers, progress, workdir, "conflated.pmtiles")
    })?;
    run_step("upload_conflated_tiles", || {
        upload::upload_conflated_tiles(&conflated_tiles_out, progress)
    })?;

    let edits = run_step("suggest_edits", || {
        edits::suggest_edits(&conflated, progress, workdir)
    })?;
    let tiles = run_step("render_tiles", || {
        tiles::render_tiles(&edits, progress, workdir, "diffed-places.pmtiles")
    })?;
    run_step("upload_tiles", || upload::upload_tiles(&tiles, progress))?;

    Ok(())
}

/// Runs one pipeline step, logging a [`memstats`]
/// snapshot and the step's wall-clock run time -- to help diagnose
/// out-of-memory kills and resource misconfigurations, since each step
/// here (import, build tables, conflate, ...) tends to be where a
/// memory or time blowup would show up first. Logging happens whether
/// the step succeeds or fails, so a step that errors out (e.g. due to
/// an actual OOM) still gets its "end" snapshot, with an elapsed time,
/// on the way out.
///
/// Not just for the top-level steps `run_pipeline_steps` itself calls
/// this with (`import_atp`, `conflate`, ...): `pipeline::osm::import_osm`
/// also calls this directly (via plain Rust module-tree visibility --
/// this function isn't `pub`, but `osm` is a descendant module of
/// `pipeline`, so it can see it) for its own internal sub-steps
/// (`import_osm.fetch`, `.open`, `.prune`, `.assemble`, `.index`),
/// dotted-named to stay visually distinct from top-level steps in
/// `pipeline.log` while sharing the exact same logging shape -- one
/// mechanism, not two, and no separate parsing convention needed for
/// sub-step timings. Added after a real case (#711) where only having
/// `import_osm`'s own start/end pair wasn't enough resolution to tell
/// which of its internal phases actually needed the memory a tight
/// `--mem-limit` run ran short on.
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

/// Logs one [`memstats`] snapshot for a pipeline step, as
/// structured fields (see `logging`) rather than baked into the
/// message text, so a log consumer can query/aggregate on them directly
/// (e.g. `jq '.fields.elapsed_seconds'`) instead of parsing a string.
/// `elapsed_seconds` is `None` for the "start" record -- there's no
/// elapsed time to report yet -- and the step's wall-clock run time for
/// "end".
fn log_snapshot(name: &str, phase: &str, elapsed_seconds: Option<f64>) {
    let stats = memstats::snapshot();
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
        cgroup_peak_bytes = stats.cgroup_peak_bytes,
        children_rss_peak_bytes = stats.children_rss_peak_bytes;
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
