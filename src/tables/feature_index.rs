//! Spatial index over OSM [Feature] protos, keyed by S2 cell coverage.
//!
//! Unlike [crate::places::PlaceIndex] (which indexes single-point `Place`
//! values backed by Parquet), `OsmFeatureIndex` indexes real OSM
//! geometry -- points, lines, and polygons alike -- so a single feature
//! can cover more than one S2 cell. That means the field used to sort
//! features physically on disk (`centroid_s2_cell_id`, for locality only)
//! is *not* a valid query key: a query has to test a feature's full
//! `coverage_s2_cell_id` list, not just its centroid. So this index is
//! really two on-disk structures:
//!
//! 1. **Feature storage**: features laid out in `centroid_s2_cell_id`
//!    order (for locality -- never queried directly), each addressable by
//!    a stable position (its [LocalFeatureRef]).
//! 2. **Inverted index**: the actual query structure, in the classic
//!    information-retrieval sense -- the natural relationship is
//!    "feature → S2 cells covering it"; this inverts that into "S2 cell →
//!    features covering it". Sorted by S2 cell id, so a query is a
//!    `partition_point` range scan, independent of centroid/storage
//!    order.
//!
//! Both structures are mmap'd, uncompressed, read-only after
//! [OsmFeatureIndex::create]. A query only ever binary-searches and reads
//! numeric arrays -- no protobuf decode happens until [OsmFeatureIndex::get_feature]
//! is called for a specific candidate.
//!
//! # File format
//!
//! Feature storage (`out`):
//!
//! ```text
//! byte 0..8:   magic "featstg0"
//! byte 8..16:  entry count, u64 little-endian
//! byte 16..24: offset of the `id` array, u64 little-endian
//! byte 24..32: offset of the `starts` array, u64 little-endian
//! byte 32..40: offset of the `blob` region, u64 little-endian
//! byte 40..64: reserved, zero-filled
//!
//! id array:     `entry count` OSM feature ids, u64 little-endian each
//! starts array: `entry count + 1` byte offsets into `blob`, u64
//!               little-endian each
//! blob region:  concatenated `Feature.encode_to_vec()` bytes
//! ```
//!
//! Inverted index (`out` with an added `.inverted` extension):
//!
//! ```text
//! byte 0..8:   magic "featinv0"
//! byte 8..16:  entry count, u64 little-endian
//! byte 16..24: offset of the `coverage_cell_id` array, u64 little-endian
//! byte 24..32: offset of the `packed` array, u64 little-endian
//! byte 32..64: reserved, zero-filled
//!
//! coverage_cell_id array: `entry count` S2 cell ids, u64 little-endian
//!                         each, sorted ascending (duplicates allowed)
//! packed array:           `entry count` values, u64 little-endian each,
//!                         aligned 1:1 with coverage_cell_id: high 32
//!                         bits are the feature's MatchMask, low 32 bits
//!                         are its LocalFeatureRef (position in feature
//!                         storage)
//! ```

