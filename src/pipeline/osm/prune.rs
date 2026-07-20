use super::BlobReader;
use crate::{
    coverage::Coverage,
    make_progress_bar,
    matchers::MatchMask,
    tables::{CoordsMap, Edge, GraphTable, StringCounts},
    u64_table,
    u64_table::U64Table,
};
use anyhow::{Ok, Result};
use geo::Coord;
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
#[allow(unused)]
pub struct PruneOutput<'a> {
    _coverage: &'a Coverage<'a>,
    coords: CoordsMap<'a>,
    strings: StringCounts<'a>,
    keep_ways: U64Table,
    keep_relations: U64Table,
    relation_members: U64Table,
}

/// Output of [prune_relations], the first  step of pruning.
struct PruneRelationsOutput<'a> {
    /// The IDs of the OpenStreetMap relations we want to keep.
    ///
    /// For example, a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// tagged with `amenity=restaurant` becomes an element of this set,
    /// wherease a relation tagged with `boundary=administrative` gets omitted.
    ///
    /// As of July 2026, this set contains 0.9 million IDs, which is
    /// 6.2% of the 14.6 million relations in OpenStreetMap.
    keep_relations: U64Table,

    /// The IDs of the OpenStreetMap features that are members of any relation we want to keep,
    /// either directly or [indirectly](https://wiki.openstreetmap.org/wiki/Super-relation).
    ///
    /// For example, when a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// is tagged with `amenity=restaurant`, the various ways forming its interior holes
    /// and exterior boundary all become be part of this set.
    ///
    /// As of July 2026, this set contains 5.8 million IDs, which is
    /// 0.05% of the 11.9 billion features in OpenStreetMap.
    relation_members: U64Table, // 2413085 nodes, 3368795 ways, 47213 relations

    /// The strings that appear the ways and relations we want to keep, and how
    /// often each string gets used. Later down the pipeline, we need these
    /// counters to construct a string pool where more the most frequent strings
    /// get assigned lower numbers.
    ///
    /// For example, when a relation is tagged with `tourism=hotel`,
    /// the strings `"tourism"` and `"hotel"` get added to this counter.
    ///
    /// As of July 2026, this counter contains 1.03 million unique strings,
    /// which is 0.53% of the 194.3 million unique tags in OpenStreetMap.
    strings: StringCounts<'a>,
}

/// Output of [prune_ways], the second step of pruning.
struct PruneWaysOutput<'a> {
    /// The IDs of the OpenStreetMap nodes whose coordinates we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, the IDs
    /// of its member nodes become part of this set. Likewise, when a relation
    /// or [super-relation](https://wiki.openstreetmap.org/wiki/Super-relation)
    /// is tagged as a hotel, the IDs of all its supporting nodes get included.
    ///
    /// As of July 2026, this set contains 286.7 million node IDs,
    /// which is 2.7% of the 10.7 billion nodes in OpenStreetMap.
    keep_coords: U64Table,

    /// The IDs of the OpenStreetMap ways we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, it becomes part
    /// of this set, whereas a way tagged as `highway=residential` gets omitted.
    ///
    /// As of July 2026, this set contains 40.3 million way IDs, which is
    /// 3.3% of the 1.2 billion ways in OpenStreetMap.
    keep_ways: U64Table,

    /// The strings that appear the ways and relations we want to keep, and how
    /// often each string gets used. Later down the pipeline, we need these
    /// counters to construct a string pool where more the most frequent strings
    /// get assigned lower numbers.
    ///
    /// For example, when a way or relation is tagged with `tourism=hotel`,
    /// the strings `"tourism"` and `"hotel"` get added to this counter.
    ///
    /// As of July 2026, this counter contains 9.3 million unique strings,
    /// which is 4.8% of the 194.3 million unique tags in OpenStreetMap.
    strings: StringCounts<'a>,
}

/// Output of [prune_nodes], the third step of pruning.
struct PruneNodesOutput<'a> {
    /// The coordinates we want to keep, keyed by OpenStreetMap node ID.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, the coordinates
    /// of its member nodes become part of this table. Likewise, when a relation
    /// or [super-relation](https://wiki.openstreetmap.org/wiki/Super-relation)
    /// is tagged as a hotel, the coordiantes of all its supporting nodes get
    /// included.
    ///
    /// As of July 2026, this map contains coordinates for 286.7 million nodes,
    /// which is 2.7% of the 10.7 billion node coordinates in OpenStreetMap.
    coords: CoordsMap<'a>,

    /// The strings we want to keep, and how often each string gets used.
    /// Later down the pipeline, we need these counters to construct a string pool
    /// where more the most frequent strings get assigned lower numbers.
    ///
    /// For example, when a node, way or relation is tagged with `tourism=hotel`,
    /// the strings `"tourism"` and `"hotel"` get added to this counter.
    /// The strings also include relation roles such as `"inner"`.
    ///
    /// As of July 2026, this counter contains 30.8 million unique strings,
    /// which is 15.9% of the 194.3 million unique tags in OpenStreetMap.
    strings: StringCounts<'a>,
}

