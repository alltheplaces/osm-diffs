//! Disk-based, potentially very large map with `String` keys and `u64`
//! counter values.
//!
//! `StringCounts` is used to tally how often each distinct string occurs
//! across a large input (for example, how often each tag key appears in
//! an OpenStreetMap planet dump), without keeping every string in RAM at
//! once. Entries with the same key passed to [StringCounts::create] are
//! summed into a single counter; an entry whose total ends up at zero is
//! dropped rather than written out.
//!
//! # File format
//!
//! ```text
//! byte 0..8:   magic "strcnt_0"
//! byte 8..16:  entry count, u64 little-endian
//! byte 16..24: offset of the chars array, u64 little-endian
//! byte 24..32: size of the chars array, u64 little-endian
//! byte 32..40: offset of the char_offsets array, u64 little-endian
//!              (always equal to the header size, 64)
//! byte 40..48: offset of the counter_values array, u64 little-endian
//! byte 48..64: reserved, zero-filled
//!
//! char_offsets array:   `entry count + 1` entries, u64 little-endian
//!                       each, immediately following the header.
//!                       `char_offsets[i]` is the byte offset into the
//!                       chars array where the string of the i-th entry
//!                       (in ascending order) begins;
//!                       `char_offsets[entry count]` is a sentinel equal
//!                       to the size of the chars array.
//!
//! counter_values array: `entry count` entries, u64 little-endian each,
//!                       aligned 1:1 with char_offsets, immediately
//!                       following it. `counter_values[i]` is the summed
//!                       count for the i-th entry's string.
//!
//! chars array:          the UTF-8 bytes of every string, concatenated in
//!                       ascending order, immediately following the
//!                       counter_values array.
//! ```
//!
//! The header is a fixed 64 bytes so that the char_offsets and
//! counter_values arrays, which follow it back to back, stay 8-byte
//! aligned and can be reinterpreted as `&[u64]` slices directly on the
//! mmap'd bytes.

use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use memmap2::Mmap;
use std::{
    fs::{File, remove_file, rename},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

/// Read-only, memory-mapped map from `String` keys to `u64` counter
/// values, iterated in ascending order of key by [StringCounts::iter].
/// See the "File format" section above.
pub struct StringCounts<'a> {
    file: File,
    _mmap: Mmap,
    entries_count: usize,

    chars: &'a [u8],
    char_offsets: &'a [u64],
    counter_values: &'a [u64],
}

/// Size of the file header, in bytes: see the "File format" section above.
const HEADER_SIZE: usize = 8 * 8;

/// Magic bytes identifying a `StringCounts` file, written as the first
/// eight bytes of the file header.
const FILE_SIGNATURE: &[u8; 8] = b"strcnt_0";

