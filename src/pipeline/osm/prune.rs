use super::{BlobReader, decode_feature_id, encode_feature_id};
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    pipeline::EXTERNAL_SORT_CHUNK_BYTES,
    tables::{CoordTable, Edge, GraphTable, StringCounts, U64Set},
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

/// Which parts of OpenStreetMap we need for conflation.
#[allow(unused)]
pub struct Prunings<'a> {
    pub coords: CoordTable<'a>,
    pub strings: StringCounts<'a>,
    pub keep_nodes: U64Set,
    pub keep_ways: U64Set,
    pub keep_relations: U64Set,
    pub relation_members: U64Set,
    pub relation_graph: GraphTable<'a>,
}

/// Output of [prune_relations], the first  step of pruning.
struct PruneRelationsOutput<'a> {
    /// The IDs of OpenStreetMap relations we want to keep.
    ///
    /// For example, a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// tagged with `amenity=restaurant` becomes an element of this set,
    /// wherease a relation tagged with `boundary=administrative` gets omitted.
    ///
    /// As of July 2026, this set contains 0.9 million IDs, which is
    /// 6.2% of the 14.6 million relations in OpenStreetMap.
    keep_relations: U64Set,

    /// The IDs of OpenStreetMap features that are members of any relation we want to keep,
    /// either directly or [indirectly](https://wiki.openstreetmap.org/wiki/Super-relation).
    ///
    /// For example, when a
    /// [multipolygon](https://wiki.openstreetmap.org/wiki/Relation:multipolygon)
    /// is tagged with `amenity=restaurant`, the various ways forming its interior holes
    /// and exterior boundary all become be part of this set.
    ///
    /// As of July 2026, this set contains 5.8 million IDs, which is
    /// 0.05% of the 11.9 billion features in OpenStreetMap.
    relation_members: U64Set, // 2413085 nodes, 3368795 ways, 47213 relations

    /// The containment graph between OpenStreetMap relations: Which “child”
    /// relation is itself a member of another, “parent” or
    /// [“super“](https://wiki.openstreetmap.org/wiki/Super-relation),
    /// relation.
    relation_graph: GraphTable<'a>,

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
    /// The IDs of OpenStreetMap nodes whose coordinates we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, the IDs
    /// of its member nodes become part of this set. Likewise, when a relation
    /// or [super-relation](https://wiki.openstreetmap.org/wiki/Super-relation)
    /// is tagged as a hotel, the IDs of all its supporting nodes get included.
    ///
    /// As of July 2026, this set contains 286.7 million node IDs,
    /// which is 2.7% of the 10.7 billion nodes in OpenStreetMap.
    keep_coords: U64Set,

    /// The IDs of OpenStreetMap ways we want to keep.
    ///
    /// For example, when a way is tagged with `tourism=hotel`, it becomes part
    /// of this set, whereas a way tagged as `highway=residential` gets omitted.
    ///
    /// As of July 2026, this set contains 40.3 million way IDs, which is
    /// 3.3% of the 1.2 billion ways in OpenStreetMap.
    keep_ways: U64Set,

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
    /// The IDs of OpenStreetMap nodes we want to keep, for example
    /// nodes tagged with `tourism=hotel`.
    ///
    /// As of July 2026, this set contains 122.5 million node IDs,
    /// which is 1.1% of the 10.7 billion nodes in OpenStreetMap,
    /// or 41.8% of the 292.9 million OSM nodes with at least one tag.
    keep_nodes: U64Set,

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
    coords: CoordTable<'a>,

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

impl<'a> Prunings<'a> {
    pub fn create(
        osm_reader: &mut BlobReader<File>,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<Prunings<'a>> {
        let rels_output = prune_relations(osm_reader, progress, workdir)?;
        let ways_output = prune_ways(osm_reader, &rels_output, progress, workdir)?;
        let nodes_output = prune_nodes(osm_reader, &ways_output, progress, workdir)?;
        Ok(Prunings {
            coords: nodes_output.coords,
            strings: nodes_output.strings,
            keep_nodes: nodes_output.keep_nodes,
            keep_ways: ways_output.keep_ways,
            keep_relations: rels_output.keep_relations,
            relation_members: rels_output.relation_members,
            relation_graph: rels_output.relation_graph,
        })
    }

    #[allow(unused)]
    pub fn coord(&self, node_id: u64) -> Option<Coord> {
        self.coords.get(node_id)
    }

    #[allow(unused)]
    pub fn keep_way(&self, id: u64) -> bool {
        self.keep_ways.contains(id)
            || self
                .relation_members
                .contains(encode_feature_id(RelationMemberType::Way, id))
    }

    #[allow(unused)]
    pub fn keep_relation(&self, id: u64) -> bool {
        self.keep_relations.contains(id)
            || self
                .relation_members
                .contains(encode_feature_id(RelationMemberType::Relation, id))
    }
}

