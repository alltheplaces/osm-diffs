use crate::places::{PlaceIter, S2RowGroupIndex};
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

    let coverer = RegionCoverer {
        max_cells: 16,
        min_level: 12,
        max_level: s2::cellid::MAX_LEVEL as u8,
        level_mod: 1,
    };

    // TODO: Implement.
    let atp_places = PlaceIter::try_new(atp)?;
    let row_group_index = S2RowGroupIndex::from_path(osm)?;
    atp_places.par_bridge().try_for_each(|place| {
        let place = place?;
        // println!("got {:?}", place);
        let s2_cell = Cell::from(CellID(place.s2_cell_id));
        let radius = match_distance(&place.mask);
        let cap = Cap::from_center_chordangle(&s2_cell.center(), &radius);
        let covering = coverer.covering(&cap);
        for (_lo, _hi) in MergedCellRanges::new(covering) {
            // println!("TODO: find matches in range {}..={}", lo, hi);
            // if cap.contains_point(&candidate.cell_id.center()) {}
        }
        Ok::<(), anyhow::Error>(())
    })?;

    let _ = row_group_index.query(34, 78);

    let num_features = 1234u64;
    let _progress_bar = make_progress_bar(progress, "diff     ", num_features, "features");

    Ok(out_path)
}
