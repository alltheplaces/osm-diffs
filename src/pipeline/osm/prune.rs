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
    sync::mpsc::sync_channel,
    thread,
};

/// Decides which parts of OpenStreetMap we need for conflation.
pub struct Pruner<'a> {
    _coverage: &'a Coverage<'a>,
    keep_coords: U64Table,
    keep_ways: U64Table,
    keep_relations: U64Table,
    relation_members: U64Table,
}

/// Output of [prune_relations], the first  step of pruning.
struct PruneRelationsOutput {
    /// The OpenStreetMap relations we want to keep.
    ///
    /// For example, a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// tagged with `amenity=restaurant` becomes an element of this set,
    /// wherease a relation tagged with `boundary=administrative` gets omitted.
    ///
    /// As of July 2026, this set contains 0.9 million elements, which is
    /// 6.2% of the 14.6 million relations in OpenStreetMap.
    keep_relations: U64Table,

    /// The OpenStreetMap features that are members of any relation we want to keep,
    /// either directly or [indirectly](https://wiki.openstreetmap.org/wiki/Super-relation).
    ///
    /// For example, when a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// is tagged with `amenity=restaurant`, the various ways forming its interior holes
    /// and exterior boundary all become be part of this set.
    ///
    /// As of July 2026, this set contains 5.8 million elements, which is
    /// 0.05% of the 11.9 billion features in OpenStreetMap.
    relation_members: U64Table, // 2413085 nodes, 3368795 ways, 47213 relations
}

/// Output of [prune_ways], the second step of pruning.
struct PruneWaysOutput {
    /// The OpenStreetMap nodes whose coordinates we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`,
    /// its member nodes become part of this set. Likewise,
    /// when a relation or [super-relation](https://wiki.openstreetmap.org/wiki/Super-relation)
    /// is tagged as a hotel, all its supporting nodes get included.
    ///
    /// As of July 2026, this set contains 286.7 million nodes,
    /// which is 2.7% of the 10.7 billion nodes in OpenStreetMap.
    keep_coords: U64Table,

    /// The OpenStreetMap ways we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, it becomes part
    /// of this set, whereas a way tagged as `highway=residential` gets omitted.
    ///
    /// As of July 2026, this set contains 40.3 M elements, which is
    /// 3.3% of the 1.2 B ways in OpenStreetMap.
    keep_ways: U64Table,
}

impl<'a> Pruner<'a> {
    pub fn create(
        osm_reader: &mut BlobReader<File>,
        coverage: &'a Coverage<'a>,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<Pruner<'a>> {
        let rels_output = prune_relations(osm_reader, progress, workdir)?;
        let ways_output = prune_ways(osm_reader, &rels_output, progress, workdir)?;
        Ok(Pruner {
            _coverage: coverage,
            keep_coords: ways_output.keep_coords,
            keep_ways: ways_output.keep_ways,
            keep_relations: rels_output.keep_relations,
            relation_members: rels_output.relation_members,
        })
    }

    #[allow(unused)]
    pub fn keep_coord(&self, node_id: u64) -> bool {
        self.keep_coords.contains(node_id)
    }

    #[allow(unused)]
    pub fn keep_way(&self, id: u64) -> bool {
        self.keep_ways.contains(id) || self.relation_members.contains(id * 10 + 2)
    }

