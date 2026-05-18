use anyhow::Result;
use indicatif::MultiProgress;
use std::path::{Path, PathBuf};

use crate::make_progress_bar;
use crate::place::{PlaceIter, S2RowGroupIndex};

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

    // TODO: Implement.
    let atp_places = PlaceIter::try_new(atp)?;
    let row_group_index = S2RowGroupIndex::from_path(osm)?;
    for _place in atp_places {
        // println!("got {:?}", place);
    }

    let _ = row_group_index.query(34, 78);

    let num_features = 1234u64;
    let _progress_bar = make_progress_bar(progress, "diff     ", num_features, "features");

    Ok(out_path)
}