use crate::matchers::MatchMask;
use crate::tables::{Feature, FeatureToIndex};
use anyhow::{Context, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use memmap2::Mmap;
use prost::Message;
use s2::cellid::CellID;
use std::{
    fs::{File, remove_file, rename},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    mem::size_of,
    ops::RangeInclusive,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Size of each file's header, in bytes: see the "File format" section above.
const HEADER_SIZE: usize = 8 * 8;

const STORAGE_FILE_SIGNATURE: &[u8; 8] = b"featstg0";
const INVERTED_FILE_SIGNATURE: &[u8; 8] = b"featinv0";

/// An opaque reference to a feature's position in an [OsmFeatureIndex]'s
/// feature storage. Cheap to copy and hold onto (it's just an array
/// index); not meaningful outside the [OsmFeatureIndex] that produced it,
/// and not an OSM id -- see [OsmFeatureIndex::feature_id].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalFeatureRef(u32);

/// Read-only, memory-mapped spatial index over [Feature] protos. See the
/// module documentation for the on-disk layout and the reasoning behind
/// splitting this into two structures.
pub struct OsmFeatureIndex<'a> {
    storage_file: File,
    _storage_mmap: Mmap,
    entries_count: usize,
    id: &'a [u64],
    starts: &'a [u64],
    blob: &'a [u8],

    inverted_file: File,
    _inverted_mmap: Mmap,
    coverage_cell_id: &'a [u64],
    packed: &'a [u64],
}

impl<'a> OsmFeatureIndex<'a> {
    /// Path of the inverted-index file that accompanies `out`/`path`.
    fn inverted_path(path: &Path) -> PathBuf {
        let mut p = PathBuf::from(path);
        p.add_extension("inverted");
        p
    }

    /// Builds an `OsmFeatureIndex` from `fti`, which may be in any order.
    ///
    /// Two passes, since a feature's [LocalFeatureRef] is only known once
    /// the first pass's sort (by `centroid_s2_cell_id`, for storage
    /// locality) is written:
    ///
    /// 1. External-sort `fti` by `centroid_s2_cell_id`. Walking the
    ///    sorted result in order fixes each feature's `LocalFeatureRef`;
    ///    write the feature storage arrays as we go, and spool
    ///    `(coverage_cell_id, match_mask, local_index)` tuples -- one per
    ///    entry in each feature's `coverage_s2_cell_id` -- to a temporary
    ///    file for pass 2.
    /// 2. External-sort the spooled tuples by `coverage_cell_id`, and
    ///    write the inverted-index arrays.
    pub fn create(
        fti: impl Iterator<Item = FeatureToIndex>,
        workdir: &Path,
        out: &Path,
    ) -> Result<OsmFeatureIndex<'a>> {
        let inverted_path = Self::inverted_path(out);
        let pass2_spool_path = {
            let mut p = PathBuf::from(out);
            p.add_extension("pass2.tmp");
            p
        };

        // Pass 1.
        let sorter1: ExternalSorter<(u64, Vec<u8>), std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    64 * 1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        let sorted1 = sorter1.sort(fti.map(|f| {
            let key = f.centroid_s2_cell_id;
            std::io::Result::Ok((key, f.encode_to_vec()))
        }))?;

        let mut storage_writer = StorageWriter::create(out)?;
        let mut pass2_writer =
            BufWriter::with_capacity(64 * 1024, File::create(&pass2_spool_path)?);
        for (local_index, item) in (0_u32..).zip(sorted1) {
            let (_, bytes) = item?;
            let fti = FeatureToIndex::decode(bytes.as_slice())
                .context("failed to decode a FeatureToIndex spooled during pass 1")?;
            let feature = fti.feature.expect("feature");
            storage_writer.write(feature.id, &feature.encode_to_vec())?;
            for cell_id in &fti.coverage_s2_cell_id {
                pass2_writer.write_all(&cell_id.to_le_bytes())?;
                pass2_writer.write_all(&fti.match_mask.to_le_bytes())?;
                pass2_writer.write_all(&local_index.to_le_bytes())?;
            }
        }
        storage_writer.close()?;
        pass2_writer.flush()?;
        drop(pass2_writer);

        // Pass 2.
        let pass2_records = Pass2Reader::open(&pass2_spool_path)?;
        let sorter2: ExternalSorter<(u64, u32, u32), std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    32 * 1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        let sorted2 = sorter2.sort(pass2_records.map(std::io::Result::Ok))?;
        let mut inverted_writer = InvertedWriter::create(&inverted_path)?;
        for item in sorted2 {
            let (cell_id, match_mask, local_index) = item?;
            inverted_writer.write(cell_id, match_mask, local_index)?;
        }
        inverted_writer.close()?;
        remove_file(&pass2_spool_path)?;

        Self::open(out)
    }

    /// Opens an `OsmFeatureIndex` previously written by
    /// [OsmFeatureIndex::create], mapping both files into memory rather
    /// than reading them into heap-allocated buffers.
    pub fn open(path: &Path) -> Result<OsmFeatureIndex<'a>> {
        let storage_file = File::open(path)
            .with_context(|| format!("could not open feature storage {}", path.display()))?;

        // SAFETY: We don't modify the file while it is mapped into memory.
        let storage_mmap = unsafe { Mmap::map(&storage_file)? };
        if storage_mmap.len() < HEADER_SIZE || &storage_mmap[0..8] != STORAGE_FILE_SIGNATURE {
            anyhow::bail!(
                "not feature storage for OsmFeatureIndex: {}",
                path.display()
            );
        }

        // SAFETY: storage_mmap.len() checked above; offset 0 is aligned for u64.
        let header = unsafe {
            let ptr = storage_mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };
        let entries_count = usize::try_from(u64::from_le(header[1]))?;

        let id = read_u64_array(
            &storage_mmap,
            usize::try_from(u64::from_le(header[2]))?,
            entries_count,
            path,
            "id",
        )?;
        let starts = read_u64_array(
            &storage_mmap,
            usize::try_from(u64::from_le(header[3]))?,
            entries_count + 1,
            path,
            "starts",
        )?;
        let blob = {
            let blob_offset = usize::try_from(u64::from_le(header[4]))?;
            if blob_offset <= storage_mmap.len() {
                // SAFETY: Verified length; no alignment constraints on &[u8].
                unsafe {
                    let ptr = storage_mmap.as_ptr().add(blob_offset);
                    std::slice::from_raw_parts(ptr, storage_mmap.len() - blob_offset)
                }
            } else {
                anyhow::bail!("misplaced blob in OsmFeatureIndex: {}", path.display());
            }
        };

        let inverted_path = Self::inverted_path(path);
        let inverted_file = File::open(&inverted_path).with_context(|| {
            format!("could not open inverted index {}", inverted_path.display())
        })?;

        // SAFETY: We don't modify the file while it is mapped into memory.
        let inverted_mmap = unsafe { Mmap::map(&inverted_file)? };
        if inverted_mmap.len() < HEADER_SIZE || &inverted_mmap[0..8] != INVERTED_FILE_SIGNATURE {
            anyhow::bail!(
                "not an inverted index for OsmFeatureIndex: {}",
                inverted_path.display()
            );
        }

        // SAFETY: inverted_mmap.len() checked above; offset 0 is aligned for u64.
        let inverted_header = unsafe {
            let ptr = inverted_mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };
        let inverted_entries_count = usize::try_from(u64::from_le(inverted_header[1]))?;

        let coverage_cell_id = read_u64_array(
            &inverted_mmap,
            usize::try_from(u64::from_le(inverted_header[2]))?,
            inverted_entries_count,
            &inverted_path,
            "coverage_cell_id",
        )?;
        let packed = read_u64_array(
            &inverted_mmap,
            usize::try_from(u64::from_le(inverted_header[3]))?,
            inverted_entries_count,
            &inverted_path,
            "packed",
        )?;

        Ok(OsmFeatureIndex {
            storage_file,
            _storage_mmap: storage_mmap,
            entries_count,
            id,
            starts,
            blob,

            inverted_file,
            _inverted_mmap: inverted_mmap,
            coverage_cell_id,
            packed,
        })
    }

    /// The number of features in this index.
    pub fn len(&self) -> usize {
        self.entries_count
    }

    /// Whether this index has no features.
    pub fn is_empty(&self) -> bool {
        self.entries_count == 0
    }

    /// The later of the two backing files' modification times. Backed by
    /// two mmap'd files (feature storage + inverted index), so there's no
    /// single path to report; this is what a staleness check (see
    /// `conflate`) actually needs.
    pub fn modified(&self) -> Result<SystemTime> {
        let a = self.storage_file.metadata()?.modified()?;
        let b = self.inverted_file.metadata()?.modified()?;
        Ok(a.max(b))
    }

    /// Returns every feature whose `coverage_s2_cell_id` includes a cell
    /// in `range`, and whose `match_mask` intersects `query_mask`. A
    /// cheap, decode-free scan over the inverted index: `partition_point`
    /// for the range bounds, then a numeric `match_mask` check per hit.
    /// No protobuf decode happens until [OsmFeatureIndex::get_feature] is
    /// called on a returned reference.
    ///
    /// `range` is expected to be one merged sub-range of an S2 covering
    /// of the caller's search cap -- the same shape already passed to
    /// `PlaceIndex::query`.
    ///
    /// The same feature can come back more than once (its coverage cells
    /// can straddle more than one hit in `range`) -- harmless for callers
    /// that just track a running best-scoring candidate; not deduplicated
    /// here.
    pub fn query(
        &self,
        range: RangeInclusive<CellID>,
        query_mask: MatchMask,
    ) -> impl Iterator<Item = LocalFeatureRef> + '_ {
        let lo = range.start().0;
        let hi = range.end().0;
        let start = self
            .coverage_cell_id
            .partition_point(|&k| u64::from_le(k) < lo);
        let end = self
            .coverage_cell_id
            .partition_point(|&k| u64::from_le(k) <= hi);
        (start..end).filter_map(move |i| {
            let packed = u64::from_le(self.packed[i]);
            let match_mask = MatchMask((packed >> 32) as u16);
            if match_mask.intersects(&query_mask) {
                Some(LocalFeatureRef((packed & 0xFFFF_FFFF) as u32))
            } else {
                None
            }
        })
    }

    /// The OSM id of the feature `r` refers to -- `osm_id * 10 + {1,2,3}`
    /// for {node,way,relation}, same encoding as `Feature.id` elsewhere in
    /// this codebase (see `make_feature_id`/`feature_to_osm_id` in
    /// `pipeline::osm::assemble`). O(1), array read only -- no decode.
    pub fn feature_id(&self, r: LocalFeatureRef) -> u64 {
        u64::from_le(self.id[r.0 as usize])
    }

    /// Fully decodes the feature `r` refers to. The only operation on
    /// this index that costs a protobuf decode -- callers should only do
    /// this for candidates that survive `MatchMask` filtering (and
    /// typically, scoring).
    pub fn get_feature(&self, r: LocalFeatureRef) -> Result<Feature> {
        let i = r.0 as usize;
        let start = usize::try_from(u64::from_le(self.starts[i]))?;
        let end = usize::try_from(u64::from_le(self.starts[i + 1]))?;
        Feature::decode(&self.blob[start..end]).context("failed to decode Feature")
    }
}

