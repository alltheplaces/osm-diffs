use super::last_modified;
use crate::{
    make_progress_bar,
    matchers::{OsmCandidate, create_matcher, match_distance},
    pipeline::EXTERNAL_SORT_CHUNK_BYTES,
    places::{Place, PlaceReader},
    s2_util::MergedCellRanges,
    tables::{Feature, OsmFeatures},
};
use anyhow::{Context, Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use indicatif::{MultiProgress, ProgressBar};
use rayon::prelude::*;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};
use time::UtcDateTime;

/// How often [produce_rows] logs a progress/memory snapshot while
/// running -- a wall-clock interval, not a feature count, since what
/// matters here is watching RSS/page-cache behavior *over time* as this
/// repeatedly queries and decodes from `OsmFeatureIndex`'s mmap'd
/// tables, not RSS at a fixed fraction of progress.
const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(30);

mod writer;
use writer::{ParquetRow, ParquetWriter};

pub fn conflate(
    atp: &Path,
    osm: &OsmFeatures,
    progress: &MultiProgress,
    workdir: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
) -> Result<PathBuf> {
    let input_modified = last_modified(&[atp])?.max(osm.modified()?);
    let out_path = workdir.join("conflated.parquet");
    if out_path.exists() && last_modified(&[&out_path])? >= input_modified {
        return Ok(out_path);
    }

    let atp_reader = PlaceReader::open(atp)?;
    let num_atp_features = atp_reader.total_rows() as u64;
    let producer_progress =
        make_progress_bar(progress, "conflate.match", num_atp_features, "ATP features");
    let writer_progress = make_progress_bar(progress, "conflate.write", 0, "parquet rows");

    let mut producer_result = Ok(());
    let mut writer_result = Ok(());
    thread::scope(|s| {
        let (tx, rx) = sync_channel::<ParquetRow>(8192);
        s.spawn(|| {
            producer_result = produce_rows(&atp_reader, osm, producer_progress, tx);
        });
        s.spawn(|| {
            writer_result = write_conflated(
                rx,
                writer_progress,
                workdir,
                &out_path,
                pipeline_run_id,
                pipeline_start_time,
            );
        });
    });
    writer_result?;
    producer_result?;

    Ok(out_path)
}

#[allow(unused)]
struct ConflatedFeature {
    atp: Option<Place>,
    osm: Option<Feature>,
    // TODO: Signals for ranking.
}

fn produce_rows(
    atp_reader: &PlaceReader,
    osm: &OsmFeatures,
    progress_bar: ProgressBar,
    out: SyncSender<ParquetRow>,
) -> Result<()> {
    let coverer = s2::region::RegionCoverer {
        max_cells: 16,
        min_level: 12,
        max_level: s2::cellid::MAX_LEVEL as u8,
        level_mod: 1,
    };

    let start = Instant::now();
    let atp_features_processed = AtomicU64::new(0);
    let osm_candidates_decoded = AtomicU64::new(0);
    // Wall-clock timestamp (millis since `start`) of the last progress
    // log line, so concurrent workers can race on a compare_exchange to
    // decide who logs next -- occasionally missing or double-logging a
    // snapshot at the boundary is fine for a diagnostic like this.
    let last_progress_log_ms = AtomicU64::new(0);

    for batch in atp_reader.read_all()? {
        // Each batch is processed by the Rayon thread pool in parallel,
        // but the outer loop is sequential — so nearby places (within a
        // batch) always go to nearby workers, preserving spatial locality.
        batch?.par_iter().try_for_each(|atp| {
            progress_bar.inc(1);
            atp_features_processed.fetch_add(1, Ordering::Relaxed);
            maybe_log_progress(start, &last_progress_log_ms, || {
                (
                    atp_features_processed.load(Ordering::Relaxed),
                    osm_candidates_decoded.load(Ordering::Relaxed),
                )
            });

            let Some(matcher) = create_matcher(atp) else {
                return Ok(());
            };

            let mut conflated = ConflatedFeature {
                atp: Some(atp.deep_clone()),
                osm: None,
            };
            let mut best_score: f64 = 0.0;
            let covering = {
                let s2_cell = s2::cell::Cell::from(s2::cellid::CellID(atp.s2_cell_id));
                let center = s2_cell.center();
                let radius = match_distance(&atp.mask);
                let cap = s2::cap::Cap::from_center_chordangle(&center, &radius);
                coverer.covering(&cap)
            };
            for (lo, hi) in MergedCellRanges::new(covering) {
                for r in osm.index.query(lo..=hi, atp.mask) {
                    let feature = osm.index.get_feature(r)?;
                    osm_candidates_decoded.fetch_add(1, Ordering::Relaxed);
                    let candidate = OsmCandidate {
                        feature: &feature,
                        strings: &osm.strings,
                    };
                    let score = matcher.score_osm_candidate(&candidate);
                    if score > best_score {
                        best_score = score;
                        conflated.osm = Some(feature);
                    }
                }
            }

            let row = ParquetRow::from_conflated(conflated, &osm.strings)?;
            out.send(row)?;

            Ok(())
        })?;
    }

    progress_bar.finish();
    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        atp_features_processed = atp_features_processed.load(Ordering::Relaxed),
        osm_candidates_decoded = osm_candidates_decoded.load(Ordering::Relaxed);
        "conflate.match: done"
    );
    Ok(())
}

