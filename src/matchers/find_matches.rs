use super::{create_matcher, match_distance};
use crate::{
    make_progress_bar,
    places::{Place, PlaceIndex},
    s2_util::MergedCellRanges,
};
use anyhow::{Ok, Result};
use deepsize::DeepSizeOf;
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use rayon::prelude::*;
use s2::{cap::Cap, cell::Cell, cellid::CellID};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
};

/// A match between OpenStreetMap and AllThePlaces.
///
/// If AllThePlaces contains features that cannot be matched to
/// anything in OpenStreetMap, we still generate a `Match` with `osm`
/// being `None`; edit writers may use this to suggest creating new
/// OSM features.
///
/// Note that AllThePlaces fetches data not only from first party
/// sources, but also from government data and aggregators. If the
/// data license is compatible with OpenStreetMap (or if the data
/// source has signed an explicit waiver to permit OpenStreetMap
/// imports), such data flows into our system.  Therefore, it can
/// happen that multiple AllThePlaces features match the same OSM
/// feature.
#[derive(Debug)]
pub struct Match {
    pub osm: Option<Place>,
    pub atp_matches: Vec<Place>,
}

pub fn find_matches(
    atp: &Path,
    osm: &Path,
    progress: &indicatif::MultiProgress,
    workdir: &Path,
    out: SyncSender<Match>,
) -> Result<()> {
    let mut producer_result = Ok(());
    let mut consumer_result = Ok(());
    thread::scope(|s| {
        let (pairs_tx, pairs_rx) = sync_channel::<MatchPair>(8192);
        s.spawn(|| {
            producer_result = find_match_pairs(atp, osm, progress, pairs_tx);
        });
        s.spawn(|| {
            consumer_result = group_match_pairs(pairs_rx, workdir, out);
        });
    });
    consumer_result?;
    producer_result?;
    Ok(())
}

// TODO: Speed up by writing a custom implementation of Ord/Eq, based on just osm_id.
#[derive(Debug, DeepSizeOf, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
struct MatchPair {
    osm: Option<Place>,
    atp: Place,
}

fn find_match_pairs(
    atp: &Path,
    osm: &Path,
    progress: &indicatif::MultiProgress,
    out: SyncSender<MatchPair>,
) -> Result<()> {
    let atp = PlaceIndex::open(atp, 1)?;
    let num_atp_features = atp.total_rows() as u64;
    let num_pairs = AtomicU64::new(0);
    let progress_bar = make_progress_bar(progress, "match.find", num_atp_features, "ATP features");
    let osm = PlaceIndex::open(osm, 32)?;

    let coverer = s2::region::RegionCoverer {
        max_cells: 16,
        min_level: 12,
        max_level: s2::cellid::MAX_LEVEL as u8,
        level_mod: 1,
    };

    for group in atp.scan_row_groups() {
        // Each group is processed by the Rayon thread pool in parallel,
        // but the outer loop is sequential — so nearby places (within a
        // group) always go to nearby workers, preserving spatial locality.
        group?.par_iter().try_for_each(|place| {
            progress_bar.inc(1);
            if let Some(matcher) = create_matcher(place) {
                let s2_cell = Cell::from(CellID(place.s2_cell_id));
                let center = s2_cell.center();
                let radius = match_distance(&place.mask);
                let cap = Cap::from_center_chordangle(&center, &radius);
                let covering = coverer.covering(&cap);
                let mut best_candidate: Option<Place> = None;
                let mut best_score: f64 = 0.0;
                for (lo, hi) in MergedCellRanges::new(covering) {
                    let mut iter = osm.query(lo..=hi, place.mask)?;
                    let mut bc: Option<&Place> = None;
                    for candidate in &mut iter {
                        let candidate = candidate?;
                        let score = matcher.score(candidate);
                        if score > best_score {
                            bc = Some(candidate);
                            best_score = score;
                        }
                    }
                    if let Some(b) = bc {
                        best_candidate = Some(b.deep_clone());
                    }
                }

                // TODO: Also send MatchPairs where no OSM feature could be found,
                // once we can handle OSM relations. Until then, there’s no point
                // because we don’t want to generate edits to create new OSM features
                // when there’s a relation in OSM we can’t recognize.
                if best_candidate != None {
                    let pair = MatchPair {
                        osm: best_candidate,
                        atp: place.deep_clone(),
                    };
                    num_pairs.fetch_add(1, Ordering::Relaxed);
                    out.send(pair)?;
                }
            }
            Ok(())
        })?;
    }

    let num_pairs = num_pairs.load(Ordering::SeqCst);
    progress_bar.finish_with_message(format!("ATP features → {} ATP/OSM MatchPairs", num_pairs));
    Ok(())
}

/// Sorts a channel of `MatchPairs` by `osm_id`, grouping them into `Matches` for the same OSM feature.
/// The resulting `Matches` are sent over a channel to the receiver.
fn group_match_pairs(
    pairs: Receiver<MatchPair>,
    workdir: &Path,
    out: SyncSender<Match>,
) -> Result<()> {
    let sorter: ExternalSorter<MatchPair, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(200_000_000))
            .build()?;
    let num_pairs = AtomicU64::new(0);
    let sorted = sorter.sort(pairs.iter().map(|x| {
        num_pairs.fetch_add(1, Ordering::Relaxed);
        std::io::Result::Ok(x)
    }))?;
    let _num_pairs = num_pairs.load(Ordering::SeqCst);

    let mut cur_group = Match {
        osm: None,
        atp_matches: Vec::with_capacity(1),
    };
    for pair in sorted {
        let pair = pair?;
        if let Some(ref pair_osm) = pair.osm
            && let Some(ref cur_group_osm) = cur_group.osm
            && pair_osm.osm_id == cur_group_osm.osm_id
        {
            cur_group.atp_matches.push(pair.atp);
            continue;
        }
        if !cur_group.atp_matches.is_empty() {
            out.send(cur_group)?;
        }
        cur_group = Match {
            osm: pair.osm,
            atp_matches: Vec::with_capacity(1),
        };
    }
    if !cur_group.atp_matches.is_empty() {
        out.send(cur_group)?;
    }

    Ok(())
}

// TODO: Write tests, especially for group_match_pairs.
