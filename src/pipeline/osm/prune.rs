use super::BlobReader;
use crate::{
    coverage::Coverage,
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
pub struct Pruner<'a> {
    _coverage: &'a Coverage<'a>,
    keep_ways: U64Table,
    keep_way_members: U64Table,
    keep_relations: U64Table,
    keep_relation_members: U64Table,
}

impl<'a> Pruner<'a> {
    pub fn create(
        osm_reader: &mut BlobReader<File>,
        coverage: &'a Coverage<'a>,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<Pruner<'a>> {
        let rels_output = prune_relations(osm_reader, progress, workdir)?;
        let ways_output = prune_ways(
            osm_reader,
            &rels_output.keep_relation_members,
            progress,
            workdir,
        )?;
        Ok(Pruner {
            _coverage: coverage,
            keep_ways: ways_output.keep_ways,
            keep_way_members: ways_output.keep_way_members,
            keep_relations: rels_output.keep_relations,
            keep_relation_members: rels_output.keep_relation_members,
        })
    }

    #[allow(unused)]
    pub fn keep_node_coords(&self, id: u64) -> bool {
        self.keep_way_members.contains(id) || self.keep_relation_members.contains(id * 10 + 1)
    }

    #[allow(unused)]
    pub fn keep_way(&self, id: u64) -> bool {
        self.keep_ways.contains(id) || self.keep_relation_members.contains(id * 10 + 2)
    }

    #[allow(unused)]
    pub fn keep_relation(&self, id: u64) -> bool {
        self.keep_relations.contains(id) || self.keep_relation_members.contains(id * 10 + 3)
    }
}

/// Output of [prune_relations].
struct PruneRelationsOutput {
    /// The set of OpenStreetMap relations to keep in our pipeline.
    /// For example, a relation tagged as `amenity=restaurant` would be
    /// a member of this set.
    keep_relations: U64Table,

    /// A table that tells which OpenStreetMap nodes, ways and relations
    /// are members (either direct or indirect, in case of recursive relations)
    /// of a relation we decided to keep. We need those members to construct
    /// a geometry for relations in `keep_relations`.
    keep_relation_members: U64Table,
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_relations(
    reader: &mut BlobReader<File>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneRelationsOutput> {
    let keep_relations_path = PathBuf::from(workdir).join("osm-prune.keep-relations");
    let keep_relation_members_path = PathBuf::from(workdir).join("osm-prune.keep-relation-members");
    if keep_relations_path.exists() && keep_relation_members_path.exists() {
        return Ok(PruneRelationsOutput {
            keep_relations: U64Table::open(&keep_relations_path)?,
            keep_relation_members: U64Table::open(&keep_relation_members_path)?,
        });
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
    let stats = prune_relations_pass_2(
        reader,
        &keep_relations,
        &graph,
        &progress_bar,
        workdir,
        &keep_relation_members_path,
    )?;

    progress_bar.finish_with_message(format!(
        "blobs → {} relations (with {} nodes, {} ways, {} relations as members)",
        keep_relations.len(),
        stats.node_count,
        stats.way_count,
        stats.relation_count,
    ));

    Ok(PruneRelationsOutput {
        keep_relations: U64Table::open(&keep_relations_path)?,
        keep_relation_members: U64Table::open(&keep_relation_members_path)?,
    })
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
        let keep_writer = s.spawn(|| u64_table::create(keep_rx, workdir, out));
        keep_writer.join().expect("panic in keep_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;
    Ok(stats.lock().expect("lock").clone())
}

/// Output of [prune_ways].
struct PruneWaysOutput {
    /// The set of OpenStreetMap ways to keep in our pipeline.
    /// For example, a way tagged as `tourism=hotel` would be
    /// a member of this set.
    keep_ways: U64Table,

    /// A table that tells which OpenStreetMap nodes are members
    /// are members of a way we decided to keep. We need their coordinates
    /// to construct the geometry for the ways in `keep_ways`.
    keep_way_members: U64Table,
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_ways(
    reader: &mut BlobReader<File>,
    keep_relation_members: &U64Table,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneWaysOutput> {
    let keep_ways_path = PathBuf::from(workdir).join("osm-prune.keep-ways");
    let keep_way_members_path = PathBuf::from(workdir).join("osm-prune.keep-way-members");
    if keep_ways_path.exists() && keep_way_members_path.exists() {
        return Ok(PruneWaysOutput {
            keep_ways: U64Table::open(&keep_ways_path)?,
            keep_way_members: U64Table::open(&keep_way_members_path)?,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.ways  ",
        reader.count_way_blobs() as u64,
        "blobs",
    );

    let stats = Arc::new(Mutex::new(PruneStats::default()));
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (way_tx, way_rx) = sync_channel::<u64>(64 * 1024);
        let (node_tx, node_rx) = sync_channel::<u64>(64 * 1024);
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
                            way_tx.send(way.id)?;
                            way_count += 1;
                            for node_id in way.refs() {
                                if node_id > 0 {
                                    node_tx.send(node_id as u64)?;
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
        let way_writer = s.spawn(|| u64_table::create(way_rx, workdir, &keep_ways_path));
        let node_writer = s.spawn(|| u64_table::create(node_rx, workdir, &keep_way_members_path));

        node_writer.join().expect("panic in node_writer")?;
        way_writer.join().expect("panic in way_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;
    let stats = stats.lock().expect("lock").clone();

    // OSM ways don’t have relation members, so we don’t emit any relations.
    progress_bar.finish_with_message(format!(
        "blobs → {} ways (with {} nodes as members)",
        stats.way_count, stats.node_count,
    ));

    Ok(PruneWaysOutput {
        keep_ways: U64Table::open(&keep_ways_path)?,
        keep_way_members: U64Table::open(&keep_way_members_path)?,
    })
}
