use super::last_modified;
use crate::{
    make_progress_bar,
    matchers::{OsmCandidate, create_matcher, match_distance},
    places::{Place, PlaceIndex},
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
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};
use time::UtcDateTime;

mod writer;
use writer::{ParquetRow, ParquetWriter};

#[allow(clippy::too_many_arguments)]
pub fn conflate(
    atp: &Path,
    coverage: &Path,
    osm: &OsmFeatures,
    progress: &MultiProgress,
    workdir: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
) -> Result<PathBuf> {
    let input_modified = last_modified(&[atp, coverage])?.max(osm.modified()?);
    let out_path = workdir.join("conflated.parquet");
    if out_path.exists() && last_modified(&[&out_path])? >= input_modified {
        return Ok(out_path);
    }

    let atp_index = PlaceIndex::open(atp, 1)?;
    let num_atp_features = atp_index.total_rows() as u64;
    let producer_progress =
        make_progress_bar(progress, "conflate.match", num_atp_features, "ATP features");
    let writer_progress = make_progress_bar(progress, "conflate.write", 0, "parquet rows");

    let mut producer_result = Ok(());
    let mut writer_result = Ok(());
    thread::scope(|s| {
        let (tx, rx) = sync_channel::<ParquetRow>(8192);
        s.spawn(|| {
            producer_result = produce_rows(atp_index.clone(), osm, producer_progress, tx);
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
    atp_index: Arc<PlaceIndex>,
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

    for group in atp_index.scan_row_groups() {
        // Each group is processed by the Rayon thread pool in parallel,
        // but the outer loop is sequential — so nearby places (within a
        // group) always go to nearby workers, preserving spatial locality.
        group?.par_iter().try_for_each(|atp| {
            progress_bar.inc(1);
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
    Ok(())
}

fn write_conflated(
    cf: Receiver<ParquetRow>,
    progress: ProgressBar,
    workdir: &Path,
    out: &Path,
    pipeline_run_id: &str,
    pipeline_start_time: UtcDateTime,
) -> Result<()> {
    let row_count = AtomicU64::new(0);
    let sorter: ExternalSorter<ParquetRow, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
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
    Ok(())
}
