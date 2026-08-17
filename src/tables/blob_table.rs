//! Disk-based, potentially very large map with `u64` keys and byte-string values.
//!
//! Unlike [crate::tables::CoordTable], whose values have a fixed size,
//! `BlobTable` values may be of any length, so it is suited for things
//! like serialized protobuf messages, WKB geeometry, or other opaque
//! binary payloads.

use crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES;
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use memmap2::Mmap;
use std::{
    fs::{File, remove_file, rename},
    io::{BufWriter, Seek, SeekFrom, Write},
    mem::size_of,
    path::{Path, PathBuf},
    time::SystemTime,
};

const HEADER_SIZE: usize = 8 * 8;
const FILE_SIGNATURE: &[u8; 8] = b"blobtbl0";

pub struct BlobTable<'a> {
    file: File,
    _mmap: Mmap,
    entries_count: usize,
    keys: &'a [u64],
    starts: &'a [u64],
    blob: &'a [u8],
}

impl<'a> BlobTable<'a> {
    /// Builds a `BlobTable` from `blobs`, which may be in any order.
    ///
    /// Since [Writer::write] requires keys in ascending order, `blobs` is
    /// first sorted by key using external sorting (spilling to `workdir`
    /// as needed), and only then written to `out`. `chunk_bytes` bounds
    /// each external-sort chunk's in-memory size (and, in turn, how many
    /// chunk files end up open at once during the final merge -- see
    /// [crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES]); a blob's actual byte
    /// size varies, so this is measured with [MemoryLimitedBufferBuilder]
    /// rather than an item count. Callers that run several external sorts
    /// concurrently (e.g. from sibling threads in the same
    /// `thread::scope`) should divide their share of the budget
    /// accordingly -- this function has no way to know how many others
    /// are running alongside it.
    pub fn create(
        blobs: impl Iterator<Item = (u64, Vec<u8>)>,
        workdir: &Path,
        out: &Path,
        chunk_bytes: usize,
    ) -> Result<BlobTable<'a>> {
        let mut writer = Writer::create(out)?;
        let sorter: ExternalSorter<(u64, Vec<u8>), std::io::Error, MemoryLimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(MemoryLimitedBufferBuilder::new(chunk_bytes as u64))
                .build()?;
        let sorted = sorter.sort(blobs.map(std::io::Result::Ok))?;
        for s in sorted {
            let (key, blob) = s?;
            writer.write(key, &blob)?;
        }
        writer.close()?;
        Self::open(out)
    }

    pub fn open(path: &Path) -> Result<BlobTable<'a>> {
        let file = File::open(path)?;

        // SAFETY: We don’t modify the file while it is mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || &mmap[0..8] != FILE_SIGNATURE {
            anyhow::bail!("not a BlobTable: {}", path.display());
        }

        // SAFETY: mmap.len() checked above; offset 0 is aligned for u64.
        let header = unsafe {
            let ptr = mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };
        let entries_count = usize::try_from(header[1])?;

        let keys = {
            let keys_count = entries_count;
            let keys_offset = usize::try_from(header[2])?;
            if keys_offset.is_multiple_of(8) && keys_offset + keys_count * 8 <= mmap.len() {
                // SAFETY: Verified alignment and length.
                unsafe {
                    let ptr = mmap.as_ptr().add(keys_offset) as *const u64;
                    std::slice::from_raw_parts(ptr, keys_count)
                }
            } else {
                anyhow::bail!("misaligned keys in BlobTable: {}", path.display());
            }
        };

        let starts = {
            let starts_count = entries_count + 1;
            let starts_offset = usize::try_from(header[3])?;
            if starts_offset.is_multiple_of(8) && starts_offset + starts_count * 8 <= mmap.len() {
                // SAFETY: Verified alignment and length.
                unsafe {
                    let ptr = mmap.as_ptr().add(starts_offset) as *const u64;
                    std::slice::from_raw_parts(ptr, starts_count)
                }
            } else {
                anyhow::bail!("misaligned starts in BlobTable: {}", path.display());
            }
        };

        let blob = {
            let blob_offset = usize::try_from(header[4])?;
            if blob_offset <= mmap.len() {
                // SAFETY: Verified length; no alignment constraints of &[u8].
                unsafe {
                    let ptr = mmap.as_ptr().add(blob_offset);
                    std::slice::from_raw_parts(ptr, mmap.len() - blob_offset)
                }
            } else {
                anyhow::bail!("misplaced blob in BlobTable: {}", path.display());
            }
        };

        Ok(BlobTable {
            file,
            _mmap: mmap,
            entries_count,
            keys,
            starts,
            blob,
        })
    }

    pub fn lookup(&self, key: u64) -> Option<&'a [u8]> {
        let idx = super::sorted_u64_index::search(self.keys, key)?;
        let start = u64::from_le(self.starts[idx]) as usize;
        let end = u64::from_le(self.starts[idx + 1]) as usize;
        Some(&self.blob[start..end])
    }

    pub fn len(&self) -> usize {
        self.entries_count
    }

    /// Returns an iterator over all entries, in ascending order of key.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &'a [u8])> + '_ {
        (0..self.entries_count).map(move |i| {
            let key = u64::from_le(self.keys[i]);
            let start = u64::from_le(self.starts[i]) as usize;
            let end = u64::from_le(self.starts[i + 1]) as usize;
            (key, &self.blob[start..end])
        })
    }

    /// Returns the modification time of the backing file.
    #[allow(unused)]
    pub fn modified(&self) -> Result<SystemTime> {
        Ok(self.file.metadata()?.modified()?)
    }
}