/// Reinterprets `count` little-endian `u64`s at `offset` in `mmap` as a
/// `&'a [u64]` slice, without copying. Shared by both of `open`'s files.
///
/// `'a` is deliberately *not* tied to `mmap`'s own (elided) borrow here:
/// like `BlobTable`/`CoordTable`'s inline equivalent, this hands back a
/// slice whose lifetime the caller chooses, derived from a raw pointer
/// rather than the `&Mmap` reference -- otherwise the borrow checker
/// would tie the slice to this function's local borrow of `mmap`, not to
/// the `Mmap` value itself once it's moved into the returned struct.
fn read_u64_array<'a>(
    mmap: &Mmap,
    offset: usize,
    count: usize,
    path: &Path,
    name: &str,
) -> Result<&'a [u64]> {
    if offset.is_multiple_of(8) && offset + count * 8 <= mmap.len() {
        // SAFETY: Verified alignment and length. The mmap'd region stays
        // valid for as long as the Mmap it came from is kept alive, which
        // callers do by storing it alongside this slice (see
        // OsmFeatureIndex's _storage_mmap/_inverted_mmap fields).
        Ok(unsafe {
            let ptr = mmap.as_ptr().add(offset) as *const u64;
            std::slice::from_raw_parts(ptr, count)
        })
    } else {
        anyhow::bail!(
            "misaligned {name} array in OsmFeatureIndex: {}",
            path.display()
        );
    }
}