    #[allow(unused)]
    pub fn keep_relation(&self, id: u64) -> bool {
        self.keep_relations.contains(id) || self.relation_members.contains(id * 10 + 3)
    }
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_relations(
    reader: &mut BlobReader<File>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneRelationsOutput> {
    let rel_path = PathBuf::from(workdir).join("osm-prune.keep-relations");
    let rel_members_path = PathBuf::from(workdir).join("osm-prune.relation-members");
    if rel_path.exists() && rel_members_path.exists() {
        return Ok(PruneRelationsOutput {
            keep_relations: U64Table::open(&rel_path)?,
            relation_members: U64Table::open(&rel_members_path)?,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.rels  ",
        (reader.count_relation_blobs() as u64) * 2, // two passes
        "blobs",
    );

    // First pass.
    let (relations, graph) = prune_relations_pass_1(reader, &progress_bar, workdir, &rel_path)?;

    // Second pass.
    let rel_members = prune_relations_pass_2(
        reader,
        &relations,
        &graph,
        &progress_bar,
        workdir,
        &rel_members_path,
    )?;

    progress_bar.finish_with_message(format!(
        "blobs → {} relations with {} members",
        relations.len(),
        rel_members.len(),
    ));

    Ok(PruneRelationsOutput {
        keep_relations: relations,
        relation_members: rel_members,
    })
}

/// Pipeline step `osm.prune.rels`, pass 1 of 2.
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

/// Pipeline step `osm.prune.rels`, pass 2 of 2.
fn prune_relations_pass_2(
    reader: &mut BlobReader<File>,
    keep_1: &U64Table,
    graph: &GraphTable<'_>,
    progress_bar: &ProgressBar,
    workdir: &Path,
    out: &Path,
) -> Result<U64Table> {
    if out.exists() {
        return Ok(U64Table::open(out)?);
    }

    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_relation_blobs(blob_tx));
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(rel) = primitive
                        && graph.ancestors(rel.id).any(|id| keep_1.contains(id))
                    {
                        keep_tx.send(rel.id * 10 + 3)?;
                        for (_role_name, member_id, member_type) in rel.members() {
                            match member_type {
                                RelationMemberType::Node => keep_tx.send(member_id * 10 + 1)?,
                                RelationMemberType::Way => keep_tx.send(member_id * 10 + 2)?,
                                RelationMemberType::Relation => keep_tx.send(member_id * 10 + 3)?,
                            }
                        }
                    }
                }
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
    Ok(U64Table::open(out)?)
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_ways(
    reader: &mut BlobReader<File>,
    rels_output: &PruneRelationsOutput,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneWaysOutput> {
    let relation_members = &rels_output.relation_members;

    let keep_ways_path = PathBuf::from(workdir).join("osm-prune.keep-ways");
    let keep_coords_path = PathBuf::from(workdir).join("osm-prune.keep-coords");
    if keep_ways_path.exists() && keep_coords_path.exists() {
        return Ok(PruneWaysOutput {
            keep_ways: U64Table::open(&keep_ways_path)?,
            keep_coords: U64Table::open(&keep_coords_path)?,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.ways  ",
        reader.count_way_blobs() as u64,
        "blobs",
    );

    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (coords_tx, coords_rx) = sync_channel::<u64>(128 * 1024);
        let (ways_tx, ways_rx) = sync_channel::<u64>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_way_blobs(blob_tx));

        let coords_tx_1 = coords_tx.clone();
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Way(way) = primitive {
                        let mut mask = MatchMask::default();
                        for (key, value) in way.tags() {
                            mask.add_tag(key, value);
                        }

                        let way_feature_id = way.id * 10 + 2;
                        if !mask.is_empty() || relation_members.contains(way_feature_id) {
                            ways_tx.send(way.id)?;
                            for node_id in way.refs() {
                                if node_id > 0 {
                                    coords_tx_1.send(node_id as u64)?;
                                }
                            }
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        // We need the coordinates of all nodes that participate in any relation.
        let rel_member_collector = s.spawn(move || {
            for member_id in rels_output.relation_members.iter() {
                if member_id % 10 == 1 {
                    let node_id = member_id / 10;
                    coords_tx.send(node_id)?;
                }
            }
            Ok(())
        });
        let keep_ways_writer = s.spawn(|| u64_table::create(ways_rx, workdir, &keep_ways_path));
        let keep_coords_writer =
            s.spawn(|| u64_table::create(coords_rx, workdir, &keep_coords_path));

        keep_coords_writer
            .join()
            .expect("panic in keep_coords_writer")?;
        keep_ways_writer
            .join()
            .expect("panic in keep_ways_writer")?;
        rel_member_collector
            .join()
            .expect("panic in rel_member_collector")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;

    let keep_ways = U64Table::open(&keep_ways_path)?;
    let keep_coords = U64Table::open(&keep_coords_path)?;
    progress_bar.finish_with_message(format!(
        "blobs → {} ways, {} coords",
        keep_ways.len(),
        keep_coords.len()
    ));

    Ok(PruneWaysOutput {
        keep_ways,
        keep_coords,
    })
}
