use super::BlobReader;
use crate::{make_progress_bar, matchers::MatchMask, u64_table, u64_table::U64Table};
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

use graph::{Edge, Graph};

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
        "blobs → {} relations (with {} nodes, {} ways, {} relations as members)",
        keep_relations.len(),
        stats.node_count,
        stats.way_count,
        stats.relation_count,
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
) -> Result<(U64Table, Graph<'a>)> {
    let relation_graph_path = PathBuf::from(workdir).join("osm-prune.relation-graph");
    if keep_relations_path.exists() && relation_graph_path.exists() {
        let keep_relations = U64Table::open(keep_relations_path)?;
        let relations_graph = Graph::open(&relation_graph_path)?;
        return Ok((keep_relations, relations_graph));
    }

    let mut relations_graph: Option<Graph<'_>> = None;
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
            relations_graph = Some(Graph::create(
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
    graph: &Graph<'_>,
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

mod graph {
    use anyhow::{Ok, Result};
    use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
    use memmap2::Mmap;
    use serde::{Deserialize, Serialize};
    use std::{
        collections::{HashSet, VecDeque},
        fs::{File, remove_file, rename},
        io::{BufReader, BufWriter, Seek, SeekFrom, Write},
        ops::Range,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::SystemTime,
    };

    pub struct Graph<'a> {
        file: File,
        _mmap: Mmap,
        children: &'a [u64],
        parents: &'a [u64],
    }

    impl<'a> Graph<'a> {
        pub fn create(
            edges: impl Iterator<Item = Edge>,
            workdir: &Path,
            out: &Path,
        ) -> Result<Graph<'a>> {
            let mut writer = Writer::create(out)?;
            let num_edges = AtomicU64::new(0);
            let sorter: ExternalSorter<Edge, std::io::Error, LimitedBufferBuilder> =
                ExternalSorterBuilder::new()
                    .with_tmp_dir(workdir)
                    .with_buffer(LimitedBufferBuilder::new(
                        16 * 1024 * 1024,
                        /* preallocate */ true,
                    ))
                    .build()?;
            let sorted = sorter.sort(edges.map(|x| {
                num_edges.fetch_add(1, Ordering::SeqCst);
                std::io::Result::Ok(x)
            }))?;
            for edge in sorted {
                writer.write(edge?)?;
            }
            writer.close()?;
            Self::open(out)
        }

        #[cfg(target_pointer_width = "64")]
        pub fn open(path: &Path) -> Result<Graph<'a>> {
            let file = File::open(path)?;

            // SAFETY: We don’t truncate the file while it’s mapped into memory.
            let mmap = unsafe { Mmap::map(&file)? };
            let edge_count = usize::from_le_bytes(mmap[0..8].try_into().expect("edge_count"));
            let expected_size = 8 + edge_count * 16;
            if mmap.len() != expected_size {
                anyhow::bail!(
                    "{} has wrong file size, expected {}, got {}",
                    &path.display(),
                    expected_size,
                    mmap.len()
                );
            }

            // SAFETY: mmap.len() checked above.
            let children = unsafe {
                let ptr = mmap.as_ptr().add(8) as *const u64;
                std::slice::from_raw_parts(ptr, edge_count)
            };

            // SAFETY: mmap.len() checked above.
            let parents = unsafe {
                let ptr = mmap.as_ptr().add(8 + edge_count * 8) as *const u64;
                std::slice::from_raw_parts(ptr, edge_count)
            };

            Ok(Graph {
                file,
                _mmap: mmap,
                children,
                parents,
            })
        }

        #[allow(unused)]
        pub fn modified(&'a self) -> Result<SystemTime> {
            Ok(self.file.metadata()?.modified()?)
        }

        /// Returns an iterator over the reflexive transitive closure of the
        /// child-parent relation, starting at `start`. Each node is yielded at
        /// most once, so the iterator terminates even if the graph is cyclic.
        pub fn ancestors(&'a self, start: u64) -> impl Iterator<Item = u64> + 'a {
            let mut visited = HashSet::with_capacity(5);
            visited.insert(start);
            AncestorIter {
                graph: self,
                queue: VecDeque::from([start]),
                visited,
            }
        }

        /// Reads element `idx` of `children`, correcting for the fact that the
        /// underlying bytes are always little-endian on disk/in the mmap.
        #[inline]
        fn child_at(&self, idx: usize) -> u64 {
            u64::from_le(self.children[idx])
        }

        #[inline]
        fn parent_at(&self, idx: usize) -> u64 {
            u64::from_le(self.parents[idx])
        }

        /// Returns the range of indices `[lo, hi)` in `children` (and
        /// correspondingly in `parents`) whose child id equals `node`.
        /// Relies on `children` being sorted ascending (in native-value terms).
        fn parent_range(&self, child: u64) -> Range<usize> {
            let lo = self.children.partition_point(|&c| u64::from_le(c) < child);

            let mut hi = lo;
            while hi < self.children.len() && self.child_at(hi) == child {
                hi += 1;
            }

            lo..hi
        }
    }

    #[derive(Serialize, Deserialize, Ord, PartialOrd, PartialEq, Eq)]
    pub struct Edge {
        pub child: u64,
        pub parent: u64,
    }

    struct AncestorIter<'a> {
        graph: &'a Graph<'a>,
        queue: VecDeque<u64>,
        visited: HashSet<u64>,
    }

    impl<'a> Iterator for AncestorIter<'a> {
        type Item = u64;

        fn next(&mut self) -> Option<Self::Item> {
            let child = self.queue.pop_front()?;

            let range = self.graph.parent_range(child);
            for idx in range {
                let parent = self.graph.parent_at(idx);
                if self.visited.insert(parent) {
                    self.queue.push_back(parent);
                }
            }

            Some(child)
        }
    }

    pub struct Writer {
        edge_count: u64,
        path: PathBuf,
        tmp_path: PathBuf,
        out: BufWriter<File>,
        parents_out: BufWriter<File>,
    }

    impl Writer {
        pub fn create(path: &Path) -> Result<Writer> {
            let mut tmp_path = PathBuf::from(path);
            tmp_path.add_extension("tmp");
            let mut out = BufWriter::with_capacity(32768, File::create(&tmp_path)?);
            out.write_all(&0_u64.to_le_bytes())?;

            let parents_file = File::create(Self::parents_path(path))?;

            Ok(Writer {
                edge_count: 0,
                path: PathBuf::from(path),
                tmp_path,
                out,
                parents_out: BufWriter::with_capacity(32768, parents_file),
            })
        }

        pub fn write(&mut self, edge: Edge) -> Result<()> {
            self.edge_count += 1;
            self.out.write_all(&edge.child.to_le_bytes())?;
            self.parents_out.write_all(&edge.parent.to_le_bytes())?;
            Ok(())
        }

        pub fn close(mut self) -> Result<()> {
            let parents_path = Self::parents_path(&self.path);
            self.parents_out.flush()?;
            let parents_file = self.parents_out.into_inner()?;
            parents_file.sync_all()?;
            drop(parents_file);

            let mut reader = BufReader::new(File::open(&parents_path)?);
            std::io::copy(&mut reader, &mut self.out)?;
            remove_file(&parents_path)?;
            drop(parents_path);

            self.out.seek(SeekFrom::Start(0))?;
            self.out.write_all(&self.edge_count.to_le_bytes())?;

            self.out.flush()?;
            self.out.into_inner()?.sync_all()?;
            rename(&self.tmp_path, &self.path)?;
            Ok(())
        }

        fn parents_path(path: &Path) -> PathBuf {
            let mut p = PathBuf::from(path);
            p.add_extension("parents.tmp");
            p
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use tempfile::TempDir;

        #[test]
        fn test_graph() -> Result<()> {
            let edges: Vec<(u64, u64)> = vec![
                (1, 2),
                (2, 3),
                (2, 4),
                (4, 5),
                (4, 6),
                // Cycle: 21 -> 22 -> 23 -> 21.
                (21, 22),
                (22, 23),
                (23, 21),
            ];
            let edges_iter = edges
                .into_iter()
                .map(|(child, parent)| Edge { child, parent });
            let workdir = TempDir::new()?;
            let path = workdir.path().join("testgraph");
            let graph = Graph::create(edges_iter, &workdir.path(), &path)?;
            assert_eq!(graph.modified()?, std::fs::metadata(&path)?.modified()?);
            assert_eq!(
                graph.ancestors(1).collect::<Vec<u64>>(),
                &[1, 2, 3, 4, 5, 6]
            );
            assert_eq!(graph.ancestors(2).collect::<Vec<u64>>(), &[2, 3, 4, 5, 6]);
            assert_eq!(graph.ancestors(4).collect::<Vec<u64>>(), &[4, 5, 6]);
            assert_eq!(graph.ancestors(21).collect::<Vec<u64>>(), &[21, 22, 23]);
            assert_eq!(graph.ancestors(22).collect::<Vec<u64>>(), &[22, 23, 21]);
            assert_eq!(graph.ancestors(23).collect::<Vec<u64>>(), &[23, 21, 22]);
            assert_eq!(graph.ancestors(7777).collect::<Vec<u64>>(), &[7777]);
            Ok(())
        }
    }
}
