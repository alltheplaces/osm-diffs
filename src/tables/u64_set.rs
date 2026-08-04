//! Disk-based, memory-mapped set of `u64` values.
//!
//! `U64Set` is used in the pipeline to represent large sets of identifiers
//! that may not entirely fit into the available memory. For example, the
//! set of all OpenStreetMap nodes that are geographically near an
//! AllThePlaces feature.
//!
//! Containment test ([U64Set::contains]) is currently implemented as a
//! regular binary search. If performance ever becomes an issue, consider
//! Cache-Sensitive Skip Lists, but this would make the file format
//! slightly more complicated.
//!
//! # File format
//!
//! ```text
//! byte 0..8:  magic "u64set_0"
//! byte 8..16: entry count, u64 little-endian
//! byte 16..:  the set's elements, sorted ascending, deduplicated, u64
//!             little-endian each
//! ```
//!
//! The header is a fixed 16 bytes so that the elements array, which
//! follows it immediately, stays 8-byte aligned and can be reinterpreted
//! as a `&[u64]` slice directly on the mmap'd bytes.

use anyhow::{Ok, Result, anyhow};
use memmap2::Mmap;
use std::{fs::File, mem::size_of, path::Path, time::SystemTime};

/// Size of the file header, in bytes: see the "File format" section above.
const HEADER_SIZE: usize = 2 * 8;

/// Magic bytes identifying a `U64Set` file, written as the first eight
/// bytes of the file header.
const FILE_SIGNATURE: &[u8; 8] = b"u64set_0";

/// Read-only, memory-mapped set of `u64` values. See the "File format"
/// section above.
pub struct U64Set {
    file: File, // The file that backs mmap.
    mmap: Mmap,
    entries_count: usize,
}

impl U64Set {
    /// Builds a `U64Set` from `elements`, which may be in any order and
    /// may contain duplicates.
    ///
    /// `elements` is sorted and deduplicated using external sorting
    /// (spilling to `workdir` as needed), and only then written to `out`.
    pub fn create(
        elements: impl Iterator<Item = u64>,
        workdir: &Path,
        out: &Path,
    ) -> Result<U64Set> {
        _ = writer::create(elements, workdir, out)?;
        Self::open(out)
    }

    /// Opens a `U64Set` previously written by [U64Set::create], mapping it
    /// into memory rather than reading it into a heap-allocated buffer.
    pub fn open(path: &Path) -> Result<U64Set> {
        let file = File::open(path)?;

        // SAFETY: We don’t truncate the file while it is mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || &mmap[0..8] != FILE_SIGNATURE {
            return Err(anyhow!("not a U64Set: {}", path.display()));
        }

        // SAFETY: mmap.len() checked above; offset 0 is aligned for u64.
        let header = unsafe {
            let ptr = mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };
        let entries_count = usize::try_from(header[1])?;
        if HEADER_SIZE + entries_count * 8 != mmap.len() {
            return Err(anyhow!("bad element count in U64Set: {}", path.display()));
        }

