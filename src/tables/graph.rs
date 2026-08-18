//! Disk-based, memory-mapped directed graph with `u64` node IDs.
//!
//! `GraphTable` is used to represent the "is member of" relation between
//! OpenStreetMap relations: a relation may itself be a member of other,
//! "super", relations, so the pipeline needs a graph of which relation
//! (the child) is contained in which other relation (the parent).
//! [GraphTable::ancestors] walks the transitive closure of that relation
//! starting at a node; [GraphTable::nodes] yields every node bottom-up,
//! which is useful for building the geometry of a relation only once the
//! geometries of every relation it contains have already been built.
//!
//! # File format
//!
//! ```text
//! byte 0..8:  magic "graph_v0"
//! byte 8..16: edge count, u64 little-endian
//! byte 16..:  children array, `edge count` entries, u64 little-endian
//!             each
//! byte ..:    parents array, `edge count` entries, u64 little-endian
//!             each, aligned 1:1 with the children array
//! ```
//!
//! The children and parents arrays are two parallel columns of one
//! `(child, parent)` edge list: edge `i` is `(children[i], parents[i])`.
//! They are sorted ascending by `(child, parent)` (see [Edge]'s derived
//! [Ord]), which is what makes the binary search in
//! [GraphTable::parent_range] possible; a node with several outgoing
//! edges appears several times in `children`.
//!
//! The header is a fixed 16 bytes so that the children and parents
//! arrays, which follow it back to back, stay 8-byte aligned and can be
//! reinterpreted as `&[u64]` slices directly on the mmap'd bytes.

use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    fs::{File, remove_file, rename},
    io::{BufReader, BufWriter, Seek, SeekFrom, Write},
    mem::size_of,
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

/// Size of the file header, in bytes: see the "File format" section above.
const HEADER_SIZE: usize = 16;

/// Magic bytes identifying a `GraphTable` file, written as the first
/// eight bytes of the file header.
const FILE_SIGNATURE: &[u8; 8] = b"graph_v0";

/// Read-only, memory-mapped directed graph of `(child, parent)` edges. See
/// the "File format" section above.
pub struct GraphTable<'a> {
    file: File,
    _mmap: Mmap,
    children: &'a [u64],
    parents: &'a [u64],
}

impl<'a> GraphTable<'a> {
    /// Builds a `GraphTable` from `edges`, which may be in any order.
    ///
    /// Since [Writer::write] requires edges in ascending `(child, parent)`
    /// order, `edges` is first sorted using external sorting (spilling to
    /// `workdir` as needed), and only then written to `out`. `chunk_bytes`
    /// bounds each external-sort chunk's in-memory size (and, in turn, how
    /// many chunk files end up open at once during the final merge -- see
    /// [crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES]); `Edge` is fixed-size,
    /// so this is converted to an item count rather than measured via
    /// `MemoryLimitedBufferBuilder`. Callers that run several external
    /// sorts concurrently (e.g. from sibling threads in the same
    /// `thread::scope`) should divide their share of the budget
    /// accordingly -- this function has no way to know how many others
    /// are running alongside it.
    pub fn create(
        edges: impl Iterator<Item = Edge>,
        workdir: &Path,
        out: &Path,
        chunk_bytes: usize,
    ) -> Result<GraphTable<'a>> {
        let mut writer = Writer::create(out)?;
        let num_edges = AtomicU64::new(0);
        // Named as a local type alias, not spelled out twice, so the item
        // type used to build the sorter and the one used to size its
        // buffer can't silently drift apart under a future refactoring.
        type Item = Edge;
        let sorter: ExternalSorter<Item, std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    chunk_bytes / size_of::<Item>(),
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

    /// Opens a `GraphTable` previously written by [GraphTable::create],
    /// mapping it into memory rather than reading it into a
    /// heap-allocated buffer.
    #[cfg(target_pointer_width = "64")]
    pub fn open(path: &Path) -> Result<GraphTable<'a>> {
        let file = File::open(path)?;