/// Writer for the feature storage file: `id`, `starts` and `blob` arrays
/// as described in the module documentation. Keys and packed coordinates
/// are appended to separate temporary files as they arrive; [StorageWriter::close]
/// then concatenates them behind a fixed-size header and atomically
/// renames the result into place -- same technique as
/// [crate::tables::BlobTable]'s private `Writer`, minus the ascending-key
/// requirement: `id` here is a parallel data column, not a search key
/// (features are looked up by [LocalFeatureRef] position, not by id).
struct StorageWriter {
    path: PathBuf,
    tmp_path: PathBuf,
    writer: BufWriter<File>,

    starts_path: PathBuf,
    starts_writer: BufWriter<File>,

    blob_path: PathBuf,
    blob_writer: BufWriter<File>,

    entries_count: u64,
}

impl StorageWriter {
    fn create(path: &Path) -> Result<StorageWriter> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let mut writer = BufWriter::with_capacity(32 * 1024, File::create(&tmp_path)?);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut starts_path = PathBuf::from(path);
        starts_path.add_extension("starts.tmp");
        let starts_writer = BufWriter::with_capacity(32 * 1024, File::create(&starts_path)?);

        let mut blob_path = PathBuf::from(path);
        blob_path.add_extension("blob.tmp");
        let blob_writer = BufWriter::with_capacity(32 * 1024, File::create(&blob_path)?);