impl<'a> StringCounts<'a> {
    /// Builds a `StringCounts` from `counts`, which may be in any order
    /// and may repeat the same key multiple times; repeated keys are
    /// summed into a single entry.
    ///
    /// Since [Writer::write] requires keys in ascending order, `counts` is
    /// first sorted by key using external sorting (spilling to `workdir`
    /// as needed), and only then written to `out`.
    pub fn create(
        counts: impl Iterator<Item = (String, u64)>,
        workdir: &Path,
        out: &Path,
    ) -> Result<StringCounts<'a>> {
        let mut writer = Writer::create(out)?;
        let entry_count = AtomicU64::new(0);
        let sorter: ExternalSorter<(String, u64), std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    8 * 1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        let sorted = sorter.sort(counts.map(|entry| {
            entry_count.fetch_add(1, Ordering::SeqCst);
            std::io::Result::Ok(entry)
        }))?;
        for entry in sorted {
            let (key, count) = entry?;
            writer.write(key, count)?;
        }
        writer.close()?;
        Self::open(out)
    }

    /// Opens a `StringCounts` previously written by
    /// [StringCounts::create], mapping it into memory rather than reading
    /// it into a heap-allocated buffer.
    pub fn open(path: &Path) -> Result<StringCounts<'a>> {
        let file = File::open(path)?;

        // SAFETY: We don’t modify the file while it is mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || &mmap[0..8] != FILE_SIGNATURE {
            anyhow::bail!("not a StringCounts file: {}", path.display());
        }

        // SAFETY: mmap.len() checked above; offset 0 is aligned for u64.
        let header = unsafe {
            let ptr = mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };

        let entries_count = usize::try_from(header[1])?;

        let chars = {
            let offset = usize::try_from(header[2])?;
            let len = usize::try_from(header[3])?;
            if offset + len <= mmap.len() {
                // SAFETY: Verified length; no alignment constraints of &[u8].
                unsafe {
                    let ptr = mmap.as_ptr().add(offset);
                    std::slice::from_raw_parts(ptr, len)
                }
            } else {
                anyhow::bail!(
                    "misplaced chars in {}: mmap.len={}, offset={}, len={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    len
                );
            }
        };

        let char_offsets = {
            let offset = usize::try_from(header[4])?;
            let len = entries_count + 1; // + 1 for final sentinel entry
            if offset.is_multiple_of(8) && offset + len * 8 <= mmap.len() {
                // SAFETY: Verified alignment and length.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u64;
                    std::slice::from_raw_parts(ptr, len)
                }
            } else {
                anyhow::bail!(
                    "misplaced char_offsets in {}: mmap.len={}, offset={}, len={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    len
                );
            }
        };

        let counter_values = {
            let offset = usize::try_from(header[5])?;
            let len = entries_count;
            if offset.is_multiple_of(8) && offset + len * 8 <= mmap.len() {
                // SAFETY: Verified alignment and length.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u64;
                    std::slice::from_raw_parts(ptr, len)
                }
            } else {
                anyhow::bail!(
                    "misplaced counter_values in {}: mmap.len={}, offset={}, len={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    len
                );
            }
        };

        Ok(StringCounts {
            file,
            _mmap: mmap,
            entries_count,
            chars,
            char_offsets,
            counter_values,
        })
    }

    /// Returns the number of entries in the table.
    pub fn len(&self) -> usize {
        self.entries_count
    }

    /// Returns an iterator over all entries, as `(key, count)` pairs, in
    /// ascending order of key.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, u64)> + '_ {
        (0..self.len()).map(move |i| {
            let start = usize::try_from(u64::from_le(self.char_offsets[i])).expect("char_offset");
            let limit =
                usize::try_from(u64::from_le(self.char_offsets[i + 1])).expect("char_offset");

            // SAFETY: Writer API only accepts Rust strings, which are valid UTF-8.
            let chars = unsafe { str::from_utf8_unchecked(&self.chars[start..limit]) };
            let counter_value = u64::from_le(self.counter_values[i]);
            (chars, counter_value)
        })
    }

    /// Returns the modification time of the backing file.
    pub fn modified(&self) -> Result<SystemTime> {
        Ok(self.file.metadata()?.modified()?)
    }
}

struct Writer {
    path: PathBuf,
    tmp_path: PathBuf,
    writer: BufWriter<File>,
    strings_path: PathBuf,
    strings_writer: BufWriter<File>,
    values_path: PathBuf,
    values_writer: BufWriter<File>,
    strings_count: u64,
    last_key: String,
    last_value: u64,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Writer> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let mut writer = BufWriter::with_capacity(32 * 1024, File::create(&tmp_path)?);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut strings_path = PathBuf::from(path);
        strings_path.add_extension("strings.tmp");
        let strings_writer = BufWriter::with_capacity(32 * 1024, File::create(&strings_path)?);

        let mut values_path = PathBuf::from(path);
        values_path.add_extension("values.tmp");
        let values_writer = BufWriter::with_capacity(32 * 1024, File::create(&values_path)?);