/// Logs a `conflate.match: progress` snapshot (elapsed time, the two
/// counts from `counts`, and a `crate::memstats` snapshot -- `rss_bytes`/
/// `rss_file_bytes` in particular, since that's where resident pages of
/// `OsmFeatureIndex`'s mmap'd tables are expected to show up) at most
/// once per [PROGRESS_LOG_INTERVAL], across however many parallel
/// callers race to call this. `counts` is a closure, not plain
/// arguments, so the (relaxed, so individually cheap) atomic loads to
/// build it only happen on the rare call that actually ends up logging.
fn maybe_log_progress(
    start: Instant,
    last_log_ms: &AtomicU64,
    counts: impl FnOnce() -> (u64, u64),
) {
    let now_ms = start.elapsed().as_millis() as u64;
    let last = last_log_ms.load(Ordering::Relaxed);
    let interval_ms = PROGRESS_LOG_INTERVAL.as_millis() as u64;
    if now_ms.saturating_sub(last) < interval_ms {
        return;
    }
    if last_log_ms
        .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // another thread already logged this interval
    }

    let (atp_features_processed, osm_candidates_decoded) = counts();
    let stats = crate::memstats::snapshot();
    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        atp_features_processed = atp_features_processed,
        osm_candidates_decoded = osm_candidates_decoded,
        rss_bytes = stats.rss_bytes,
        rss_peak_bytes = stats.rss_peak_bytes,
        rss_anon_bytes = stats.rss_anon_bytes,
        rss_file_bytes = stats.rss_file_bytes,
        rss_shmem_bytes = stats.rss_shmem_bytes,
        cgroup_current_bytes = stats.cgroup_current_bytes,
        cgroup_max_bytes = stats.cgroup_max_bytes,
        cgroup_peak_bytes = stats.cgroup_peak_bytes;
        "conflate.match: progress"
    );
}

fn write_conflated(
    cf: Receiver<ParquetRow>,
    progress: ProgressBar,
    workdir: &Path,
    out: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
) -> Result<()> {
    let start = Instant::now();
    let row_count = AtomicU64::new(0);
    // write_conflated's only caller (conflate()) doesn't run any other
    // external sort concurrently with it, so this gets the full
    // chunk-size budget; see EXTERNAL_SORT_CHUNK_BYTES.
    let sorter: ExternalSorter<ParquetRow, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(
                EXTERNAL_SORT_CHUNK_BYTES as u64,
            ))
            .build()?;
    let sorted = sorter.sort(cf.iter().map(|row| {
        progress.set_length(row_count.fetch_add(1, Ordering::SeqCst));
        std::io::Result::Ok(row)
    }))?;
    progress.set_length(row_count.load(Ordering::SeqCst));
    let provenance_bom = crate::provenance::build_bom_for_conflated_parquet(
        workdir,
        pipeline_run_id,
        pipeline_start_time,
    )
    .context("could not assemble provenance BOM")?
    .to_string();
    let mut writer =
        ParquetWriter::create(out, /* max_rows_per_group */ 200_000, &provenance_bom)?;
    for row in sorted {
        writer.write(row?)?;
        progress.inc(1);
    }
    writer.close()?;
    progress.finish();
    log::info!(
        elapsed_seconds = start.elapsed().as_secs_f64(),
        rows_written = row_count.load(Ordering::SeqCst),
        bytes_written = std::fs::metadata(out)?.len();
        "conflate.write: done"
    );
    Ok(())
}