/// Runs the pipeline step `osm.prune.rels`.
fn prune_relations<'a>(
    reader: &mut BlobReader<File>,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PruneRelationsOutput<'a>> {
    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.rels  ",
        (reader.count_relation_blobs() as u64) * 2, // two passes
        "blobs",
    );

    let (relations, rel_graph) = prune_relations_pass_1(reader, &progress_bar, workdir)?;
    let (rel_members, strings) =
        prune_relations_pass_2(reader, &relations, &rel_graph, &progress_bar, workdir)?;

    progress_bar.finish_with_message(format!(
        "blobs → {} relations with {} members, {} graph edges, {} strings",
        relations.len(),
        rel_members.len(),
        rel_graph.edge_count(),
        strings.len(),
    ));

    Ok(PruneRelationsOutput {
        keep_relations: relations,
        relation_members: rel_members,
        relation_graph: rel_graph,
        strings,
    })
}

/// Pipeline step `osm.prune.rels`, pass 1 of 2.
fn prune_relations_pass_1<'a>(
    reader: &mut BlobReader<File>,
    progress_bar: &ProgressBar,
    workdir: &Path,
) -> Result<(U64Set, GraphTable<'a>)> {
    let keep_relations_path = workdir.join("osm-prune.keep-relations");
    let relation_graph_path = workdir.join("osm-prune.relation-graph");
    if keep_relations_path.exists() && relation_graph_path.exists() {
        let keep_relations = U64Set::open(&keep_relations_path)?;
        let relations_graph = GraphTable::open(&relation_graph_path)?;
        return Ok((keep_relations, relations_graph));
    }

    let mut relations_graph: Option<GraphTable<'_>> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(1024);
        let (edge_tx, edge_rx) = sync_channel::<Edge>(1024);
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
        // keep_writer and graph_writer each run their own external sort
        // concurrently in this scope -- split the chunk-size budget
        // between the two so their combined peak memory stays within one
        // EXTERNAL_SORT_CHUNK_BYTES, not double it.
        const CONCURRENT_SORTS: usize = 2;
        let keep_writer = s.spawn(|| {
            U64Set::create(
                keep_rx.into_iter(),
                workdir,
                &keep_relations_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )
        });
        let graph_writer = s.spawn(|| {
            relations_graph = Some(GraphTable::create(
                edge_rx.into_iter(),
                workdir,
                &relation_graph_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )?);
            Ok(())
        });
        keep_writer.join().expect("panic in keep_writer")?;
        graph_writer.join().expect("panic in graph_writer")?;
        blob_consumer.join().expect("panic in consumer")?;
        blob_producer.join().expect("panic in producer")?;
        Ok(())
    })?;

    let keep_relations = U64Set::open(&keep_relations_path)?;
    Ok((keep_relations, relations_graph.expect("graph")))
}

