use anyhow::{Context, Ok, Result};
use std::{
    io::Write,
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

pub fn run_pipeline(http_client: &reqwest::Client, workdir: &Path, raw_run_id: &str) -> Result<()> {
    // Fallback identifier for this invocation, used only for
    // `pipeline.log`'s upload key when no `--run_id` was given (a
    // local/dev run). Everything that must be reproducible is anchored to
    // the inputs instead (see `pipeline::provenance`).
    let pipeline_start_time = UtcDateTime::now();

    // `--run_id` is scheduler-supplied and could contain anything;
    // normalise it once, here, to a filesystem/URL-safe form and use
    // *that* everywhere -- the log object key, the `workdir/run_id`
    // sentinel, and the provenance BOM's workflow `uid` -- so the three
    // always agree. See [`run_id_slug`].
    let pipeline_run_id = run_id_slug(raw_run_id);
    let pipeline_run_id = pipeline_run_id.as_str();

    if !workdir.exists() {
        std::fs::create_dir(workdir)?;
    }
    logging::init(workdir)?;

    // Before any step runs: pin this run's identity to the workdir, so a
    // restart can't quietly resume a workdir that belongs to a different
    // run. Returns early (nothing to upload) on a mismatch.
    reconcile_run_id(workdir, pipeline_run_id)?;

    let progress = indicatif::MultiProgress::new();
    let result = run_pipeline_steps(http_client, workdir, pipeline_run_id, &progress);

    // Upload the log regardless of whether the run above succeeded --
    // a failed run's log is exactly the one you want archived for
    // debugging, not just a successful one's. `run_step` (below) has
    // already logged whichever step failed and why, so that's already
    // in the uploaded file; this just adds one more line making the
    // overall outcome obvious without having to find that line first.
    if let Err(e) = &result {
        log::error!("pipeline run failed: {e:#}");
    }
    if let Err(upload_err) =
        upload::upload_logs(workdir, pipeline_run_id, pipeline_start_time, &progress)
    {
        log::error!("failed to upload pipeline.log: {upload_err:#}");
    }

    result
}

/// A filesystem- and URL-safe rendering of `--run_id`, used verbatim as
/// this run's identity everywhere it's written down: `pipeline.log`'s
/// object key, the `workdir/run_id` sentinel, and the provenance BOM's
/// workflow `uid`.
///
/// Empty stays empty (a local run with no `--run_id`). Otherwise every
/// maximal run of characters outside `[A-Za-z0-9._-]` collapses to a
/// single `_`, and leading/trailing `_ . -` are trimmed. If any of that
/// changed the string (or it collapsed to nothing), an 8-hex-character
/// SHA-256 prefix of the *original* is appended -- so two distinct
/// `--run_id`s can never collide on the same key, while a value that was
/// already safe (a Kubernetes Job name, a UUID, a colon-free timestamp)
/// passes through untouched.
fn run_id_slug(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut s = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            s.push(ch);
        } else if !s.ends_with('_') {
            s.push('_');
        }
    }
    let trimmed = s.trim_matches(|c| matches!(c, '_' | '.' | '-'));
    if trimmed == raw {
        return trimmed.to_string();
    }
    let hash = &crate::utils::sha256_hex(raw.as_bytes())[..8];
    if trimmed.is_empty() {
        hash.to_string()
    } else {
        format!("{trimmed}-{hash}")
    }
}

/// Filename, in the workdir, of the run-ID sentinel [`reconcile_run_id`]
/// writes and checks.
const RUN_ID_FILENAME: &str = "run_id";

