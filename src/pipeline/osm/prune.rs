use super::BlobReader;
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    tables::{Edge, GraphTable},
    u64_table,
    u64_table::U64Table,
};
use anyhow::{Ok, Result};
use indicatif::{MultiProgress, ProgressBar};
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use rayon::prelude::*;
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc::sync_channel},
    thread,
};

/// Decides which parts of OpenStreetMap we need for conflation.
pub struct Pruner {
    // coverage: &Coverage,
    keep_way_members: U64Table,
    keep_relations: U64Table,
    keep_relation_members: U64Table,
}

impl Pruner {
    pub fn create(
        osm_reader: &mut BlobReader<File>,
        // coverage: &Coverage,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<Pruner> {
        let (keep_relations, keep_relation_members) =
            prune_relations(osm_reader, progress, workdir)?;
        let keep_way_members = prune_ways(osm_reader, &keep_relation_members, progress, workdir)?;
        Ok(Pruner {
            keep_way_members,
            keep_relations,
            keep_relation_members,
        })
    }

    #[allow(unused)]
    pub fn keep_node_coords(&self, id: u64) -> bool {
        let feature_id = id * 10 + 1;
        self.keep_way_members.contains(feature_id)
            || self.keep_relation_members.contains(feature_id)
    }

    #[allow(unused)]
    pub fn keep_way(&self, id: u64) -> bool {
        // TODO: Also test self.keep_ways.contains(id)
        self.keep_relation_members.contains(id * 10 + 3)
    }

    #[allow(unused)]
    pub fn keep_relation(&self, id: u64) -> bool {
        self.keep_relations.contains(id) || self.keep_relation_members.contains(id * 10 + 3)
    }
}

/// Pipeline step `osm.prune.rels`.
///
/// # Inputs
///
/// * OpenStreetMap relations.
///
/// # Outputs
///
/// * `osm-prune.relation-members`, a table that tells which OpenStreetMap
///   nodes, ways and relations need to be indexed for conflating OSM relations
///   with AllThePlaces. To evaluate OSM relations as match candidates, we need
///   to access their members (which can be nodes, ways or relations).
fn prune_relations(
    reader: &mut BlobReader<File>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<(U64Table, U64Table)> {
    let keep_relations_path = PathBuf::from(workdir).join("osm-prune.keep-relations");
    let keep_relation_members_path = PathBuf::from(workdir).join("osm-prune.keep-relation-members");
    if keep_relations_path.exists() && keep_relation_members_path.exists() {
        let keep_relations = U64Table::open(&keep_relations_path)?;
        let keep_relation_members = U64Table::open(&keep_relation_members_path)?;
        return Ok((keep_relations, keep_relation_members));
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.rels  ",
        (reader.count_relation_blobs() as u64) * 2, // two passes
        "blobs",
    );

    // First pass.
    let (keep_relations, graph) =
        prune_relations_pass_1(reader, &progress_bar, workdir, &keep_relations_path)?;

    // Second pass.
    let tmp_path = PathBuf::from(workdir).join("osm-prune-relations.tmp");
    let stats = prune_relations_pass_2(
        reader,
        &keep_relations,
        &graph,
        &progress_bar,
        workdir,
        &tmp_path,
    )?;
    std::fs::rename(&tmp_path, &keep_relation_members_path)?;

    progress_bar.finish_with_message(format!(
        "blobs → {} nodes, {} ways, {} relations",
        stats.node_count, stats.way_count, stats.relation_count,
    ));

    let keep_relations = U64Table::open(&keep_relations_path)?;
    let keep_relation_members = U64Table::open(&keep_relation_members_path)?;
    Ok((keep_relations, keep_relation_members))
}

/// Pipeline step `osm.prune.rels`, pass 1 of 2.
///
/// # Inputs
///
/// * OpenStreetMap relations.
///
/// # Outputs
///
/// * `osm-prune.keep-relations`, a table indicating which
///   OSM relations carry tags that indicate potential conflation
///   candidates. For most relations in OpenStreetMap (such as city
///   boundaries or river networks) we don’t have any matchers in our
///   conflation pipeline, so we can drop them early.
///
/// * `osm-prune.relation-graph`, a table with the containment
///   hierarchy of relations. In the OpenStreetMap schema, relations
///   may point to other relations as their members, forming a
///   containment graph. (Theoretically, this graph is supposed to be
///   acyclic, but in practice this is not always the case; we guard
///   against cycles when traversing the graph).
fn prune_relations_pass_1<'a>(
    reader: &mut BlobReader<File>,
    progress_bar: &ProgressBar,
    workdir: &Path,
    keep_relations_path: &Path,
) -> Result<(U64Table, GraphTable<'a>)> {
    let relation_graph_path = PathBuf::from(workdir).join("osm-prune.relation-graph");
    if keep_relations_path.exists() && relation_graph_path.exists() {
        let keep_relations = U64Table::open(keep_relations_path)?;
        let relations_graph = GraphTable::open(&relation_graph_path)?;
        return Ok((keep_relations, relations_graph));
    }

    let mut relations_graph: Option<GraphTable<'_>> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(64 * 1024);
        let (edge_tx, edge_rx) = sync_channel::<Edge>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_relation_blobs(blob_tx));
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(rel) = primitive {
                        // Build table of relations worth keeping.
                        let mut mask = MatchMask::default();
                        for (key, value) in rel.tags() {
                            mask.add_tag(key, value);
                        }
                        if !mask.is_empty() {
                            keep_tx.send(rel.id)?;
                        }

                        // Build relations graph.
                        for (_, member_id, member_type) in rel.members() {
                            if member_type == RelationMemberType::Relation {
                                edge_tx.send(Edge {
                                    child: member_id,
                                    parent: rel.id,
                                })?;
                            }
                        }
                    };
                }
                progress_bar.inc(1);
                Ok(())
            })
        });
        let keep_writer = s.spawn(|| u64_table::create(keep_rx, workdir, keep_relations_path));
        let graph_writer = s.spawn(|| {
            relations_graph = Some(GraphTable::create(
                edge_rx.into_iter(),
                workdir,
                &relation_graph_path,
            )?);
            Ok(())
        });
        keep_writer.join().expect("panic in keep_writer")?;
        graph_writer.join().expect("panic in graph_writer")?;
        blob_consumer.join().expect("panic in consumer")?;
        blob_producer.join().expect("panic in producer")?;
        Ok(())
    })?;

    let keep_relations = U64Table::open(keep_relations_path)?;
    Ok((keep_relations, relations_graph.expect("graph")))
}