impl<'a> PruneOutput<'a> {
    pub fn create(
        osm_reader: &mut BlobReader<File>,
        coverage: &'a Coverage<'a>,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<PruneOutput<'a>> {
        let rels_output = prune_relations(osm_reader, progress, workdir)?;
        let ways_output = prune_ways(osm_reader, &rels_output, progress, workdir)?;
        let nodes_output = prune_nodes(osm_reader, &ways_output, progress, workdir)?;
        Ok(PruneOutput {
            _coverage: coverage,
            coords: nodes_output.coords,
            strings: nodes_output.strings,
            keep_ways: ways_output.keep_ways,
            keep_relations: rels_output.keep_relations,
            relation_members: rels_output.relation_members,
        })
    }

    #[allow(unused)]
    pub fn coord(&self, node_id: u64) -> Option<Coord> {
        self.coords.get(node_id)
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
fn prune_relations<'a>(
    reader: &mut BlobReader<File>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneRelationsOutput<'a>> {
    let rel_path = PathBuf::from(workdir).join("osm-prune.keep-relations");
    let rel_members_path = PathBuf::from(workdir).join("osm-prune.relation-members");
    let strings_path = PathBuf::from(workdir).join("osm-prune-rels.strings");
    if rel_path.exists() && rel_members_path.exists() && strings_path.exists() {
        return Ok(PruneRelationsOutput {
            keep_relations: U64Table::open(&rel_path)?,
            relation_members: U64Table::open(&rel_members_path)?,
            strings: StringCounts::open(&strings_path)?,
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
    let (rel_members, strings) = prune_relations_pass_2(
        reader,
        &relations,
        &graph,
        &progress_bar,
        workdir,
        &rel_members_path,
    )?;

    progress_bar.finish_with_message(format!(
        "blobs → {} relations with {} members, {} strings",
        relations.len(),
        rel_members.len(),
        strings.len(),
    ));

    Ok(PruneRelationsOutput {
        keep_relations: relations,
        relation_members: rel_members,
        strings,
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
fn prune_relations_pass_2<'a>(
    reader: &mut BlobReader<File>,
    keep_1: &U64Table,
    graph: &GraphTable<'_>,
    progress_bar: &ProgressBar,
    workdir: &Path,
    out: &Path,
) -> Result<(U64Table, StringCounts<'a>)> {
    let strings_path = workdir.join("osm-prune-rels.strings");
    if out.exists() && strings_path.exists() {
        let rel_members = U64Table::open(out)?;
        let strings = StringCounts::open(&strings_path)?;
        return Ok((rel_members, strings));
    }

    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (strings_tx, strings_rx) = sync_channel::<(String, u64)>(32 * 1024);
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
                        for (role_name, member_id, member_type) in rel.members() {
                            strings_tx.send((String::from(role_name), 1))?;
                            match member_type {
                                RelationMemberType::Node => keep_tx.send(member_id * 10 + 1)?,
                                RelationMemberType::Way => keep_tx.send(member_id * 10 + 2)?,
                                RelationMemberType::Relation => keep_tx.send(member_id * 10 + 3)?,
                            }
                        }
                        for (tag_key, tag_value) in rel.tags() {
                            strings_tx.send((String::from(tag_key), 1))?;
                            strings_tx.send((String::from(tag_value), 1))?;
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });
        let keep_writer = s.spawn(|| u64_table::create(keep_rx, workdir, out));

        let strings_writer =
            s.spawn(|| StringCounts::create(strings_rx.into_iter(), workdir, &strings_path));

        strings_writer.join().expect("panic in strings_writer")?;
        keep_writer.join().expect("panic in keep_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;

    let rel_members = U64Table::open(out)?;
    let strings = StringCounts::open(&strings_path)?;
    Ok((rel_members, strings))
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_ways<'a>(
    reader: &mut BlobReader<File>,
    rels_output: &PruneRelationsOutput,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneWaysOutput<'a>> {
    let relation_members = &rels_output.relation_members;

    let keep_ways_path = PathBuf::from(workdir).join("osm-prune.keep-ways");
    let keep_coords_path = PathBuf::from(workdir).join("osm-prune.keep-coords");
    let strings_path = PathBuf::from(workdir).join("osm-prune-ways.strings");
    if keep_ways_path.exists() && keep_coords_path.exists() && strings_path.exists() {
        return Ok(PruneWaysOutput {
            keep_ways: U64Table::open(&keep_ways_path)?,
            keep_coords: U64Table::open(&keep_coords_path)?,
            strings: StringCounts::open(&strings_path)?,
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
        let (coords_tx, coords_rx) = sync_channel::<u64>(64 * 1024);
        let (strings_tx, strings_rx) = sync_channel::<(String, u64)>(64 * 1024);
        let (ways_tx, ways_rx) = sync_channel::<u64>(64 * 1024);
        let blob_producer = s.spawn(|| reader.send_way_blobs(blob_tx));

        let coords_tx_1 = coords_tx.clone(); // ownership moved into blob_consumer
        let strings_tx_1 = strings_tx.clone(); // ownership moved into blob_consumer
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
                            for (tag_key, tag_value) in way.tags() {
                                strings_tx_1.send((String::from(tag_key), 1))?;
                                strings_tx_1.send((String::from(tag_value), 1))?;
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

        // Merge [PruneRelationsOutput.strings] into our own string counts.
        let rel_strings_reader = s.spawn(move || {
            for (s, count) in rels_output.strings.iter() {
                strings_tx.send((String::from(s), count))?;
            }
            Ok(())
        });

        let strings_writer =
            s.spawn(|| StringCounts::create(strings_rx.into_iter(), workdir, &strings_path));

        strings_writer.join().expect("panic in strings_writer")?;
        keep_coords_writer
            .join()
            .expect("panic in keep_coords_writer")?;
        keep_ways_writer
            .join()
            .expect("panic in keep_ways_writer")?;
        rel_strings_reader
            .join()
            .expect("panic in rel_strings_reader")?;
        rel_member_collector
            .join()
            .expect("panic in rel_member_collector")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;

    let keep_ways = U64Table::open(&keep_ways_path)?;
    let keep_coords = U64Table::open(&keep_coords_path)?;
    let strings = StringCounts::open(&strings_path)?;
    progress_bar.finish_with_message(format!(
        "blobs → {} ways, {} coords, {} strings",
        keep_ways.len(),
        keep_coords.len(),
        strings.len()
    ));

    Ok(PruneWaysOutput {
        keep_ways,
        keep_coords,
        strings,
    })
}

fn prune_nodes<'a>(
    reader: &mut BlobReader<File>,
    ways_output: &PruneWaysOutput,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneNodesOutput<'a>> {
    let keep_coords = &ways_output.keep_coords;
    let coords_path = PathBuf::from(workdir).join("osm-prune.coords");
    let strings_path = PathBuf::from(workdir).join("osm-prune-nodes.strings");
    if coords_path.exists() && strings_path.exists() {
        let coords = CoordsMap::open(&coords_path)?;
        let strings = StringCounts::open(&strings_path)?;
        return Ok(PruneNodesOutput { coords, strings });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.nodes ",
        reader.count_node_blobs() as u64,
        "blobs",
    );

    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (coords_tx, coords_rx) = sync_channel::<(u64, Coord)>(64 * 1024);
        let (strings_tx, strings_rx) = sync_channel::<(String, u64)>(64 * 1024);
        let producer = s.spawn(|| reader.send_node_blobs(blob_tx));

        let strings_tx_1 = strings_tx.clone(); // ownership moved into consumer
        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Node(node) = primitive {
                        let node_id = node.id;
                        if keep_coords.contains(node_id) {
                            coords_tx.send((
                                node_id,
                                Coord {
                                    x: node.lon,
                                    y: node.lat,
                                },
                            ))?;
                        }

                        let keep_node = {
                            let mut mask = MatchMask::default();
                            for (key, value) in node.tags.iter() {
                                mask.add_tag(key, value);
                            }
                            !mask.is_empty()
                        };
                        if keep_node {
                            // TODO: Measure whether it makes any difference in performance
                            // or memory consupmtion to collect counts separately per blob,
                            // and only send aggregated counts over the channel. If it does,
                            // change the code also for ways and relations.
                            for (key, value) in node.tags {
                                strings_tx_1.send((String::from(key), 1))?;
                                strings_tx_1.send((String::from(value), 1))?;
                            }
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        // Merge [PruneWaysOutput.strings] (which already contain the counts
        // for relations) into our own string counts.
        let way_strings_reader = s.spawn(move || {
            for (s, count) in ways_output.strings.iter() {
                strings_tx.send((String::from(s), count))?;
            }
            Ok(())
        });

        let coords_writer =
            s.spawn(|| CoordsMap::create(coords_rx.into_iter(), workdir, &coords_path));

        let strings_writer =
            s.spawn(|| StringCounts::create(strings_rx.into_iter(), workdir, &strings_path));

        strings_writer.join().expect("panic in strings_writer")?;
        coords_writer.join().expect("panic in coords_writer")?;
        way_strings_reader
            .join()
            .expect("panic in way_strings_reader")?;
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;

        Ok(())
    })?;

    let coords = CoordsMap::open(&coords_path)?;
    let strings = StringCounts::open(&strings_path)?;
    progress_bar.finish_with_message(format!(
        "blobs → {} coords, {} strings",
        coords.len(),
        strings.len()
    ));
    Ok(PruneNodesOutput { coords, strings })
}