        Ok(StorageWriter {
            path: PathBuf::from(path),
            tmp_path,
            writer,
            starts_path,
            starts_writer,
            blob_path,
            blob_writer,
            entries_count: 0,
        })
    }

    /// Appends `feature_bytes` (a `Feature.encode_to_vec()`), recording
    /// `id` alongside it at the same position.
    fn write(&mut self, id: u64, feature_bytes: &[u8]) -> Result<()> {
        let start: u64 = self.blob_writer.stream_position()?;
        self.starts_writer.write_all(&start.to_le_bytes())?;
        self.blob_writer.write_all(feature_bytes)?;

        self.writer.write_all(&id.to_le_bytes())?;
        self.entries_count += 1;

        Ok(())
    }

    fn close(mut self) -> Result<()> {
        let blob_size: u64 = self.blob_writer.stream_position()?;
        self.starts_writer.write_all(&blob_size.to_le_bytes())?;

        let ids_offset = HEADER_SIZE as u64;
        let starts_offset = ids_offset + self.entries_count * 8;
        let blob_offset = starts_offset + (self.entries_count + 1) * 8;
        assert_eq!(self.writer.stream_position()?, starts_offset);

        self.starts_writer.flush()?; // flush() returns errors
        drop(self.starts_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.starts_path)?, &mut self.writer)?;
        remove_file(&self.starts_path)?;
        assert_eq!(self.writer.stream_position()?, blob_offset);

        self.blob_writer.flush()?; // flush() returns errors
        drop(self.blob_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.blob_path)?, &mut self.writer)?;
        remove_file(&self.blob_path)?;

        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(STORAGE_FILE_SIGNATURE)?;
        self.writer.write_all(&self.entries_count.to_le_bytes())?;
        self.writer.write_all(&ids_offset.to_le_bytes())?;
        self.writer.write_all(&starts_offset.to_le_bytes())?;
        self.writer.write_all(&blob_offset.to_le_bytes())?;
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?; // flush() returns errors
        drop(self.writer); // drop() does not return errors

        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

/// Writer for the inverted-index file: `coverage_cell_id` and `packed`
/// arrays as described in the module documentation. Unlike
/// [crate::tables::BlobTable]/[crate::tables::CoordTable]'s writers,
/// duplicate keys are expected and allowed (only a *decreasing* key is
/// rejected) -- a single S2 cell is routinely covered by several
/// features, and a feature with non-point geometry can itself contribute
/// more than one entry.
struct InvertedWriter {
    path: PathBuf,
    tmp_path: PathBuf,
    writer: BufWriter<File>,

    packed_path: PathBuf,
    packed_writer: BufWriter<File>,

    entries_count: u64,
    last_cell_id: u64,
}

impl InvertedWriter {
    fn create(path: &Path) -> Result<InvertedWriter> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let mut writer = BufWriter::with_capacity(32 * 1024, File::create(&tmp_path)?);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut packed_path = PathBuf::from(path);
        packed_path.add_extension("packed.tmp");
        let packed_writer = BufWriter::with_capacity(32 * 1024, File::create(&packed_path)?);

