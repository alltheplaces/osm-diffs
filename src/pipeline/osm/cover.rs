use super::{BlobReader, ParentChainExt, make_progress_bar, read_blobs};
use crate::{coverage::Coverage, u64_table, u64_table::U64Table};
use anyhow::{Ok, Result};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::rename;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread;

pub fn cover_nodes<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    blobs: (usize, usize),
    coverage: &Coverage,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let num_blobs = (blobs.1 - blobs.0) as u64;
    let progress_bar = make_progress_bar(progress, "osm.cov.n", num_blobs, "blobs → nodes");
    let out = workdir.join("covered-nodes");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let mut tmp = out.clone();
    tmp.add_extension("tmp");

    let mut num_covered_nodes = 0;
    thread::scope(|s| {
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (node_tx, node_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer_thread = s.spawn(|| read_blobs(reader, blobs, &progress_bar, blob_tx));

        let handler_thread = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let node_tx = node_tx.clone();
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Node(node) = primitive {
                        let s2_lat_lng = s2::latlng::LatLng::from_degrees(node.lat, node.lon);
                        let cell_id = s2::cellid::CellID::from(s2_lat_lng);
                        if coverage.contains_s2_cell(&cell_id) {
                            node_tx.send(node.id)?;
                        }
                    }
                }
                Ok(())
            })
        });

        let writer_thread = s.spawn(|| {
            num_covered_nodes = u64_table::create(node_rx, workdir, &tmp)?;
            Ok(())
        });

        producer_thread
            .join()
            .expect("producer panic")
            .and(handler_thread.join().expect("handler panic"))
            .and(writer_thread.join().expect("writer panic"))
    })?;

    rename(&tmp, &out)?;
    progress_bar.finish_with_message(format!("blobs → {} nodes", num_covered_nodes));

    Ok(U64Table::open(&out)?)
}

pub fn cover_ways<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    blobs: (usize, usize),
    covered_nodes: &U64Table,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let num_blobs = (blobs.1 - blobs.0) as u64;
    let out = workdir.join("covered-ways");
    let mut tmp = out.clone();
    tmp.add_extension("tmp");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let mut num_covered_ways = 0;
    let progress_bar = make_progress_bar(progress, "osm.cov.w", num_blobs, "blobs → ways");
    thread::scope(|s| {
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (way_tx, way_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer_thread = s.spawn(|| read_blobs(reader, blobs, &progress_bar, blob_tx));

        let handler_thread = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Way(way) = primitive {
                        let mut is_covered = false;
                        for node in way.refs() {
                            if node >= 0 && covered_nodes.contains(node as u64) {
                                is_covered = true;
                                break;
                            }
                        }
                        if is_covered {
                            way_tx.send(way.id)?;
                        }
                    }
                }
                Ok(())
            })
        });

        // Thread to sort way ids and write the resulting table to the working directory.
        let writer_thread = s.spawn(|| {
            num_covered_ways = crate::u64_table::create(way_rx, workdir, &tmp)?;
            Ok(())
        });

        producer_thread
            .join()
            .expect("producer panic")
            .and(handler_thread.join().expect("handler panic"))
            .and(writer_thread.join().expect("writer panic"))
    })?;

    rename(&tmp, &out)?;
    progress_bar.finish_with_message(format!("blobs → {} ways", num_covered_ways));
    Ok(U64Table::open(&out)?)
}

// TODO: Handle recursive relations.
// https://github.com/diffed-places/pipeline/issues/141
pub fn cover_relations<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    blobs: (usize, usize),
    covered_nodes: &U64Table,
    covered_ways: &U64Table,
    relation_parents: &HashMap<u64, u64>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let out = workdir.join("covered-relations");
    let mut tmp = out.clone();
    tmp.add_extension("tmp");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let num_blobs = (blobs.1 - blobs.0) as u64;
    let progress_bar = make_progress_bar(progress, "osm.cov.r", num_blobs, "blobs → relations");
    let mut num_relations = 0;
    thread::scope(|s| {
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (rel_tx, rel_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer_thread = s.spawn(|| read_blobs(reader, blobs, &progress_bar, blob_tx));

        let handler_thread = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(rel) = primitive {
                        let mut is_covered = false;
                        for (_, member_id, member_type) in rel.members() {
                            match member_type {
                                RelationMemberType::Node => {
                                    if covered_nodes.contains(member_id) {
                                        is_covered = true;
                                        break;
                                    }
                                }
                                RelationMemberType::Way => {
                                    if covered_ways.contains(member_id) {
                                        is_covered = true;
                                        break;
                                    }
                                }
                                RelationMemberType::Relation => {}
                            }
                        }
                        if is_covered {
                            // If relation R is geographically within our coverage area,
                            // so are its parents, grandparents, and any further ancestors.
                            // In theory, the OpenStreetMap relation graph should be acyclic,
                            // but in practice such cycles do occur. We break cycles while
                            // walking the parent chain, so we don’t enter an infinite loop.
                            //
                            // With a coverage area derived from AllThePlaces as of 2026-01-03
                            // and the OpenStreetMap planet dump of 2026-01-19, walking the
                            // parent chain increases the yield of relations from 1947 to 2198.
                            // Even though this is only a small quantitative difference,
                            // we need to do this for correctness.
                            for id in relation_parents.parent_chain(rel.id) {
                                rel_tx.send(id)?;
                            }
                        }
                    }
                }
                Ok(())
            })
        });

        // Thread to sort way ids and write the resulting table to the working directory.
        let writer_thread = s.spawn(|| {
            num_relations = crate::u64_table::create(rel_rx, workdir, &tmp)?;
            Ok(())
        });

        producer_thread
            .join()
            .expect("producer panic")
            .and(handler_thread.join().expect("handler panic"))
            .and(writer_thread.join().expect("writer panic"))
    })?;

    rename(&tmp, &out)?;
    progress_bar.finish_with_message(format!("blobs → {} relations", num_relations));
    Ok(U64Table::open(&out)?)
}