        Ok(Writer {
            path: PathBuf::from(path),
            tmp_path,
            writer,
            strings_path,
            strings_writer,
            values_path,
            values_writer,
            strings_count: 0,
            last_key: String::from(""),
            last_value: 0,
        })
    }

    fn write(&mut self, key: String, value: u64) -> Result<()> {
        if key == self.last_key {
            self.last_value += value;
            return Ok(());
        }

        self.write_last_entry()?;
        self.last_key = key;
        self.last_value = value;
        Ok(())
    }

    fn write_last_entry(&mut self) -> Result<()> {
        if self.last_value == 0 {
            return Ok(());
        }

        let char_offset: u64 = self.strings_writer.stream_position()?;
        self.writer.write_all(&char_offset.to_le_bytes())?;
        self.strings_writer.write_all(self.last_key.as_bytes())?;
        self.values_writer
            .write_all(&self.last_value.to_le_bytes())?;

        self.strings_count += 1;
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        self.write_last_entry()?;

        let strings_size: u64 = self.strings_writer.stream_position()?;
        self.writer.write_all(&strings_size.to_le_bytes())?;

        let pos_offset = HEADER_SIZE as u64;
        let counter_values_offset = pos_offset + (self.strings_count + 1) * 8;
        let strings_offset = counter_values_offset + self.strings_count * 8;
        assert_eq!(self.writer.stream_position()?, counter_values_offset);

        // Write file header.
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(FILE_SIGNATURE)?; // header[0]
        self.writer.write_all(&self.strings_count.to_le_bytes())?; // header[1]
        self.writer.write_all(&strings_offset.to_le_bytes())?; // header[2]
        self.writer.write_all(&strings_size.to_le_bytes())?; // header[3]
        self.writer.write_all(&pos_offset.to_le_bytes())?; // header[4]
        self.writer
            .write_all(&counter_values_offset.to_le_bytes())?; // header[5]
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        // Copy counter values into the output file.
        self.writer.seek(SeekFrom::Start(counter_values_offset))?;
        self.values_writer.flush()?; // flush() returns errors
        drop(self.values_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.values_path)?, &mut self.writer)?;
        remove_file(&self.values_path)?;

        // Copy strings buffer into the output file.
        assert_eq!(self.writer.stream_position()?, strings_offset);
        self.strings_writer.flush()?; // flush() returns errors
        drop(self.strings_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.strings_path)?, &mut self.writer)?;
        remove_file(&self.strings_path)?;

        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tempfile::TempDir;

    const TEST_COUNTER: LazyLock<StringCounts> = LazyLock::new(|| {
        let entries = &[("foo", 1), ("bar", 7), ("foo", 2), ("foo", 3)];
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("test.StringCounts");
        StringCounts::create(
            entries.map(|(s, n)| (String::from(s), n)).into_iter(),
            workdir.path(),
            &path,
        )
        .expect("StringCounts::create() failed")
    });

    #[test]
    fn test_iter() {
        assert_eq!(
            TEST_COUNTER.iter().collect::<Vec<(&str, u64)>>(),
            &[("bar", 7), ("foo", 6)]
        );
    }

    #[test]
    fn test_len() {
        assert_eq!(TEST_COUNTER.len(), 2);
    }

    #[test]
    fn test_modified() -> Result<()> {
        let entries = &[("hello", 1_u64)];
        let workdir = TempDir::new()?;
        let path = workdir.path().join("test.StringCounts");
        StringCounts::create(
            entries.map(|(s, n)| (String::from(s), n)).into_iter(),
            workdir.path(),
            &path,
        )?;

        let table = StringCounts::open(&path)?;
        let file_metadata = std::fs::metadata(&path)?;
        assert_eq!(table.modified()?, file_metadata.modified()?);
        Ok(())
    }

    #[test]
    fn test_open_rejects_file_without_signature() {
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("not-a-counter");
        std::fs::write(&path, [0u8; HEADER_SIZE]).expect("write failed");
        assert!(StringCounts::open(&path).is_err());
    }

    #[test]
    fn test_open_rejects_truncated_file() {
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("too-short");
        std::fs::write(&path, FILE_SIGNATURE).expect("write failed");
        assert!(StringCounts::open(&path).is_err());
    }
}