        Ok(InvertedWriter {
            path: PathBuf::from(path),
            tmp_path,
            writer,
            packed_path,
            packed_writer,
            entries_count: 0,
            last_cell_id: 0,
        })
    }

    fn write(&mut self, cell_id: u64, match_mask: u32, local_index: u32) -> Result<()> {
        if self.entries_count > 0 && cell_id < self.last_cell_id {
            anyhow::bail!(
                "coverage cell ids must be written in non-decreasing order, but {} < {}",
                cell_id,
                self.last_cell_id,
            );
        }

        self.writer.write_all(&cell_id.to_le_bytes())?;
        self.last_cell_id = cell_id;

        let packed = ((match_mask as u64) << 32) | (local_index as u64);
        self.packed_writer.write_all(&packed.to_le_bytes())?;

        self.entries_count += 1;
        Ok(())
    }

    fn close(mut self) -> Result<()> {
        let cell_ids_offset = HEADER_SIZE as u64;
        let packed_offset = cell_ids_offset + self.entries_count * 8;
        assert_eq!(self.writer.stream_position()?, packed_offset);

        self.packed_writer.flush()?; // flush() returns errors
        drop(self.packed_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.packed_path)?, &mut self.writer)?;
        remove_file(&self.packed_path)?;

        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(INVERTED_FILE_SIGNATURE)?;
        self.writer.write_all(&self.entries_count.to_le_bytes())?;
        self.writer.write_all(&cell_ids_offset.to_le_bytes())?;
        self.writer.write_all(&packed_offset.to_le_bytes())?;
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?; // flush() returns errors
        drop(self.writer); // drop() does not return errors

        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

/// Reads back the fixed-size `(cell_id: u64, match_mask: u32,
/// local_index: u32)` tuples [OsmFeatureIndex::create]'s pass 1 spools to
/// a plain temporary file (16 bytes each, no framing needed since the
/// record size is fixed) -- input to pass 2's external sort.
struct Pass2Reader {
    reader: BufReader<File>,
}

impl Pass2Reader {
    fn open(path: &Path) -> Result<Pass2Reader> {
        Ok(Pass2Reader {
            reader: BufReader::with_capacity(64 * 1024, File::open(path)?),
        })
    }
}

impl Iterator for Pass2Reader {
    type Item = (u64, u32, u32);

