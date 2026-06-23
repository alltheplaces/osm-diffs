use crate::matchers::create_matcher;
use crate::places::{Place, PlaceIndex};
use crate::s2_util::MergedCellRanges;
use crate::{TileLayer, make_progress_bar, match_distance};
use anyhow::Result;
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use indicatif::{MultiProgress, ProgressBar};
use rayon::prelude::*;
use s2::{cap::Cap, cell::Cell, cellid::CellID, region::RegionCoverer};
use std::fs::{File, rename};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

struct EditWriter {
    shops: SyncSender<Place>,
    _infrastructure: SyncSender<Place>,
    _trees: SyncSender<Place>,
}

impl EditWriter {
    fn make_layers(workdir: &Path) -> Vec<TileLayer> {
        ["shops", "infrastructure", "trees"]
            .iter()
            .map(|&s| TileLayer {
                name: String::from(s),
                path: workdir.join(String::from(s) + ".jsonl"),
            })
            .collect()
    }
}

pub fn suggest_edits(
    _coverage: &Path,
    atp: &Path,
    osm: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<Vec<TileLayer>> {
    assert!(workdir.exists());

    let layers = EditWriter::make_layers(workdir);
    if layers.iter().all(|layer| layer.path.exists()) {
        return Ok(layers);
    }

    let atp = PlaceIndex::open(atp, 1)?;
    let num_atp_places = atp.total_rows() as u64;
    let progress_bar = make_progress_bar(progress, "sugg-edit", num_atp_places, "ATP features");
    let osm = PlaceIndex::open(osm, 32)?; // TODO: More but smaller row groups for OSM.

    let mut producer_result = Ok(());
    let mut num_shop_edits = Ok(0);
    let mut num_infrastructure_edits = Ok(0);
    let mut num_tree_edits = Ok(0);
    thread::scope(|s| {
        let (shops_tx, shops_rx) = sync_channel::<Place>(8192);
        let (infrastructure_tx, infrastructure_rx) = sync_channel::<Place>(8192);
        let (trees_tx, trees_rx) = sync_channel::<Place>(8192);
        let writer = EditWriter {
            shops: shops_tx,
            _infrastructure: infrastructure_tx,
            _trees: trees_tx,
        };
        s.spawn(|| {
            producer_result = produce_edits(atp.clone(), osm.clone(), &progress_bar, writer)
        });
        s.spawn(|| num_shop_edits = write_edits(shops_rx, &layers[0].path, workdir));
        s.spawn(|| {
            num_infrastructure_edits = write_edits(infrastructure_rx, &layers[1].path, workdir)
        });
        s.spawn(|| num_tree_edits = write_edits(trees_rx, &layers[2].path, workdir));
    });
    producer_result?;
    let num_edits = num_shop_edits? + num_infrastructure_edits? + num_tree_edits?;
    progress_bar.finish_with_message(format!("ATP features → {} suggested OSM edits", num_edits));

    let cache_stats = osm.cache_stats();
    println!(
        "  cache hits: {} misses: {} hit rate: {:.1}%",
        cache_stats.hits,
        cache_stats.misses,
        cache_stats.hit_rate().unwrap_or(0.0) * 100.0
    );

    Ok(layers)
}

fn produce_edits(
    atp: Arc<PlaceIndex>,
    osm: Arc<PlaceIndex>,
    progress_bar: &ProgressBar,
    out: EditWriter,
) -> Result<()> {
    let coverer = RegionCoverer {
        max_cells: 16,
        min_level: 12,
        max_level: s2::cellid::MAX_LEVEL as u8,
        level_mod: 1,
    };

    let num_atp_features = AtomicU64::new(0);
    let num_candidates = AtomicU64::new(0);
    let num_matches = AtomicU64::new(0);

    for group in atp.scan_row_groups() {
        // Each group is processed by the Rayon thread pool in parallel,
        // but the outer loop is sequential — so nearby places (within a
        // group) always go to nearby workers, preserving spatial locality.
        group?.par_iter().try_for_each(|place| {
            progress_bar.inc(1);
            if let Some(matcher) = create_matcher(place) {
                num_atp_features.fetch_add(1, Ordering::Relaxed);
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
                        num_candidates.fetch_add(1, Ordering::Relaxed);
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
                if let Some(best_candidate) = best_candidate
                    && best_score > 0.0
                {
                    num_matches.fetch_add(1, Ordering::Relaxed);
                    if let Some(edit) = matcher.suggest_edit(&best_candidate) {
                        if false {
                            println!(
                                "score={} place={:?} best_candidate={:?} edit={:?}",
                                best_score, place, best_candidate, edit
                            );
                        }
                        // TODO: Dispatch to one of {shops, infrastructure, trees}.
                        out.shops.send(edit)?;
                    }
                }
            };
            Ok::<(), anyhow::Error>(())
        })?;
    }

    Ok(())
}

fn write_edits(edits: Receiver<Place>, path: &Path, workdir: &Path) -> Result<u64> {
    let mut tmp_path = PathBuf::from(&path);
    tmp_path.add_extension("tmp");
    let mut writer = BufWriter::with_capacity(32768, File::create(&tmp_path)?);

    let sorter: ExternalSorter<Place, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
            .build()?;

    let num_edits = AtomicU64::new(0);
    let sorted = sorter.sort(edits.iter().map(|x| {
        num_edits.fetch_add(1, Ordering::Relaxed);
        std::io::Result::Ok(x)
    }))?;
    let mut last_osm_id = 0;
    for edit in sorted {
        let edit = edit?;
        // Only emit one single edit per OSM ID.
        if edit.osm_id == last_osm_id {
            continue;
        }
        last_osm_id = edit.osm_id;
        let mut line = edit.to_geojson().to_string();
        line.push('\n');
        writer.write_all(line.as_ref())?;
    }
    writer.flush()?;
    rename(&tmp_path, path)?;

    Ok(num_edits.load(Ordering::SeqCst))
}
