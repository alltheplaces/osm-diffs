use super::{FeatureStore, make_progress_bar};
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use geo::{Centroid, Coord, LineString};
use indicatif::{MultiProgress, ProgressBar};
use rayon::prelude::*;
use std::fs::rename;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use crate::{
    MatchMask,
    places::{ParquetWriter, Place},
};

pub fn assemble(
    store: &dyn FeatureStore,
    progress: &MultiProgress,
    workdir: &Path,
    out_path: &Path,
) -> Result<()> {
    let (tx, rx) = sync_channel::<Place>(50_000);
    let feature_count = store.node_count() + store.way_count() + store.relation_count();
    let progress_bar = make_progress_bar(progress, "osm.assemble.r", feature_count, "features");
    thread::scope(|s| {
        let node_handler = {
            let tx = tx.clone();
            s.spawn(|| assemble_nodes(store, &progress_bar, tx))
        };
        let way_handler = {
            let tx = tx.clone();
            s.spawn(|| assemble_ways(store, &progress_bar, tx))
        };
        let writer = s.spawn(|| write_places(rx, progress, workdir, out_path));
        node_handler
            .join()
            .expect("panic in node_handler")
            .and(way_handler.join().expect("panic in way_handler"))?;
        drop(tx); // Done writing. Close channel, so writer can terminate.
        writer.join().expect("panic in writer")
    })?;
    progress_bar.finish();
    Ok(())
}

fn assemble_nodes(
    store: &dyn FeatureStore,
    progress_bar: &ProgressBar,
    out: SyncSender<Place>,
) -> Result<()> {
    (0..store.node_count()).into_par_iter().try_for_each(|i| {
        if let Some(node) = store.get_nth_node(i) {
            let mut mask = MatchMask::default();
            for c in node.tags.chunks_exact(2) {
                mask.add_tag(&c[0], &c[1]);
            }
            if !mask.is_empty() {
                let coord = Coord {
                    x: node.lon_e7 as f64 * 1e-7,
                    y: node.lat_e7 as f64 * 1e-7,
                };
                let source = String::from("osm");
                let tags = node
                    .tags
                    .chunks_exact(2)
                    .map(|c| (c[0].clone(), c[1].clone()))
                    .collect();
                if let Some(mut place) = Place::new(&coord, source, mask, tags) {
                    place.osm_id = node.id * 10 + 1;
                    out.send(place)?;
                }
            }
        };
        progress_bar.inc(1);
        Ok(())
    })
}

fn assemble_ways(
    store: &dyn FeatureStore,
    progress_bar: &ProgressBar,
    out: SyncSender<Place>,
) -> Result<()> {
    (0..store.way_count()).into_par_iter().try_for_each(|i| {
        if let Some(way) = store.get_nth_way(i)
            && !way.tags.is_empty()
        {
            let mut mask = MatchMask::default();
            for c in way.tags.chunks_exact(2) {
                mask.add_tag(&c[0], &c[1]);
            }
            if !mask.is_empty() {
                let mut coords = Vec::<geo::Coord>::with_capacity(way.nodes.len());
                for node_id in way.nodes.iter() {
                    if let Some(c) = store.get_coord(*node_id) {
                        coords.push(c);
                    }
                }

                // In theory, OpenStreetMap ways can self-intersect.
                // In practice, this is super rare and checked by
                // QA tools such as Osmose. So, we don’t really care
                // as long as the pipeline doesn’t crash. If this ever
                // becomes a real problem, we could use the geos crate.
                let linestring = LineString::new(coords);
                if let Some(centroid) = linestring.centroid() {
                    let tags: Vec<(String, String)> = way
                        .tags
                        .chunks_exact(2)
                        .map(|c| (c[0].clone(), c[1].clone()))
                        .collect();
                    let source = String::from("osm");
                    if let Some(mut place) = Place::new(&centroid.0, source, mask, tags) {
                        place.osm_id = way.id * 10 + 2;
                        out.send(place)?;
                    }
                }
            }
        }

        progress_bar.inc(1);
        Ok(())
    })?;
    Ok(())
}

fn write_places(
    places: Receiver<Place>,
    progress: &MultiProgress,
    workdir: &Path,
    out: &Path,
) -> Result<()> {
    let mut tmp = PathBuf::from(out);
    tmp.add_extension("tmp");

    let mut writer = ParquetWriter::try_new(
        /* batch size, in records */ 64 * 1024,
        /* page size, in bytes */ 1024 * 1024,
        /* osm */ true,
        &tmp,
    )?;
    let sorter: ExternalSorter<Place, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
            .build()?;

    let feature_count = AtomicU64::new(0);
    let sorted = sorter.sort(places.iter().map(|x| {
        feature_count.fetch_add(1, Ordering::SeqCst);
        std::io::Result::Ok(x)
    }))?;
    let feature_count = feature_count.load(Ordering::SeqCst);

    let progress_bar = make_progress_bar(progress, "osm.assemble.w", feature_count, "features");
    for place in sorted {
        writer.write(place?)?;
        progress_bar.inc(1);
    }
    writer.close()?;
    rename(&tmp, out)?;

    progress_bar.finish_with_message(format!("{} features", feature_count));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{assemble_nodes, assemble_ways};
    use crate::MatchMask;
    use crate::osm::{Node, Way, tests::MockFeatureStore};
    use crate::places::Place;
    use anyhow::{Ok, Result};
    use indicatif::{ProgressBar, ProgressDrawTarget};
    use std::sync::mpsc::sync_channel;

    #[test]
    fn test_assemble_nodes() -> Result<()> {
        let store = MockFeatureStore::new(
            vec![Node {
                id: 12345,
                tags: vec![String::from("shop"), String::from("supermarket")],
                lon_e7: 11_000_000_0,
                lat_e7: 12_000_000_0,
            }],
            vec![],
            vec![],
        );
        let progress = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        let (tx, rx) = sync_channel::<Place>(50);
        assemble_nodes(&store, &progress, tx)?;
        let mut rx = rx.into_iter();

        let mut want = Place::new(
            &geo::Coord { x: 11.0, y: 12.0 },
            String::from("osm"),
            MatchMask::SHOP,
            vec![(String::from("shop"), String::from("supermarket"))],
        )
        .expect("cannot construct wanted Place");
        want.osm_id = 123451;
        assert_eq!(rx.next(), Some(want));
        assert_eq!(rx.next(), None);
        Ok(())
    }

    #[test]
    fn test_assemble_ways() -> Result<()> {
        let store = MockFeatureStore::new(
            vec![
                Node {
                    id: 1,
                    tags: vec![],
                    lon_e7: 50_000_000_0,
                    lat_e7: 20_000_000_0,
                },
                Node {
                    id: 2,
                    tags: vec![],
                    lon_e7: 70_000_000_0,
                    lat_e7: 40_000_000_0,
                },
            ],
            vec![Way {
                id: 3,
                nodes: vec![1, 2, 666],
                tags: vec![String::from("natural"), String::from("tree_row")],
            }],
            vec![],
        );
        let progress = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
        let (tx, rx) = sync_channel::<Place>(50);
        assemble_ways(&store, &progress, tx)?;
        let mut rx = rx.into_iter();

        let mut want = Place::new(
            &geo::Coord { x: 60.0, y: 30.0 }, // centroid
            String::from("osm"),
            MatchMask::SHRUBBERY,
            vec![(String::from("natural"), String::from("tree_row"))],
        )
        .expect("cannot construct wanted Place");
        want.osm_id = 32;
        assert_eq!(rx.next(), Some(want));
        assert_eq!(rx.next(), None);
        Ok(())
    }
}
