use crate::places::{Place, PlaceIndex};
use crate::s2_util::MergedCellRanges;
use crate::{make_progress_bar, match_distance};
use anyhow::Result;
use indicatif::MultiProgress;
use s2::{cap::Cap, cell::Cell, cellid::CellID, region::RegionCoverer};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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

    let num_atp_features = AtomicU64::new(0);
    let num_candidates = AtomicU64::new(0);

    let region = CellID::from_token("479ab6e4"); // Rapperswil SG, Switzerland
    let mut iter = atp_places.query(
        region.range_min()..=region.range_max(),
        crate::MatchMask::SHOP,
    )?;
    for place in &mut iter {
        let place: &Place = place?;
        num_atp_features.fetch_add(1, Ordering::Relaxed);
        let s2_cell = Cell::from(CellID(place.s2_cell_id));
        let center = s2_cell.center();
        let radius = match_distance(&place.mask);
        let cap = Cap::from_center_chordangle(&center, &radius);
        let covering = coverer.covering(&cap);
        for (lo, hi) in MergedCellRanges::new(covering) {
            for candidate in &mut osm_places.query(lo..=hi, place.mask)? {
                let candidate = candidate?;
                num_candidates.fetch_add(1, Ordering::Relaxed);
                let candidate_center =
                    s2::point::Point(CellID(candidate.s2_cell_id).raw_point().normalize());
                let _distance = center.distance(&candidate_center).rad() * 6_371_000.0;
            }
        }
    }

    let cache_stats = osm_places.cache_stats();
    progress_bar.finish();
    println!(
        "  features: {} candidates: {}",
        num_atp_features.load(Ordering::SeqCst),
        num_candidates.load(Ordering::SeqCst)
    );
    println!(
        "  cache hits: {} misses: {} hit rate: {:.1}%",
        cache_stats.hits,
        cache_stats.misses,
        cache_stats.hit_rate().unwrap_or(0.0) * 100.0
    );

    Ok(out_path)
}