        // SAFETY: We don’t truncate the file while it’s mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || mmap[0..8] != *FILE_SIGNATURE {
            anyhow::bail!("not a GraphTable: {}", path.display());
        }

        let edge_count = usize::try_from(u64::from_le_bytes(
            mmap[8..16].try_into().expect("edge_count"),
        ))?;
        let expected_size = HEADER_SIZE + edge_count * 16;
        if mmap.len() != expected_size {
            anyhow::bail!(
                "{} has wrong file size, expected {}, got {}",
                path.display(),
                expected_size,
                mmap.len()
            );
        }

        // SAFETY: mmap.len() checked above.
        let children = unsafe {
            let ptr = mmap.as_ptr().add(HEADER_SIZE) as *const u64;
            std::slice::from_raw_parts(ptr, edge_count)
        };

        // SAFETY: mmap.len() checked above.
        let parents = unsafe {
            let ptr = mmap.as_ptr().add(HEADER_SIZE + edge_count * 8) as *const u64;
            std::slice::from_raw_parts(ptr, edge_count)
        };

        Ok(GraphTable {
            file,
            _mmap: mmap,
            children,
            parents,
        })
    }

    /// Returns the modification time of the backing file.
    ///
    /// Currently unused -- part of the memoization surface every table in
    /// this module exposes, but no pipeline stage's staleness check reads
    /// it yet. See https://github.com/alltheplaces/osm-diffs/issues/704.
    #[allow(unused)]
    pub fn modified(&'a self) -> Result<SystemTime> {
        Ok(self.file.metadata()?.modified()?)
    }

    /// Returns the number of `(child, parent)` edges in the graph.
    pub fn edge_count(&self) -> usize {
        self.children.len()
    }

    /// Returns the number of distinct nodes in the graph, i.e. every id
    /// that appears as a child or a parent of at least one edge, counted
    /// once.
    pub fn node_count(&self) -> usize {
        // `parents` is not sorted, so there's no way around collecting its
        // distinct values into a set to count them; `remaining` in `nodes`
        // above relies on the same assumption that this stays small.
        let mut parents: HashSet<u64> = HashSet::new();
        for idx in 0..self.parents.len() {
            parents.insert(self.parent_at(idx));
        }

        // `children`, in contrast, is sorted, so distinct values form
        // contiguous runs: counting the runs gives the distinct count in
        // one O(edge_count) pass, without building a set for it. Values
        // that are both a child and a parent must only be counted once,
        // so drop them from `parents` as they're encountered here.
        let mut distinct_children = 0usize;
        let mut prev = None;
        for idx in 0..self.children.len() {
            let child = self.child_at(idx);
            if prev != Some(child) {
                distinct_children += 1;
                parents.remove(&child);
                prev = Some(child);
            }
        }

        // What's left in `parents` are nodes that never appear as a
        // child, i.e. roots of the graph.
        distinct_children + parents.len()
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

    /// Returns an iterator over every node in the graph, in breadth-first
    /// order, such that a node is only yielded once all of its children
    /// have already been yielded. A "child" of `parent` is any node `c`
    /// for which an edge `Edge { child: c, parent }` was recorded.
    ///
    /// In other words, leaves (nodes without children) come first and
    /// their ancestors follow, which makes this iterator useful for
    /// bottom-up processing, such as building the geometry of an
    /// OpenStreetMap relation from the already-built geometries of its
    /// member relations.
    ///
    /// Each node is yielded exactly once. A cycle has no node without
    /// children to start from, so it is broken by picking its
    /// numerically smallest node first; nodes reachable only through a
    /// cycle are therefore yielded in an order that does not necessarily
    /// put every child before its parent.
    pub fn nodes(&'a self) -> impl Iterator<Item = u64> + 'a {
        // `remaining[n]` counts how many of `n`'s children have not been
        // yielded yet; `n` becomes ready to yield once this reaches 0.
        // Nodes without children (leaves) are never inserted here, since
        // their count would always be 0 — they go straight into `queue`
        // below instead. This keeps memory proportional to the number of
        // nodes that have at least one child, not to the whole graph,
        // which matters since `children`/`parents` are memory-mapped
        // specifically to support graphs too large to fit in RAM.
        //
        // In our pipeline, we use this module for the containment graph
        // of OpenStreetMap relations. With the current implementation,
        // we only keep a counter for (recursive) super-relations in RAM.
        // There’s only about 40K super-relations in OSM, so memory
        // consumption is not an issue.
        let mut remaining: BTreeMap<u64, u32> = BTreeMap::new();
        for idx in 0..self.children.len() {
            *remaining.entry(self.parent_at(idx)).or_insert(0) += 1;
        }

        // `children` is sorted, so distinct child ids appear as runs of
        // equal values; picking out the first index of each run gives
        // every leaf exactly once, in ascending order.
        let mut queue = VecDeque::new();
        let mut prev = None;
        for idx in 0..self.children.len() {
            let child = self.child_at(idx);
            if prev != Some(child) {
                if !remaining.contains_key(&child) {
                    queue.push_back(child);
                }
                prev = Some(child);
            }
        }

        NodesIter {
            graph: self,
            queue,
            remaining,
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

/// One edge of a [GraphTable]: `child` is contained in `parent`.
///
/// Derives [Ord] over `(child, parent)` in field-declaration order, which
/// [GraphTable::create] relies on to sort edges into the ascending order
/// required by [Writer::write] (see the "File format" section at the top
/// of this module).
#[derive(Serialize, Deserialize, Ord, PartialOrd, PartialEq, Eq)]
pub struct Edge {
    pub child: u64,
    pub parent: u64,
}

struct AncestorIter<'a> {
    graph: &'a GraphTable<'a>,
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

struct NodesIter<'a> {
    graph: &'a GraphTable<'a>,
    queue: VecDeque<u64>,
    /// Nodes with at least one child, not yet yielded, mapped to the
    /// number of their children that have not been yielded yet.
    remaining: BTreeMap<u64, u32>,
}

impl<'a> Iterator for NodesIter<'a> {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        let node = match self.queue.pop_front() {
            Some(node) => node,
            None => {
                // No node has zero children left, so the graph must
                // contain a cycle. Break it by picking the smallest
                // remaining node, treating it as if it were ready.
                let node = *self.remaining.keys().next()?;
                self.remaining.remove(&node);
                node
            }
        };

        for idx in self.graph.parent_range(node) {
            let parent = self.graph.parent_at(idx);
            if let Some(count) = self.remaining.get_mut(&parent) {
                *count -= 1;
                if *count == 0 {
                    self.remaining.remove(&parent);
                    self.queue.push_back(parent);
                }
            }
        }

        Some(node)
    }
}

/// Writes a [GraphTable] file, one edge at a time.
///
/// Children and parents are appended to separate files as edges arrive
/// (parents to a temporary side file); [Writer::close] then concatenates
/// them behind a fixed-size header and atomically renames the result into
/// place. Splitting the files this way avoids seeking back and forth in
/// the output file, since the total number of edges — and thus the offset
/// of the parents array — is not known until all edges have been written.
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
        out.write_all(FILE_SIGNATURE)?;
        out.write_all(&0_u64.to_le_bytes())?; // placeholder edge count

        let parents_file = File::create(Self::parents_path(path))?;

        Ok(Writer {
            edge_count: 0,
            path: PathBuf::from(path),
            tmp_path,
            out,
            parents_out: BufWriter::with_capacity(32768, parents_file),
        })
    }

    /// Appends `edge`. Edges must be written in ascending `(child, parent)`
    /// order, so that [GraphTable::parent_range] can binary-search the
    /// children array.
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

        self.out.seek(SeekFrom::Start(8))?;
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
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
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
        assert_eq!(
            graph.nodes().collect::<Vec<u64>>(),
            &[1, 2, 3, 4, 5, 6, 21, 22, 23]
        );
        Ok(())
    }

    /// `nodes()` must wait for both branches of a diamond-shaped DAG
    /// (multiple nodes sharing the same parent) before yielding the
    /// parent.
    #[test]
    fn test_nodes_diamond() -> Result<()> {
        let edges: Vec<(u64, u64)> = vec![(1, 2), (1, 3), (2, 4), (3, 4)];
        let edges_iter = edges
            .into_iter()
            .map(|(child, parent)| Edge { child, parent });
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.nodes().collect::<Vec<u64>>(), &[1, 2, 3, 4]);
        Ok(())
    }

    /// `nodes()` yields nothing for a graph without edges.
    #[test]
    fn test_nodes_empty() -> Result<()> {
        let edges_iter = std::iter::empty::<Edge>();
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.nodes().collect::<Vec<u64>>(), Vec::<u64>::new());
        Ok(())
    }

    #[test]
    fn test_writer_used_directly() -> Result<()> {
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let mut writer = Writer::create(&path)?;
        writer.write(Edge {
            child: 1,
            parent: 2,
        })?;
        writer.write(Edge {
            child: 2,
            parent: 3,
        })?;
        writer.close()?;

        let graph = GraphTable::open(&path)?;
        assert_eq!(graph.ancestors(1).collect::<Vec<u64>>(), &[1, 2, 3]);
        Ok(())
    }

    #[test]
    fn test_open_rejects_file_without_signature() -> Result<()> {
        let workdir = TempDir::new()?;
        let path = workdir.path().join("not-a-graph");
        std::fs::write(&path, [0u8; HEADER_SIZE])?;
        assert!(GraphTable::open(&path).is_err());
        Ok(())
    }

    #[test]
    fn test_open_rejects_truncated_file() -> Result<()> {
        let workdir = TempDir::new()?;
        let path = workdir.path().join("too-short");
        std::fs::write(&path, FILE_SIGNATURE)?;
        assert!(GraphTable::open(&path).is_err());
        Ok(())
    }

    #[test]
    fn test_edge_count() -> Result<()> {
        let edges: Vec<(u64, u64)> = vec![(1, 2), (2, 3), (2, 4)];
        let edges_iter = edges
            .into_iter()
            .map(|(child, parent)| Edge { child, parent });
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.edge_count(), 3);
        Ok(())
    }

    #[test]
    fn test_edge_count_empty() -> Result<()> {
        let edges_iter = std::iter::empty::<Edge>();
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.edge_count(), 0);
        Ok(())
    }

    #[test]
    fn test_node_count() -> Result<()> {
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
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        // Nodes 1..6 and 21..23, nine in total.
        assert_eq!(graph.node_count(), 9);
        Ok(())
    }

    #[test]
    fn test_node_count_empty() -> Result<()> {
        let edges_iter = std::iter::empty::<Edge>();
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.node_count(), 0);
        Ok(())
    }

    /// A node that is both a child (of one edge) and a parent (of another)
    /// must be counted only once.
    #[test]
    fn test_node_count_shared_node() -> Result<()> {
        let edges: Vec<(u64, u64)> = vec![(1, 2), (2, 3)];
        let edges_iter = edges
            .into_iter()
            .map(|(child, parent)| Edge { child, parent });
        let workdir = TempDir::new()?;
        let path = workdir.path().join("testgraph");
        let graph = GraphTable::create(
            edges_iter,
            workdir.path(),
            &path,
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(graph.node_count(), 3);
        Ok(())
    }

    #[test]
    fn test_open_rejects_wrong_file_size() -> Result<()> {
        let workdir = TempDir::new()?;
        let path = workdir.path().join("wrong-size");
        let mut writer = Writer::create(&path)?;
        writer.write(Edge {
            child: 1,
            parent: 2,
        })?;
        writer.close()?;

        // Truncate the file so the edge count in the header no longer
        // matches the actual size of the children/parents arrays.
        let file = std::fs::OpenOptions::new().write(true).open(&path)?;
        file.set_len(HEADER_SIZE as u64 + 8)?;
        drop(file);

        assert!(GraphTable::open(&path).is_err());
        Ok(())
    }
}