        Ok(U64Set {
            file,
            mmap,
            entries_count,
        })
    }

    /// Returns whether `n` is in the set.
    pub fn contains(&self, n: u64) -> bool {
        // SAFETY: We check in `open()` that the file holds `entries_count`
        // elements after the header. Alignment to page size, which is
        // typically 4K or larger and always than eight bytes, is
        // guaranteed by the mmap system call; the header size is a
        // multiple of eight bytes, so the elements stay eight-byte
        // aligned too.
        let slice = unsafe {
            let ptr = self.mmap.as_ptr().add(HEADER_SIZE) as *const u64;
            std::slice::from_raw_parts(ptr, self.entries_count)
        };

        if cfg!(target_endian = "little") {
            slice.binary_search(&n).is_ok()
        } else {
            slice.binary_search_by(|x| x.swap_bytes().cmp(&n)).is_ok()
        }
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> usize {
        self.entries_count
    }

    /// Returns an iterator over all elements, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        // SAFETY: See `contains()` above.
        let slice = unsafe {
            let ptr = self.mmap.as_ptr().add(HEADER_SIZE) as *const u64;
            std::slice::from_raw_parts(ptr, self.entries_count)
        };
        (0..self.entries_count).map(move |i| u64::from_le(slice[i]))
    }

    /// Returns the modification time of the underlying file.
    #[allow(unused)]
    pub fn modified(&self) -> Result<SystemTime> {
        Ok(self.file.metadata()?.modified()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, sync::LazyLock};
    use tempfile::{NamedTempFile, TempDir};

    const TEST_TABLE: LazyLock<U64Set> = LazyLock::new(|| {
        let mut file = NamedTempFile::new().expect("NamedTempFile");
        file.write_all(FILE_SIGNATURE).expect("File::write");
        file.write_all(&3_u64.to_le_bytes()).expect("File::write");
        for i in [7_u64, 23, 42] {
            file.write_all(&i.to_le_bytes()).expect("File::write");
        }
        U64Set::open(&file.path()).expect("U64Set::open")
    });

    #[test]
    fn test_contains() {
        assert_eq!(TEST_TABLE.contains(u64::MIN), false);
        assert_eq!(TEST_TABLE.contains(7), true);
        assert_eq!(TEST_TABLE.contains(23), true);
        assert_eq!(TEST_TABLE.contains(41), false);
        assert_eq!(TEST_TABLE.contains(42), true);
        assert_eq!(TEST_TABLE.contains(43), false);
        assert_eq!(TEST_TABLE.contains(u64::MAX), false);
    }

    #[test]
    fn test_iter() {
        assert_eq!(TEST_TABLE.iter().collect::<Vec<u64>>(), &[7, 23, 42]);
    }

    #[test]
    fn test_len() {
        assert_eq!(TEST_TABLE.len(), 3);
    }

    #[test]
    fn test_modified() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(FILE_SIGNATURE)?;
        file.write_all(&1_u64.to_le_bytes())?;
        file.write_all(&42_u64.to_le_bytes())?;

        let table = U64Set::open(file.path())?;
        let file_metadata = std::fs::metadata(&file.path())?;
        assert_eq!(table.modified()?, file_metadata.modified()?);
        Ok(())
    }

    #[test]
    fn test_open() -> Result<()> {
        let file = NamedTempFile::new()?;

        // `open()` should reject a file without a header.
        assert!(U64Set::open(file.path()).is_err());

        // `open()` should accept a header-only file with no elements.
        std::fs::write(
            file.path(),
            [FILE_SIGNATURE.as_slice(), &0_u64.to_le_bytes()].concat(),
        )?;
        assert!(U64Set::open(file.path()).is_ok());

        // `open()` should accept a file with two elements after the header.
        std::fs::write(
            file.path(),
            [
                FILE_SIGNATURE.as_slice(),
                &2_u64.to_le_bytes(),
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            ]
            .concat(),
        )?;
        assert!(U64Set::open(file.path()).is_ok());

        // `open()` should not accept a file whose size doesn't match the
        // element count in the header.
        std::fs::write(
            file.path(),
            [
                FILE_SIGNATURE.as_slice(),
                &2_u64.to_le_bytes(),
                &[0, 1, 2, 3, 4, 5, 6, 7],
            ]
            .concat(),
        )?;
        assert!(U64Set::open(file.path()).is_err());

        // `open()` should not accept a file with a wrong magic signature.
        std::fs::write(
            file.path(),
            [b"notu64s0".as_slice(), &0_u64.to_le_bytes()].concat(),
        )?;
        assert!(U64Set::open(file.path()).is_err());

        Ok(())
    }

    #[test]
    fn test_open_inexistent_file() {
        let path = Path::new("file/does/not/exist");
        assert!(U64Set::open(&path).is_err());
    }

    #[test]
    fn test_create() -> Result<()> {
        let workdir = TempDir::new()?;
        let out = workdir.path().join("test.u64set");

        let set = U64Set::create([42, 7, 23, 7].into_iter(), workdir.path(), &out)?;
        assert_eq!(set.len(), 3);
        assert_eq!(set.contains(0), false);
        assert_eq!(set.contains(7), true);
        assert_eq!(set.contains(23), true);
        assert_eq!(set.contains(42), true);
        assert_eq!(set.contains(99), false);
        assert_eq!(set.iter().collect::<Vec<u64>>(), &[7, 23, 42]);

        Ok(())
    }
}

