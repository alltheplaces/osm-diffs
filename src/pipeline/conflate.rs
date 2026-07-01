use super::last_modified;
use crate::{
    make_progress_bar,
    matchers::{create_matcher, match_distance},
    places::{Place, PlaceIndex},
    s2_util::MergedCellRanges,
};
use anyhow::{Ok, Result};
use deepsize::DeepSizeOf;
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use indicatif::{MultiProgress, ProgressBar};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

pub fn conflate(
    atp: &Path,
    coverage: &Path,
    osm: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PathBuf> {
    let input_modified = last_modified(&[atp, coverage, osm])?;
    let out_path = workdir.join("conflated.parquet");
    if out_path.exists() && last_modified(&[&out_path])? >= input_modified {
        return Ok(out_path);
    }

    let atp_index = PlaceIndex::open(atp, 1)?;
    let num_atp_features = atp_index.total_rows() as u64;
    let producer_progress =
        make_progress_bar(progress, "conflate.match", num_atp_features, "ATP features");
    let osm_index = PlaceIndex::open(osm, 32)?;

    let mut producer_result = Ok(());
    let mut consumer_result = Ok(());
    thread::scope(|s| {
        let (tx, rx) = sync_channel::<Row>(8192);
        s.spawn(|| {
            producer_result =
                produce_rows(atp_index.clone(), osm_index.clone(), producer_progress, tx);
        });
        s.spawn(|| {
            consumer_result = consume_rows(rx, progress, workdir);
        });
    });
    consumer_result?;
    producer_result?;

    Ok(out_path)
}

// A single row in the conflated parquet file.
#[derive(Debug, DeepSizeOf, Deserialize, Serialize)]
struct Row {
    /// Internal sort key. Intentionally not written to our output
    /// parquet file because we don’t want to expose S2 cells to
    /// external clients. For point geometries, this would not be a
    /// big issue, but the algorithm to compute a single S2 cell for
    /// polylines and polygons may change in the future. (At the
    /// moment, we take the centroid, but we should rather leave this
    /// to the S2 library; but the Rust version of S2 does not
    /// implement this yet). We still sort the output by S2 because
    /// spatial sorting gives better compression and higher query
    /// performance with geographic Parquet files.
    s2_cell_id: u64,

    osm_id: Option<NonZeroU64>,
    osm_changeset: Option<NonZeroU64>,
    osm_version: Option<NonZeroU32>,
    osm_tags: Vec<(String, String)>,

    atp_spider: Option<String>,
    atp_tags: Vec<(String, String)>,
}

impl Row {
    fn from_atp_feature(atp: &Place) -> Row {
        Row {
            s2_cell_id: atp.s2_cell_id,
            osm_id: None,
            osm_changeset: None,
            osm_version: None,
            osm_tags: Vec::with_capacity(0),
            atp_spider: Some(atp.source.clone()),
            atp_tags: atp.tags.clone(),
        }
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We do not need to look at osm_tags since OSM IDs are unique.
        self.s2_cell_id
            .cmp(&other.s2_cell_id)
            .then(self.osm_id.cmp(&other.osm_id))
            .then(self.atp_spider.cmp(&other.atp_spider))
            .then(self.atp_tags.cmp(&other.atp_tags))
    }
}

impl PartialOrd for Row {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Row {}

fn produce_rows(
    atp_index: Arc<PlaceIndex>,
    osm_index: Arc<PlaceIndex>,
    progress_bar: ProgressBar,
    out: SyncSender<Row>,
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

            let mut row = Row::from_atp_feature(atp);
            let mut best_score: f64 = 0.0;
            let covering = {
                let s2_cell = s2::cell::Cell::from(s2::cellid::CellID(atp.s2_cell_id));
                let center = s2_cell.center();
                let radius = match_distance(&atp.mask);
                let cap = s2::cap::Cap::from_center_chordangle(&center, &radius);
                coverer.covering(&cap)
            };
            for (lo, hi) in MergedCellRanges::new(covering) {
                let mut iter = osm_index.query(lo..=hi, atp.mask)?;
                let mut best_candidate: Option<&Place> = None;
                for candidate in &mut iter {
                    let candidate = candidate?;
                    let score = matcher.score(candidate);
                    if score > best_score {
                        best_candidate = Some(candidate);
                        best_score = score;
                    }
                }
                if let Some(osm) = best_candidate {
                    row.s2_cell_id = osm.s2_cell_id;
                    row.osm_id = osm.osm_id;
                    row.osm_changeset = osm.osm_changeset;
                    row.osm_version = osm.osm_version;
                    row.osm_tags = osm.tags.clone();
                }
            }

            // TODO: Once we support relations, always send rows,
            // even if we could not find a matching feature in OSM.
            // https://github.com/alltheplaces/osm-diffs/issues/187
            if row.osm_id.is_some() {
                out.send(row)?;
            }

            Ok(())
        })?;
    }

    progress_bar.finish();
    Ok(())
}

fn consume_rows(rows: Receiver<Row>, progress: &MultiProgress, workdir: &Path) -> Result<()> {
    let row_count = AtomicU64::new(0);
    let sorter: ExternalSorter<Row, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
            .build()?;
    let sorted = sorter.sort(rows.iter().map(|x| {
        row_count.fetch_add(1, Ordering::SeqCst);
        std::io::Result::Ok(x)
    }))?;
    let row_count = row_count.load(Ordering::SeqCst);
    let progress_bar = make_progress_bar(progress, "conflate.write", row_count, "parquet rows");
    for row in sorted {
        let _row = row?;
        progress_bar.inc(1);
        // println!("*** GIRAFFE {:?}", row?);
    }
    progress_bar.finish();
    Ok(())
}