/// Reconciles `run_id` (already normalised by [`run_id_slug`]) with
/// `workdir/run_id`, so a restarted job can't quietly resume a workdir
/// that belongs to a different run.
///
/// - `run_id` empty (a local/interactive run): nothing to pin, and
///   re-running in place is expected -- do nothing.
/// - `workdir/run_id` absent: written (temp file, flushed, atomically
///   renamed), marking the workdir as this run's.
/// - present and equal: a restart of the same run -- proceed, reusing
///   whatever completed steps already left behind.
/// - present and different: refuse, rather than mix two runs'
///   intermediate files in one workdir.
///
/// Kubernetes ephemeral CSI volumes start empty, so a first start always
/// takes the "absent" branch and a same-volume restart takes "equal".
fn reconcile_run_id(workdir: &Path, run_id: &str) -> Result<()> {
    if run_id.is_empty() {
        return Ok(());
    }
    let path = workdir.join(RUN_ID_FILENAME);
    match std::fs::read_to_string(&path) {
        std::result::Result::Ok(existing) if existing == run_id => Ok(()),
        std::result::Result::Ok(existing) => anyhow::bail!(
            "workdir {} belongs to run {existing:?}, but --run_id is {run_id:?}; \
             use a fresh workdir",
            workdir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let tmp = path.with_extension("tmp");
            let mut file = std::fs::File::create(&tmp)
                .with_context(|| format!("failed to create {}", tmp.display()))?;
            file.write_all(run_id.as_bytes())
                .with_context(|| format!("failed to write {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("failed to flush {}", tmp.display()))?;
            drop(file);
            std::fs::rename(&tmp, &path).with_context(|| {
                format!("failed to rename {} to {}", tmp.display(), path.display())
            })?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn run_pipeline_steps(
    http_client: &reqwest::Client,
    workdir: &Path,
    pipeline_run_id: &str,
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
            conflate::conflate(&atp, &osm_features, progress, workdir, pipeline_run_id)
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
    //
    // conflated.pmtiles itself is built in two zoom-bounded passes,
    // joined together, rather than one -zg/auto-zoom build: an
    // overview pass (one minimal feature per row -- no tags -- up to
    // DETAIL_MIN_ZOOM - 1) and a detail pass (full-tag features,
    // DETAIL_MIN_ZOOM..DETAIL_MAX_ZOOM; matched rows up to three each,
    // unmatched rows one). See conflated_tiles::DETAIL_MAX_ZOOM's doc
    // comment for why the detail pass can't use tippecanoe's automatic
    // zoom selection the way the overview pass (and diffed-places.pmtiles,
    // below) safely can, and the conflated_tiles module doc comment for
    // why the overview carries no tags. Both passes' input layers come
    // from one extract_conflated_layers scan, not two.
    let conflated_layers = run_step("extract_conflated_layers", || {
        conflated_tiles::extract_conflated_layers(&conflated, progress, workdir)
    })?;
    let conflated_overview = run_step("render_conflated_overview", || {
        tiles::render_tiles(
            &[
                conflated_layers.overview_matched.clone(),
                conflated_layers.overview_unmatched.clone(),
            ],
            progress,
            workdir,
            "conflated-overview.pmtiles",
            tiles::ZoomRange::Bounded {
                min: 0,
                max: conflated_tiles::DETAIL_MIN_ZOOM - 1,
            },
        )
    })?;
    let conflated_detail = run_step("render_conflated_detail", || {
        tiles::render_tiles(
            &[
                conflated_layers.detail_matched.clone(),
                conflated_layers.detail_unmatched.clone(),
            ],
            progress,
            workdir,
            "conflated-detail.pmtiles",
            tiles::ZoomRange::Bounded {
                min: conflated_tiles::DETAIL_MIN_ZOOM,
                max: conflated_tiles::DETAIL_MAX_ZOOM,
            },
        )
    })?;
    let conflated_tiles_out = run_step("join_conflated_tiles", || {
        tiles::join_tiles(
            &[conflated_overview, conflated_detail],
            workdir,
            "conflated.pmtiles",
        )
    })?;
    run_step("upload_conflated_tiles", || {
        upload::upload_conflated_tiles(&conflated_tiles_out, progress)
    })?;

    let edits = run_step("suggest_edits", || {
        edits::suggest_edits(&conflated, progress, workdir)
    })?;
    let tiles = run_step("render_tiles", || {
        tiles::render_tiles(
            &edits,
            progress,
            workdir,
            "diffed-places.pmtiles",
            tiles::ZoomRange::Auto,
        )
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

    #[test]
    fn test_reconcile_run_id() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let wd = dir.path();
        let sentinel = wd.join(RUN_ID_FILENAME);

        // Empty --run_id: a no-op, writes nothing.
        reconcile_run_id(wd, "")?;
        assert!(!sentinel.exists());

        // First run with an id: writes the sentinel.
        reconcile_run_id(wd, "job-abc.1")?;
        assert_eq!(std::fs::read_to_string(&sentinel)?, "job-abc.1");

        // Restart of the same run: fine, sentinel unchanged.
        reconcile_run_id(wd, "job-abc.1")?;
        assert_eq!(std::fs::read_to_string(&sentinel)?, "job-abc.1");

        // A different run against the same workdir: refused.
        let err = reconcile_run_id(wd, "job-xyz").unwrap_err().to_string();
        assert!(
            err.contains("job-abc.1") && err.contains("job-xyz"),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn test_run_id_slug() {
        // Empty stays empty (a local run).
        assert_eq!(run_id_slug(""), "");

        // Already-safe values pass straight through.
        for safe in ["osmdiffs-28912345", "2026-09-09-14-30-00", "a.b_c-1"] {
            assert_eq!(run_id_slug(safe), safe);
        }

        // Unsafe characters collapse to a single '_', and a hash of the
        // original is appended so distinct inputs can't collide.
        let a = run_id_slug("2026-09-09T14:30:00+00:00");
        assert!(a.starts_with("2026-09-09T14_30_00_00_00-"), "{a}");
        assert_eq!(run_id_slug("run/1"), run_id_slug("run/1")); // deterministic
        assert_ne!(run_id_slug("run/1"), run_id_slug("run:1")); // no collision
        assert!(run_id_slug("run/1").starts_with("run_1-"));

        // All-unsafe input still yields a usable, non-empty key.
        let h = run_id_slug("///");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
