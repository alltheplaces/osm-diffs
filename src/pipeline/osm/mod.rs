use anyhow::{Context, Ok, Result, anyhow};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use protobuf_iter::MessageIter;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

use crate::coverage::Coverage;
use crate::make_progress_bar;

mod assemble;
mod coords;
mod cover;
mod fetch;
mod filter;
mod index;
mod prune;

use filter::FilteredFeatureStore;
use index::Index;
use prune::Prunings;

pub fn import_osm(
    coverage: &Path,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<(PathBuf, Box<dyn FeatureStore>)> {
    assert!(workdir.exists());

    let out_path = workdir.join("osm.parquet");
    if out_path.exists() && FilteredFeatureStore::exists(workdir) {
        let store = FilteredFeatureStore::open(workdir)?;
        return Ok((out_path, Box::new(store)));
    }

    let pbf = fetch::fetch_planet(progress, workdir)?;
    let pbf_error = || format!("could not open file `{:?}`", pbf);
    let mut file = File::open(&pbf).with_context(pbf_error)?;
    let mut reader = BlobReader::open(&mut file).with_context(pbf_error)?;

    let prunings = Prunings::create(&mut reader, progress, workdir)?;
    let _index = Index::create(&mut reader, &prunings, progress, workdir)?;
    if false {
        todo!();
    }

    // TODO: Remove the old version of the pipeline (everything below),
    // once the new code actually works.
    let coverage = Coverage::load(coverage)
        .with_context(|| format!("could not open coverage file `{:?}`", coverage))?;

    let relation_parents = build_relation_parents(&mut reader, progress)?;

    // Find which nodes, ways and relations lie within the coverage.
    let covered_nodes = cover::cover_nodes(&mut reader, &coverage, progress, workdir)?;
    let covered_ways = cover::cover_ways(&mut reader, &covered_nodes, progress, workdir)?;
    let covered_relations = cover::cover_relations(
        &mut reader,
        &covered_nodes,
        &covered_ways,
        &relation_parents,
        progress,
        workdir,
    )?;

    let relations = filter::filter_relations(
        &mut reader,
        &coverage,
        &covered_relations,
        progress,
        workdir,
    )?;

    let ways = filter::filter_ways(
        &mut reader,
        &coverage,
        &covered_ways,
        &relations,
        progress,
        workdir,
    )?;

    let nodes = filter::filter_nodes(
        &mut reader,
        &coverage,
        &covered_nodes,
        &ways,
        &relations,
        progress,
        workdir,
    )?;

    let feature_store = filter::FilteredFeatureStore::new(nodes, ways, relations);
    assemble::assemble(&feature_store, progress, workdir, &out_path)?;

    Ok((out_path, Box::new(feature_store)))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Node {
    id: u64,
    changeset: Option<NonZeroU64>,
    version: Option<NonZeroU32>,
    tags: Vec<String>,
    lon_e7: i32,
    lat_e7: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Way {
    id: u64,
    changeset: Option<NonZeroU64>,
    version: Option<NonZeroU32>,
    nodes: Vec<u64>,
    tags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Relation {
    id: u64,
    changeset: Option<NonZeroU64>,
    version: Option<NonZeroU32>,
    tags: Vec<String>,
    members: Vec<RelationMember>,
}

/// Role of a member inside a relation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum MemberRole {
    Outer,
    Inner,
    Other(String),
}

impl MemberRole {
    fn from_str(s: &str) -> Self {
        match s {
            "outer" => MemberRole::Outer,
            "inner" => MemberRole::Inner,
            other => MemberRole::Other(other.to_owned()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum RelationMember {
    Node { id: u64, role: MemberRole },
    Way { id: u64, role: MemberRole },
    Relation { id: u64, role: MemberRole },
}

/// Abstracts OSM feature retrieval, so the geometry logic is independent of the storage.
/// Implemented by MockFeatureStore for testing, and FilteredFeatureStore in production.
pub trait FeatureStore: Send + Sync {
    fn get_node(&self, id: u64) -> Option<Node>;
    fn get_way(&self, id: u64) -> Option<Way>;
    fn get_relation(&self, id: u64) -> Option<Relation>;

    fn node_count(&self) -> u64;
    fn way_count(&self) -> u64;
    fn relation_count(&self) -> u64;

    fn get_coord(&self, node_id: u64) -> Option<geo::Coord>;
    fn get_nth_node(&self, n: u64) -> Option<Node>;
    fn get_nth_way(&self, n: u64) -> Option<Way>;

    // TODO: Handle relations.
    // https://github.com/alltheplaces/osm-diffs/issues/187
    #[allow(unused)]
    fn get_nth_relation(&self, n: u64) -> Option<Relation>;
}

fn build_relation_parents<R: Read + Seek + Send>(
    reader: &mut BlobReader<R>,
    progress: &MultiProgress,
) -> Result<HashMap<u64, u64>> {
    let progress_bar = make_progress_bar(
        progress,
        "osm.prt.r",
        reader.count_relation_blobs() as u64,
        "blobs",
    );
    let mut result = HashMap::<u64, u64>::new();
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let producer = s.spawn(|| reader.send_relation_blobs(blob_tx));
        let consumer = s.spawn(|| {
            result = blob_rx
                .into_iter()
                .par_bridge()
                .try_fold(
                    || HashMap::with_capacity(1024),
                    |mut map, blob| {
                        let data = blob.into_data(); // decompress
                        let block = PrimitiveBlock::parse(&data);
                        for primitive in block.primitives() {
                            if let Primitive::Relation(rel) = primitive {
                                for (_, member_id, member_type) in rel.members() {
                                    if member_type == RelationMemberType::Relation {
                                        map.insert(member_id, rel.id);
                                    }
                                }
                            }
                        }
                        progress_bar.inc(1);
                        Ok(map)
                    },
                )
                .try_reduce(
                    || HashMap::with_capacity(16384),
                    |mut a, b| {
                        a.extend(b);
                        Ok(a)
                    },
                )?;
            Ok(())
        });
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;
        Ok(())
    })?;

    progress_bar.finish_with_message(format!("blobs → {} relation parents", result.len()));
    Ok(result)
}

/// Reads data blobs from OpenStreetMap PBF files.
struct BlobReader<'a, R: Read + Seek + Send> {
    reader: &'a mut R,

    /// Offset and size of each data blob.
    blobs: Vec<(u64, usize)>,
    node_blobs: Range<usize>,
    way_blobs: Range<usize>,
    relation_blobs: Range<usize>,
}

// SAFETY: Can be safely sent across threads, if the underlying reader
// implements the Send trait. With the type trait being declared as
// below (`+ Send`), this gets enforced by the Rust compiler.
unsafe impl<'a, R: Read + Seek + Send> Send for BlobReader<'a, R> {}

impl<'a, R: Read + Seek + Send> BlobReader<'a, R> {
    pub fn open(reader: &'a mut R) -> Result<BlobReader<'a, R>> {
        reader.seek(SeekFrom::End(0))?;
        let file_size = reader.stream_position()?;
        if file_size == 0 {
            return Err(anyhow!("empty file"));
        }
        let mut pos = 0_u64;
        let mut blobs = Vec::<(u64, usize)>::new();
        while pos < file_size {
            reader.seek(SeekFrom::Start(pos))?;
            let blob_header = Self::read_blob_header(reader)?;
            let Some((blob_type, data_size)) = Self::parse_blob_header(&blob_header) else {
                return Err(anyhow!("bad blob header at offset {}", pos));
            };
            match blob_type {
                b"OSMHeader" => {}
                b"OSMData" => {
                    blobs.push((pos + 4_u64 + (blob_header.len() as u64), data_size));
                }
                _ => {}
            }
            pos += 4_u64 + (blob_header.len() as u64) + (data_size as u64);
        }

        let (nodes_end, ways_end) = Self::partition(reader, &blobs)?;
        let relations_end = blobs.len();
        Ok(BlobReader {
            reader,
            blobs,
            node_blobs: 0..nodes_end,
            way_blobs: nodes_end.saturating_sub(1)..ways_end,
            relation_blobs: ways_end.saturating_sub(1)..relations_end,
        })
    }

    pub fn count_node_blobs(&self) -> usize {
        self.node_blobs.len()
    }

    pub fn count_way_blobs(&self) -> usize {
        self.way_blobs.len()
    }

    pub fn count_relation_blobs(&self) -> usize {
        self.relation_blobs.len()
    }

    pub fn send_node_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.node_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    pub fn send_way_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.way_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    pub fn send_relation_blobs(&mut self, tx: SyncSender<Blob>) -> Result<()> {
        for i in self.relation_blobs.clone() {
            let (offset, len) = self.blobs[i];
            tx.send(Self::read_blob(self.reader, offset, len)?)?;
        }
        Ok(())
    }

    fn read_blob(reader: &mut R, offset: u64, len: usize) -> Result<Blob> {
        let mut buf = Vec::with_capacity(len);
        reader.seek(SeekFrom::Start(offset))?;

        // SAFETY: After read_exact(), all bytes in buffer have a defined value.
        unsafe {
            buf.set_len(len);
            reader.read_exact(&mut buf)?;
        }
        Self::decode_blob(&buf)
    }

    fn decode_blob(data: &[u8]) -> Result<Blob> {
        for m in MessageIter::new(data) {
            match m.tag {
                1 => return Ok(Blob::Raw(Vec::from(m.value.get_data()))),
                3 => return Ok(Blob::Zlib(Vec::from(m.value.get_data()))),
                _ => {}
            }
        }

        Err(anyhow!("cannot decode blob"))
    }

    /// Partitions the blogs into nodes, ways and relations.
    ///
    /// # Returns
    ///
    /// A tuple `(a, b)` where `a` is the first blob without any nodes,
    /// and `b` is the first blob without either nodes or ways.
    ///
    /// # Warnings
    ///
    /// In the
    /// [OpenStreetMap PBF format](https://wiki.openstreetmap.org/wiki/PBF_Format),
    /// a single blog may contain repeated PrimitiveGroups. While all primitives
    /// in the same must be of the same type (node, way or relation), the format
    /// makes no such guarantee on the blob level.
    fn partition(reader: &mut R, blobs: &[(u64, usize)]) -> Result<(usize, usize)> {
        let ways = {
            let mut left = 0;
            let mut right = blobs.len();
            while left < right {
                let mid = left + (right - left) / 2;
                let blob = Self::read_blob(reader, blobs[mid].0, blobs[mid].1)?;
                if Self::classify(blob)? < 2 {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        let relations = {
            let mut left = ways;
            let mut right = blobs.len();
            while left < right {
                let mid = left + (right - left) / 2;
                let blob = Self::read_blob(reader, blobs[mid].0, blobs[mid].1)?;
                if Self::classify(blob)? < 3 {
                    left = mid + 1;
                } else {
                    right = mid;
                }
            }
            left
        };

        Ok((ways, relations))
    }

    /// Internal helper for partition().
    fn classify(blob: Blob) -> Result<u8> {
        let data = blob.into_data();
        let block = PrimitiveBlock::parse(&data);
        match block.primitives().next() {
            Some(Primitive::Node(_)) => Ok(1),
            Some(Primitive::Way(_)) => Ok(2),
            Some(Primitive::Relation(_)) => Ok(3),
            None => Err(anyhow!("empty blob")),
        }
    }

    fn read_blob_header<T: Read>(reader: &mut T) -> Result<Vec<u8>> {
        let header_len = {
            let mut header_len_buf = [0; 4];
            reader.read_exact(&mut header_len_buf)?;
            u32::from_be_bytes(header_len_buf) as usize
        };
        let mut header = vec![0; header_len];
        reader.read_exact(&mut header)?;
        Ok(header)
    }

    fn parse_blob_header(data: &[u8]) -> Option<(&[u8], usize)> {
        let mut blob_type: Option<&[u8]> = None;
        let mut data_size: Option<usize> = None;
        for m in MessageIter::new(data) {
            match m.tag {
                1 => blob_type = Some(m.value.get_data()),
                3 => data_size = Some(u32::from(m.value) as usize),
                _ => {}
            }
        }
        Some((blob_type?, data_size?))
    }
}

struct ParentChainIter<'a> {
    parents: &'a HashMap<u64, u64>,
    current: Option<u64>,
    visited: HashSet<u64>,
}

impl<'a> ParentChainIter<'a> {
    fn new(parents: &'a HashMap<u64, u64>, start: u64) -> Self {
        ParentChainIter {
            parents,
            current: Some(start),
            visited: HashSet::new(),
        }
    }
}

impl<'a> Iterator for ParentChainIter<'a> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current?;

        // Check for cycles.
        if !self.visited.insert(current) {
            // Cycle detected; stop iteration.
            self.current = None;
            return None;
        }

        // Look up the parent for next iteration.
        self.current = self.parents.get(&current).copied();

        Some(current)
    }
}

trait ParentChainExt {
    fn parent_chain<'a>(&'a self, start: u64) -> ParentChainIter<'a>;
}

impl ParentChainExt for HashMap<u64, u64> {
    fn parent_chain<'a>(&'a self, start: u64) -> ParentChainIter<'a> {
        ParentChainIter::new(self, start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::path::PathBuf;

    pub struct MockFeatureStore {
        nodes: HashMap<u64, Node>,
        ways: HashMap<u64, Way>,
        relations: HashMap<u64, Relation>,

        node_ids: Vec<u64>,
        way_ids: Vec<u64>,

        #[allow(unused)] // TODO: Remove attribute once we use relations in FeatureStore.
        relation_ids: Vec<u64>,
    }

    impl MockFeatureStore {
        pub fn new(nodes: Vec<Node>, ways: Vec<Way>, relations: Vec<Relation>) -> MockFeatureStore {
            let node_ids = nodes.iter().map(|node| node.id).collect();
            let way_ids = ways.iter().map(|way| way.id).collect();
            let relation_ids = relations.iter().map(|rel| rel.id).collect();

            let nodes: HashMap<u64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
            let ways: HashMap<u64, Way> = ways.into_iter().map(|w| (w.id, w)).collect();
            let relations: HashMap<u64, Relation> =
                relations.into_iter().map(|r| (r.id, r)).collect();

            MockFeatureStore {
                nodes,
                ways,
                relations,
                node_ids,
                way_ids,
                relation_ids,
            }
        }
    }

    impl FeatureStore for MockFeatureStore {
        fn get_node(&self, id: u64) -> Option<Node> {
            if let Some(node) = self.nodes.get(&id) {
                Some(node.clone())
            } else {
                None
            }
        }

        fn get_way(&self, id: u64) -> Option<Way> {
            if let Some(way) = self.ways.get(&id) {
                Some(way.clone())
            } else {
                None
            }
        }

        fn get_relation(&self, id: u64) -> Option<Relation> {
            if let Some(relation) = self.relations.get(&id) {
                Some(relation.clone())
            } else {
                None
            }
        }

        fn node_count(&self) -> u64 {
            self.nodes.len() as u64
        }

        fn way_count(&self) -> u64 {
            self.ways.len() as u64
        }

        fn relation_count(&self) -> u64 {
            self.relations.len() as u64
        }

        fn get_coord(&self, node_id: u64) -> Option<geo::Coord> {
            let node = self.nodes.get(&node_id)?;
            Some(geo::Coord {
                x: node.lon_e7 as f64 * 1e-7,
                y: node.lat_e7 as f64 * 1e-7,
            })
        }

        fn get_nth_node(&self, n: u64) -> Option<Node> {
            let n = usize::try_from(n).ok()?;
            if n < self.node_ids.len() {
                Some(self.nodes.get(&self.node_ids[n])?.clone())
            } else {
                None
            }
        }

        fn get_nth_way(&self, n: u64) -> Option<Way> {
            let n = usize::try_from(n).ok()?;
            if n < self.way_ids.len() {
                Some(self.ways.get(&self.way_ids[n])?.clone())
            } else {
                None
            }
        }

        fn get_nth_relation(&self, n: u64) -> Option<Relation> {
            let n = usize::try_from(n).ok()?;
            if n < self.relation_ids.len() {
                Some(self.relations.get(&self.relation_ids[n])?.clone())
            } else {
                None
            }
        }
    }

    #[test]
    fn test_blob_reader() -> Result<()> {
        let mut file = File::open(test_data_path("zugerland.osm.pbf"))?;
        let mut reader = BlobReader::open(&mut file)?;
        assert_eq!(reader.blobs, &[(119, 16681), (16816, 15278), (32110, 8616)]);
        assert_eq!(reader.node_blobs, 0..1);
        assert_eq!(reader.way_blobs, 0..2);
        assert_eq!(reader.relation_blobs, 1..3);
        let (tx, rx) = sync_channel::<Blob>(5);
        reader.send_node_blobs(tx)?;
        if let Blob::Zlib(_) = rx.recv()? {
        } else {
            return Err(anyhow!("failed to read blob"));
        }
        Ok(())
    }

    #[test]
    fn test_blob_reader_decode() -> Result<()> {
        if let Blob::Raw(blob) = BlobReader::<File>::decode_blob(&[0x0a, 1, 77])? {
            assert_eq!(blob, &[77]);
        } else {
            panic!("unexpected blob type");
        }
        if let Blob::Zlib(blob) = BlobReader::<File>::decode_blob(&[0x1a, 1, 77])? {
            assert_eq!(blob, &[77]);
        } else {
            panic!("unexpected blob type");
        }
        assert!(BlobReader::<File>::decode_blob(&[0x2a, 1, 77]).is_err());
        Ok(())
    }

    #[test]
    fn test_blob_reader_bad_data() {
        assert!(BlobReader::open(&mut Cursor::new(b"")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"\0\0\0")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"\0\0\0\0")).is_err());
        assert!(BlobReader::open(&mut Cursor::new(b"test file with junk data")).is_err());
    }

    fn test_data_path(filename: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests");
        path.push("test_data");
        path.push(filename);
        path
    }

    #[test]
    fn test_parent_chain() {
        let mut g = HashMap::new();
        g.insert(5, 23);
        g.insert(23, 42);
        g.insert(27, 42);
        g.insert(42, 100);

        let parent_chain = |i| g.parent_chain(i).collect::<Vec<u64>>();
        assert_eq!(parent_chain(5), &[5, 23, 42, 100]);
        assert_eq!(parent_chain(23), &[23, 42, 100]);
        assert_eq!(parent_chain(27), &[27, 42, 100]);
        assert_eq!(parent_chain(42), &[42, 100]);
        assert_eq!(parent_chain(100), &[100]);
        assert_eq!(parent_chain(9999), &[9999]);
    }

    #[test]
    fn test_parent_chain_cycle() {
        let mut g = HashMap::new();
        g.insert(5, 23);
        g.insert(23, 42);
        g.insert(42, 100);
        g.insert(100, 23);

        let parent_chain = |i| g.parent_chain(i).collect::<Vec<u64>>();
        assert_eq!(parent_chain(5), &[5, 23, 42, 100]);
        assert_eq!(parent_chain(23), &[23, 42, 100]);
        assert_eq!(parent_chain(42), &[42, 100, 23]);
        assert_eq!(parent_chain(100), &[100, 23, 42]);
    }
}
