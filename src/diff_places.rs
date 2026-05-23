use crate::places::PlaceIndex;
use crate::s2_util::MergedCellRanges;
use crate::{make_progress_bar, match_distance};
use anyhow::Result;
use indicatif::MultiProgress;
use rayon::prelude::*;
use s2::{cap::Cap, cell::Cell, cellid::CellID, region::RegionCoverer};
use std::path::{Path, PathBuf};

pub fn diff_places(
    _coverage: &Path,
    atp: &Path,
    osm: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PathBuf> {
    assert!(workdir.exists());

    let out_path = workdir.join("diff.parquet");
    if out_path.exists() {
        return Ok(out_path);
    }

    // TODO: Open out_path for writing.

    let coverer = RegionCoverer {
        max_cells: 16,
        min_level: 12,
        max_level: s2::cellid::MAX_LEVEL as u8,
        level_mod: 1,
    };
    let atp_places = PlaceIndex::open(atp, 1)?;
    let num_atp_places = atp_places.total_rows() as u64;
    let progress_bar = make_progress_bar(progress, "diff     ", num_atp_places, "features");
    let osm_places = PlaceIndex::open(osm, 32)?;
    atp_places.scan().par_bridge().try_for_each(|place| {
        let place = place?;
        let s2_cell = Cell::from(CellID(place.s2_cell_id));
        let radius = match_distance(&place.mask);
        let cap = Cap::from_center_chordangle(&s2_cell.center(), &radius);
        let covering = coverer.covering(&cap);
        for (lo, hi) in MergedCellRanges::new(covering) {
            for candidate in osm_places.query(lo..=hi, place.mask)? {
                let _candidate = candidate?;
                // TODO: Compute spatial distance between candidate and place.
                // TODO: Compute match score between candidate and place.
            }
        }
        progress_bar.inc(1);
        Ok::<(), anyhow::Error>(())
    })?;
    progress_bar.finish();
    Ok(out_path)
}