/// High-level statistics for pruning steps.
#[derive(Clone, Default)]
struct PruneStats {
    node_count: u64,
    way_count: u64,
    relation_count: u64,
}

/// Pipeline step `osm.prune.rels`, pass 2 of 2.
///
/// # Inputs
///
/// * OpenStreetMap relations.
///
/// * `osm-prune.keep-relations`, a table indicating which
///   OSM relations carry tags that indicate potential conflation
///   candidates. For most relations in OpenStreetMap (such as city
///   boundaries or river networks) we don’t have any matchers in our
///   conflation pipeline, so we can drop them early. This input gets
///   produced by [prune_relations_pass_1].
///
/// * `osm-prune.relations-graph`, a table with the containment
///   hierarchy of relations. In the OpenStreetMap schema, relations
///   may point to other relations as their members, forming a
///   containment graph. (Theoretically, this graph is supposed to be
///   acyclic, but in practice this is not always the case; we guard
///   against cycles when traversing the graph). Again, this thinput
///   gets produces by [prune_relations_pass_1].
///
/// # Outputs
///
/// * `osm-prune.keep-relation-members`, a table that tells which OpenStreetMap
///   nodes, ways and relations need to be indexed for conflating OSM relations
///   with AllThePlaces. To evaluate OSM relations as match candidates, we need
///   to access their members (which can be nodes, ways or relations).
fn prune_relations_pass_2(
    reader: &mut BlobReader<File>,
    keep_1: &U64Table,
    graph: &GraphTable<'_>,
    progress_bar: &ProgressBar,
    workdir: &Path,
    out: &Path,
) -> Result<PruneStats> {
    let stats = Arc::new(Mutex::new(PruneStats::default()));
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_relation_blobs(blob_tx));
        let blob_consumer_stats = stats.clone();
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let mut node_count = 0;
                let mut way_count = 0;
                let mut relation_count = 0;
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(rel) = primitive
                        && graph.ancestors(rel.id).any(|id| keep_1.contains(id))
                    {
                        keep_tx.send(rel.id * 10 + 3)?;
                        for (_role_name, member_id, member_type) in rel.members() {
                            match member_type {
                                RelationMemberType::Node => {
                                    node_count += 1;
                                    keep_tx.send(member_id * 10 + 1)?
                                }
                                RelationMemberType::Way => {
                                    way_count += 1;
                                    keep_tx.send(member_id * 10 + 2)?
                                }
                                RelationMemberType::Relation => {
                                    relation_count += 1;
                                    keep_tx.send(member_id * 10 + 3)?
                                }
                            }
                        }
                    }
                }
                let mut s = blob_consumer_stats.lock().unwrap();
                s.node_count += node_count;
                s.way_count += way_count;
                s.relation_count += relation_count;
                progress_bar.inc(1);
                Ok(())
            })
        });
        let pruned_writer = s.spawn(|| u64_table::create(keep_rx, workdir, out));
        pruned_writer.join().expect("panic in pruned_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;
    Ok(stats.lock().expect("lock").clone())
}