struct Writer {
    path: PathBuf,
    tmp_path: PathBuf,
    writer: BufWriter<File>,

    starts_path: PathBuf,
    starts_writer: BufWriter<File>,

    blob_path: PathBuf,
    blob_writer: BufWriter<File>,

    entries_count: u64,
    last_key: Option<u64>,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Writer> {
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

        Ok(Writer {
            path: PathBuf::from(path),
            tmp_path,
            writer,
            starts_path,
            starts_writer,
            blob_path,
            blob_writer,
            entries_count: 0,
            last_key: None,
        })
    }

    /// Appends `blob` under `key`. Keys must be written in strictly
    /// ascending order, so that [BlobTable::lookup] can search them via
    /// [super::sorted_u64_index].
    pub fn write(&mut self, key: u64, blob: &[u8]) -> Result<()> {
        if let Some(last_key) = self.last_key
            && key <= last_key
        {
            anyhow::bail!(
                "keys must be written in ascending order, but {} <= {}",
                key,
                last_key,
            );
        }

        let start: u64 = self.blob_writer.stream_position()?;
        self.starts_writer.write_all(&start.to_le_bytes())?;
        self.blob_writer.write_all(blob)?;

        self.writer.write_all(&key.to_le_bytes())?;
        self.last_key = Some(key);
        self.entries_count += 1;

        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        // Write sentinel to end of starts array.
        let blob_size: u64 = self.blob_writer.stream_position()?;
        self.starts_writer.write_all(&blob_size.to_le_bytes())?;

        let keys_offset = HEADER_SIZE as u64;
        let starts_offset = keys_offset + self.entries_count * 8;
        let blob_offset = starts_offset + (self.entries_count + 1) * 8;
        assert_eq!(self.writer.stream_position()?, starts_offset);

        // Copy starts array into the output file.
        self.starts_writer.flush()?; // flush() returns errors
        drop(self.starts_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.starts_path)?, &mut self.writer)?;
        remove_file(&self.starts_path)?;
        assert_eq!(self.writer.stream_position()?, blob_offset);

        // Copy blob data into the output file.
        self.blob_writer.flush()?; // flush() returns errors
        drop(self.blob_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.blob_path)?, &mut self.writer)?;
        remove_file(&self.blob_path)?;

        // Write file header.
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(FILE_SIGNATURE)?; // header[0] = magic
        self.writer.write_all(&self.entries_count.to_le_bytes())?; // header[1] = entries_count
        self.writer.write_all(&keys_offset.to_le_bytes())?; // header[2] = keys_offset
        self.writer.write_all(&starts_offset.to_le_bytes())?; // header[3] = starts_offset
        self.writer.write_all(&blob_offset.to_le_bytes())?; // header[4] = blob_offset
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?; // flush() returns errors
        drop(self.writer); // drop() does not return errors

        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_create_sorts_unsorted_input() -> Result<()> {
        let workdir = TempDir::new()?;
        let file = NamedTempFile::new()?;
        let blobs = vec![
            (44, b"melbourne".to_vec()),
            (17, b"bern".to_vec()),
            (42, b"ottawa".to_vec()),
            (41, b"".to_vec()),
        ];

        let table = BlobTable::create(
            blobs.into_iter(),
            workdir.path(),
            file.path(),
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(table.len(), 4);
        assert_eq!(table.lookup(0), None);
        assert_eq!(table.lookup(17), Some(b"bern".as_slice()));
        assert_eq!(table.lookup(41), Some(b"".as_slice()));
        assert_eq!(table.lookup(42), Some(b"ottawa".as_slice()));
        assert_eq!(table.lookup(43), None);
        assert_eq!(table.lookup(44), Some(b"melbourne".as_slice()));

        Ok(())
    }

    #[test]
    fn test_blob_table() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut writer = Writer::create(file.path())?;
        writer.write(17, b"bern")?;
        writer.write(41, b"")?;
        writer.write(42, b"ottawa")?;
        writer.write(44, b"melbourne")?;
        writer.close()?;
        let file_metadata = std::fs::metadata(file.path())?;

        let table = BlobTable::open(file.path())?;
        assert_eq!(table.modified()?, file_metadata.modified()?);
        assert_eq!(table.len(), 4);

        assert_eq!(table.lookup(0), None);
        assert_eq!(table.lookup(16), None);
        assert_eq!(table.lookup(17), Some(b"bern".as_slice()));
        assert_eq!(table.lookup(18), None);
        assert_eq!(table.lookup(41), Some(b"".as_slice()));
        assert_eq!(table.lookup(42), Some(b"ottawa".as_slice()));
        assert_eq!(table.lookup(43), None);
        assert_eq!(table.lookup(44), Some(b"melbourne".as_slice()));
        assert_eq!(table.lookup(99), None);

        Ok(())
    }

    #[test]
    fn test_iter() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut writer = Writer::create(file.path())?;
        writer.write(17, b"bern")?;
        writer.write(41, b"")?;
        writer.write(42, b"ottawa")?;
        writer.close()?;

        let table = BlobTable::open(file.path())?;
        let entries: Vec<(u64, &[u8])> = table.iter().collect();
        assert_eq!(
            entries,
            vec![
                (17, b"bern".as_slice()),
                (41, b"".as_slice()),
                (42, b"ottawa".as_slice()),
            ]
        );

        Ok(())
    }

    #[test]
    fn test_write_requires_ascending_keys() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut writer = Writer::create(file.path())?;
        writer.write(42, b"a")?;
        assert!(writer.write(41, b"b").is_err());
        assert!(writer.write(42, b"c").is_err());
        Ok(())
    }
}
