use super::{BlobReader, ParentChainExt, make_progress_bar};
use crate::{coverage::Coverage, tables::U64Table};
use anyhow::{Ok, Result};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread;

pub fn cover_nodes<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    coverage: &Coverage,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let progress_bar = make_progress_bar(
        progress,
        "osm.cover.nodes ",
        reader.count_node_blobs() as u64,
        "blobs → nodes",
    );
    let out = workdir.join("covered-nodes");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let mut result: Option<U64Table> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (node_tx, node_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer = s.spawn(|| reader.send_node_blobs(blob_tx));

        let handler = s.spawn(move || {
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
                progress_bar.inc(1);
                Ok(())
            })
        });

        let writer = s.spawn(|| {
            result = Some(U64Table::create(node_rx.into_iter(), workdir, &out)?);
            Ok(())
        });

        writer.join().expect("writer panic")?;
        handler.join().expect("handler panic")?;
        producer.join().expect("producer panic")?;
        Ok(())
    })?;
    let result = result.expect("result");
    progress_bar.finish_with_message(format!("blobs → {} nodes", result.len()));
    Ok(result)
}

pub fn cover_ways<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    covered_nodes: &U64Table,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let out = workdir.join("covered-ways");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let mut result: Option<U64Table> = None;
    let progress_bar = make_progress_bar(
        progress,
        "osm.cover.ways  ",
        reader.count_way_blobs() as u64,
        "blobs → ways",
    );
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (way_tx, way_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer = s.spawn(|| reader.send_way_blobs(blob_tx));

        let handler = s.spawn(move || {
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
                progress_bar.inc(1);
                Ok(())
            })
        });

        // Thread to sort way ids and write the resulting table to the working directory.
        let writer = s.spawn(|| {
            result = Some(U64Table::create(way_rx.into_iter(), workdir, &out)?);
            Ok(())
        });

        handler.join().expect("handler panic")?;
        writer.join().expect("writer panic")?;
        producer.join().expect("producer panic")?;
        Ok(())
    })?;
    let result = result.expect("result");

    progress_bar.finish_with_message(format!("blobs → {} ways", result.len()));
    Ok(result)
}

// TODO: Handle recursive relations.
// https://github.com/diffed-places/pipeline/issues/141
pub fn cover_relations<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    covered_nodes: &U64Table,
    covered_ways: &U64Table,
    relation_parents: &HashMap<u64, u64>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    let out = workdir.join("covered-relations");
    if out.exists() {
        return Ok(U64Table::open(&out)?);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.cover.rels  ",
        reader.count_relation_blobs() as u64,
        "blobs → relations",
    );

    let mut result: Option<U64Table> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (rel_tx, rel_rx) = sync_channel::<u64>(num_workers * 1024);

        let producer = s.spawn(|| reader.send_relation_blobs(blob_tx));

        let handler = s.spawn(move || {
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
                progress_bar.inc(1);
                Ok(())
            })
        });

        // Thread to sort way ids and write the resulting table to the working directory.
        let writer = s.spawn(|| {
            result = Some(U64Table::create(rel_rx.into_iter(), workdir, &out)?);
            Ok(())
        });

        handler.join().expect("handler panic")?;
        writer.join().expect("writer panic")?;
        producer.join().expect("producer panic")?;
        Ok(())
    })?;
    let result = result.expect("result");

    progress_bar.finish_with_message(format!("blobs → {} relations", result.len()));
    Ok(result)
}