mod writer {
    use super::{FILE_SIGNATURE, HEADER_SIZE};
    use anyhow::{Ok, Result};
    use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
    use std::fs::File;
    use std::io::{BufWriter, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    pub fn create(elements: impl Iterator<Item = u64>, workdir: &Path, out: &Path) -> Result<u64> {
        let mut tmp_out = PathBuf::from(out);
        tmp_out.add_extension("tmp");
        let sorter: ExternalSorter<u64, std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    /* buffer_size */ 5_000_000, /* preallocate */ true,
                ))
                .build()?;
        let sorted = sorter.sort(elements.map(std::io::Result::Ok))?;
        let file = File::create(&tmp_out)?;
        let mut writer = BufWriter::with_capacity(32768, file);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut num_unique_values: u64 = 0;
        let mut last: Option<u64> = None;
        for value in sorted {
            let value = value?;
            if let Some(last) = last
                && last != value
            {
                writer.write_all(&last.to_le_bytes())?;
                num_unique_values += 1;
            }
            last = Some(value);
        }
        if let Some(last) = last {
            writer.write_all(&last.to_le_bytes())?;
            num_unique_values += 1;
        }

        // Write file header.
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(FILE_SIGNATURE)?; // header[0] = magic
        writer.write_all(&num_unique_values.to_le_bytes())?; // header[1] = entries_count
        writer.seek(SeekFrom::End(0))?;

        writer.flush()?;
        writer.into_inner()?.sync_all()?;
        std::fs::rename(&tmp_out, out)?;
        Ok(num_unique_values)
    }

    #[cfg(test)]
    mod tests {
        use super::super::U64Set;
        use anyhow::{Ok, Result};

        #[test]
        fn test_create() -> Result<()> {
            let tmp = tempfile::TempDir::new()?;
            let out = tmp.path().join("test.u64_table");

            let num_written = super::create(
                /* elements */ [42, 23, 23, 7777, 23].into_iter(),
                /* workdir */ tmp.path(),
                /* out */ &out,
            )?;
            assert_eq!(num_written, 3);

            let table = U64Set::open(&out)?;
            assert_eq!(table.contains(4), false);
            assert_eq!(table.contains(23), true);
            assert_eq!(table.contains(42), true);
            assert_eq!(table.contains(7777), true);
            assert_eq!(table.contains(123), false);

            Ok(())
        }

        #[test]
        fn test_create_single_value() -> Result<()> {
            let tmp = tempfile::TempDir::new()?;
            let out = tmp.path().join("test.u64_table");

            let num_written = super::create([9].into_iter(), tmp.path(), &out)?;
            assert_eq!(num_written, 1);

            let table = U64Set::open(&out)?;
            assert_eq!(table.contains(4), false);
            assert_eq!(table.contains(8), false);
            assert_eq!(table.contains(9), true);
            assert_eq!(table.contains(10), false);

            Ok(())
        }

        #[test]
        fn test_create_empty() -> Result<()> {
            let tmp = tempfile::TempDir::new()?;
            let out = tmp.path().join("test.u64_table");

            let num_written = super::create([].into_iter(), tmp.path(), &out)?;
            assert_eq!(num_written, 0);

            let table = U64Set::open(&out)?;
            assert_eq!(table.contains(42), false);

            Ok(())
        }
    }
}
