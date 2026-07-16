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

pub struct GraphTable<'a> {
    file: File,
    _mmap: Mmap,
    children: &'a [u64],
    parents: &'a [u64],
}

impl<'a> GraphTable<'a> {
    pub fn create(
        edges: impl Iterator<Item = Edge>,
        workdir: &Path,
        out: &Path,
    ) -> Result<GraphTable<'a>> {
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
    pub fn open(path: &Path) -> Result<GraphTable<'a>> {
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

        Ok(GraphTable {
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
        let graph = GraphTable::create(edges_iter, &workdir.path(), &path)?;
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