/// Pipeline step `osm.prune.rels`, pass 2 of 2.
fn prune_relations_pass_2<'a>(
    reader: &mut BlobReader<File>,
    keep_1: &U64Set,
    graph: &GraphTable<'_>,
    progress_bar: &ProgressBar,
    workdir: &Path,
) -> Result<(U64Set, StringCounts<'a>)> {
    let rel_members_path = workdir.join("osm-prune.relation-members");
    let strings_path = workdir.join("osm-prune-rels.strings");
    if rel_members_path.exists() && strings_path.exists() {
        let rel_members = U64Set::open(&rel_members_path)?;
        let strings = StringCounts::open(&strings_path)?;
        return Ok((rel_members, strings));
    }

    let mut rel_members: Option<U64Set> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (strings_tx, strings_rx) = sync_channel::<(String, u64)>(1024);
        let (keep_tx, keep_rx) = sync_channel::<u64>(1024);
        let blob_producer = s.spawn(|| reader.send_relation_blobs(blob_tx));
        let blob_consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(rel) = primitive
                        && graph.ancestors(rel.id).any(|id| keep_1.contains(id))
                    {
                        keep_tx.send(encode_feature_id(RelationMemberType::Relation, rel.id))?;
                        for (role_name, member_id, member_type) in rel.members() {
                            strings_tx.send((String::from(role_name), 1))?;
                            keep_tx.send(encode_feature_id(member_type, member_id))?;
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

        // keep_writer and strings_writer each run their own external sort
        // concurrently in this scope -- split the chunk-size budget
        // between the two so their combined peak memory stays within one
        // EXTERNAL_SORT_CHUNK_BYTES, not double it.
        const CONCURRENT_SORTS: usize = 2;
        let keep_writer = s.spawn(|| {
            rel_members = Some(U64Set::create(
                keep_rx.into_iter(),
                workdir,
                &rel_members_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )?);
            Ok(())
        });

        let strings_writer = s.spawn(|| {
            StringCounts::create(
                strings_rx.into_iter(),
                workdir,
                &strings_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )
        });

        strings_writer.join().expect("panic in strings_writer")?;
        keep_writer.join().expect("panic in keep_writer")?;
        blob_consumer.join().expect("panic in blob_consumer")?;
        blob_producer.join().expect("panic in blob_producer")?;
        Ok(())
    })?;

    let strings = StringCounts::open(&strings_path)?;
    Ok((rel_members.expect("rel_members"), strings))
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
            keep_ways: U64Set::open(&keep_ways_path)?,
            keep_coords: U64Set::open(&keep_coords_path)?,
            strings: StringCounts::open(&strings_path)?,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.ways  ",
        reader.count_way_blobs() as u64,
        "blobs",
    );

    let mut keep_coords: Option<U64Set> = None;
    let mut keep_ways: Option<U64Set> = None;
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

                        let way_feature_id = encode_feature_id(RelationMemberType::Way, way.id);
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
                if let Some((RelationMemberType::Node, node_id)) = decode_feature_id(member_id) {
                    coords_tx.send(node_id)?;
                }
            }
            Ok(())
        });
        // keep_ways_writer, keep_coords_writer, and strings_writer (below)
        // each run their own external sort concurrently in this scope --
        // split the chunk-size budget three ways so their combined peak
        // memory stays within one EXTERNAL_SORT_CHUNK_BYTES, not triple it.
        const CONCURRENT_SORTS: usize = 3;
        let keep_ways_writer = s.spawn(|| {
            keep_ways = Some(U64Set::create(
                ways_rx.into_iter(),
                workdir,
                &keep_ways_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )?);
            Ok(())
        });
        let keep_coords_writer = s.spawn(|| {
            keep_coords = Some(U64Set::create(
                coords_rx.into_iter(),
                workdir,
                &keep_coords_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )?);
            Ok(())
        });

        // Merge [PruneRelationsOutput.strings] into our own string counts.
        let rel_strings_reader = s.spawn(move || {
            for (s, count) in rels_output.strings.iter() {
                strings_tx.send((String::from(s), count))?;
            }
            Ok(())
        });

        let strings_writer = s.spawn(|| {
            StringCounts::create(
                strings_rx.into_iter(),
                workdir,
                &strings_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )
        });

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

    let keep_ways = keep_ways.expect("keep_ways");
    let keep_coords = keep_coords.expect("keep_coords");
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
    let keep_nodes_path = PathBuf::from(workdir).join("osm-prune.keep-nodes");
    let coords_path = PathBuf::from(workdir).join("osm-prune.coords");
    let strings_path = PathBuf::from(workdir).join("osm-prune.strings");
    if keep_nodes_path.exists() && coords_path.exists() && strings_path.exists() {
        let keep_nodes = U64Set::open(&keep_nodes_path)?;
        let coords = CoordTable::open(&coords_path)?;
        let strings = StringCounts::open(&strings_path)?;
        return Ok(PruneNodesOutput {
            keep_nodes,
            coords,
            strings,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.prune.nodes ",
        reader.count_node_blobs() as u64,
        "blobs",
    );

    let mut keep_nodes: Option<U64Set> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (keep_tx, keep_rx) = sync_channel::<u64>(64 * 1024);
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
                            keep_tx.send(node.id)?;
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

        // strings_writer, coords_writer, and keep_nodes_writer each run
        // their own external sort concurrently in this scope -- split the
        // chunk-size budget three ways so their combined peak memory
        // stays within one EXTERNAL_SORT_CHUNK_BYTES, not triple it.
        const CONCURRENT_SORTS: usize = 3;
        let strings_writer = s.spawn(|| {
            StringCounts::create(
                strings_rx.into_iter(),
                workdir,
                &strings_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )
        });
        let coords_writer = s.spawn(|| {
            CoordTable::create(
                coords_rx.into_iter(),
                workdir,
                &coords_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )
        });
        let keep_nodes_writer = s.spawn(|| {
            keep_nodes = Some(U64Set::create(
                keep_rx.into_iter(),
                workdir,
                &keep_nodes_path,
                EXTERNAL_SORT_CHUNK_BYTES / CONCURRENT_SORTS,
            )?);
            Ok(())
        });

        strings_writer.join().expect("panic in strings_writer")?;
        coords_writer.join().expect("panic in coords_writer")?;
        keep_nodes_writer
            .join()
            .expect("panic in keep_nodes_writer")?;
        way_strings_reader
            .join()
            .expect("panic in way_strings_reader")?;
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;

        Ok(())
    })?;

    let keep_nodes = keep_nodes.expect("keep_nodes");
    let coords = CoordTable::open(&coords_path)?;
    let strings = StringCounts::open(&strings_path)?;
    progress_bar.finish_with_message(format!(
        "blobs → {} nodes, {} coords, {} strings",
        keep_nodes.len(),
        coords.len(),
        strings.len()
    ));
    Ok(PruneNodesOutput {
        keep_nodes,
        coords,
        strings,
    })
}