    fn next(&mut self) -> Option<Self::Item> {
        let mut buf = [0u8; 16];
        match self.reader.read_exact(&mut buf) {
            Ok(()) => Some((
                u64::from_le_bytes(buf[0..8].try_into().unwrap()),
                u32::from_le_bytes(buf[8..12].try_into().unwrap()),
                u32::from_le_bytes(buf[12..16].try_into().unwrap()),
            )),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Builds a minimal `FeatureToIndex` for tests: a bare `Feature` with
    /// just `id` set, wrapped with the given centroid/coverage/mask.
    fn fti(id: u64, centroid: u64, coverage: &[u64], mask: u16) -> FeatureToIndex {
        FeatureToIndex {
            centroid_s2_cell_id: centroid,
            feature: Some(Feature {
                id,
                ..Default::default()
            }),
            match_mask: mask as u32,
            coverage_s2_cell_id: coverage.to_vec(),
        }
    }

    fn build(records: Vec<FeatureToIndex>) -> (TempDir, OsmFeatureIndex<'static>) {
        let dir = TempDir::new().expect("tempdir");
        let out = dir.path().join("osm-features.index");
        let index = OsmFeatureIndex::create(records.into_iter(), dir.path(), &out)
            .expect("OsmFeatureIndex::create");
        (dir, index)
    }

    #[test]
    fn empty_index() {
        let (_dir, index) = build(vec![]);
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
        assert_eq!(
            index
                .query(CellID(0)..=CellID(u64::MAX), MatchMask::SHOP)
                .count(),
            0
        );
    }

    #[test]
    fn query_filters_by_range_and_mask() {
        // Three point-like features (single coverage cell == centroid),
        // spread across the u64 key space, with different masks.
        let (_dir, index) = build(vec![
            fti(101, 1_000, &[1_000], MatchMask::SHOP.0),
            fti(102, 2_000, &[2_000], MatchMask::STREET_FURNITURE.0),
            fti(103, 3_000, &[3_000], MatchMask::SHOP.0),
        ]);
        assert_eq!(index.len(), 3);

        let ids = |lo: u64, hi: u64, mask: MatchMask| -> Vec<u64> {
            index
                .query(CellID(lo)..=CellID(hi), mask)
                .map(|r| index.feature_id(r))
                .collect()
        };

        assert_eq!(ids(0, u64::MAX, MatchMask::SHOP), vec![101, 103]);
        assert_eq!(ids(0, u64::MAX, MatchMask::STREET_FURNITURE), vec![102]);
        assert_eq!(ids(0, 1_500, MatchMask::SHOP), vec![101]);
        assert_eq!(ids(1_500, u64::MAX, MatchMask::SHOP), vec![103]);
        assert_eq!(ids(0, u64::MAX, MatchMask::FUEL), Vec::<u64>::new());
    }

    #[test]
    fn get_feature_decodes_on_demand() {
        let (_dir, index) = build(vec![fti(555, 42, &[42], MatchMask::SHOP.0)]);
        let r = index
            .query(CellID(0)..=CellID(u64::MAX), MatchMask::SHOP)
            .next()
            .expect("one candidate");
        assert_eq!(index.feature_id(r), 555);
        let feature = index.get_feature(r).expect("get_feature");
        assert_eq!(feature.id, 555);
    }

    /// The correctness-critical case: a feature's `coverage_s2_cell_id`
    /// cells are deliberately *not* adjacent to its `centroid_s2_cell_id`
    /// in sort order (as can genuinely happen for multi-part geometry --
    /// see the module documentation). A query against a coverage cell
    /// must still find it, even though a (wrong) query against the
    /// centroid-sorted storage order would not.
    #[test]
    fn query_uses_coverage_cells_not_centroid_order() {
        let (_dir, index) = build(vec![
            // Centroid far from either of its own coverage cells, and
            // from the other feature's centroid/coverage -- multi-part
            // geometry whose parts are geographically split, similar to
            // e.g. two disjoint sub-polygons of a MultiPolygon.
            fti(
                /* id */ 201,
                /* centroid */ 5_000_000,
                /* coverage */ &[10, 9_999_999],
                MatchMask::SHOP.0,
            ),
            fti(
                /* id */ 202,
                /* centroid */ 20,
                /* coverage */ &[20],
                MatchMask::SHOP.0,
            ),
        ]);

        let ids = |lo: u64, hi: u64| -> Vec<u64> {
            let mut v: Vec<u64> = index
                .query(CellID(lo)..=CellID(hi), MatchMask::SHOP)
                .map(|r| index.feature_id(r))
                .collect();
            v.sort_unstable();
            v
        };

        // A query near feature 201's low coverage cell (10) must find
        // it, even though 201's centroid (5,000,000) sorts nowhere near
        // cell 10 in storage order.
        assert_eq!(ids(0, 15), vec![201]);
        // Likewise for its high coverage cell (9,999,999).
        assert_eq!(ids(9_999_990, 9_999_999), vec![201]);
        // A query at the centroid itself must find nothing (no feature
        // actually covers that cell -- it's a centroid, not coverage).
        assert_eq!(ids(5_000_000, 5_000_000), Vec::<u64>::new());
        // Feature 202 is unaffected.
        assert_eq!(ids(20, 20), vec![202]);
    }

    #[test]
    fn coverage_cell_shared_by_two_features() {
        let (_dir, index) = build(vec![
            fti(301, 100, &[500], MatchMask::SHOP.0),
            fti(302, 200, &[500], MatchMask::SHOP.0),
        ]);
        let mut ids: Vec<u64> = index
            .query(CellID(500)..=CellID(500), MatchMask::SHOP)
            .map(|r| index.feature_id(r))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![301, 302]);
    }

    #[test]
    fn modified_reflects_both_backing_files() {
        let (dir, index) = build(vec![fti(1, 1, &[1], MatchMask::SHOP.0)]);
        let modified = index.modified().expect("modified");
        let out = dir.path().join("osm-features.index");
        let inverted = OsmFeatureIndex::inverted_path(&out);
        let storage_modified = std::fs::metadata(&out).unwrap().modified().unwrap();
        let inverted_modified = std::fs::metadata(&inverted).unwrap().modified().unwrap();
        // Whichever of the two files was written last (the inverted
        // index, since create() writes it in a second pass, after the
        // feature storage) -- not necessarily the storage file.
        assert_eq!(modified, storage_modified.max(inverted_modified));
    }

    #[test]
    fn open_rejects_files_with_wrong_signature() {
        let dir = TempDir::new().expect("tempdir");
        let bad = dir.path().join("not-an-index");
        std::fs::write(&bad, b"not the right file format at all, padded to 64+ bytes so it clears the header-size check")
            .expect("write");
        assert!(OsmFeatureIndex::open(&bad).is_err());
    }
}