/// Pipeline step `osm.prune.ways`.
///
/// # Inputs
///
/// * OpenStreetMap ways.
///
/// * `osm-prune.keep-relation-members`, a table that tells which OpenStreetMap
///   nodes, ways and relations need to be indexed for conflating OSM relations
///   with AllThePlaces. To evaluate OSM relations as match candidates, we need
///   to access their members (which can be nodes, ways or relations).
///   This input gets produced by [prune_relations].
///
/// # Outputs
///
/// * `osm-prune.keep-way-members`, a table that tells which OpenStreetMap nodes
///   and ways need to be indexed for conflating OSM ways with AllThePlaces.
///   To evaluate an OSM way as a match candidate, we need to access not only
///   the way itself, but also its member nodes.
fn prune_ways(
    reader: &mut BlobReader<File>,
    keep_relation_members: &U64Table,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<U64Table> {
    // TODO: Also build a table osm-prune.keep-ways, separate from keep-way-members.
    let out_path = PathBuf::from(workdir).join("osm-prune.keep-way-members");
    if out_path.exists() {
        return U64Table::open(&out_path);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.ways  ",
        reader.count_way_blobs() as u64,
        "blobs",
    );

    let mut tmp_path = out_path.clone();
    tmp_path.add_extension("tmp");

    let stats = Arc::new(Mutex::new(PruneStats::default()));
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_way_blobs(blob_tx));
        let blob_consumer_stats = stats.clone();
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let mut node_count = 0;
                let mut way_count = 0;
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Way(way) = primitive {
                        let mut mask = MatchMask::default();
                        for (key, value) in way.tags() {
                            mask.add_tag(key, value);
                        }

                        let way_feature_id = way.id * 10 + 2;
                        if !mask.is_empty() || keep_relation_members.contains(way_feature_id) {
                            keep_tx.send(way_feature_id)?;
                            way_count += 1;
                            for node_id in way.refs() {
                                if node_id > 0 {
                                    keep_tx.send(node_id as u64 * 10 + 1)?;
                                    node_count += 1;
                                }
                            }
                        }
                    }
                }
                let mut stats = blob_consumer_stats.lock().unwrap();
                stats.node_count += node_count;
                stats.way_count += way_count;
                progress_bar.inc(1);
                Ok(())
            })
        });
        let pruned_writer = s.spawn(|| u64_table::create(keep_rx, workdir, &tmp_path));
        pruned_writer.join().expect("panic in pruned_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;
    let stats = stats.lock().expect("lock").clone();
    std::fs::rename(&tmp_path, &out_path)?;

    // OSM ways don’t have relation members, so we don’t emit any relations.
    progress_bar.finish_with_message(format!(
        "blobs → {} nodes, {} ways",
        stats.node_count, stats.way_count,
    ));

    U64Table::open(&out_path)
}
