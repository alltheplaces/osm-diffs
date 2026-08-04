//! Disk-based table of strings that supports both lookup by index and
//! lookup by value.
//!
//! `StringPool` is used to intern the tag keys and values of OpenStreetMap
//! features: rather than storing the same short strings over and over in
//! every record, other tables store a `u32` index into a shared
//! `StringPool` and use [StringPool::get] to resolve it back to text, or
//! [StringPool::lookup] to go from text to index while building those
//! records. The pool itself does not deduplicate its input; callers that
//! rely on `lookup` being a well-defined inverse of `get` must supply
//! already-unique strings (as `assemble_strings` in
//! `pipeline/osm/assemble.rs` does). If a string is written more than
//! once, `lookup` returns the smallest of the indices under which it was
//! stored.
//!
//! # File format
//!
//! ```text
//! byte 0..8:    magic "strpool0"
//! byte 8..16:   entry count, u64 little-endian
//! byte 16..24:  offset of the buckets array, u64 little-endian
//! byte 24..32:  size of the buckets array, u64 little-endian
//! byte 32..40:  offset of the hash_index array, u64 little-endian
//! byte 40..48:  size of the hash_index array, u64 little-endian
//! byte 48..56:  offset of the hash_values array, u64 little-endian
//! byte 56..64:  size of the hash_values array, u64 little-endian
//! byte 64..72:  offset of the starts array, u64 little-endian
//! byte 72..80:  size of the starts array, u64 little-endian
//! byte 80..88:  offset of the chars array, u64 little-endian
//! byte 88..96:  size of the chars array, u64 little-endian
//! byte 96..128: reserved, zero-filled
//!
//! buckets array:     65537 buckets, u32 little-endian each, 64-byte
//!                    aligned. `buckets[b]` is the index into the
//!                    hash_index/hash_values arrays of the first entry
//!                    whose 32-bit hash has `b` in its upper 16 bits;
//!                    `buckets[65536]` is a sentinel equal to `entry
//!                    count`. Monotonically non-decreasing, so bucket `b`
//!                    occupies the half-open range
//!                    `buckets[b]..buckets[b + 1]` in both arrays below.
//!
//! hash_index array:  `entry count` entries, u32 little-endian each, in
//!                    ascending order of the entries' 32-bit hash (and,
//!                    for equal hashes, ascending original index).
//!                    `hash_index[p]` is the original insertion index
//!                    (i.e. an index into the starts array) of the p-th
//!                    entry in that order.
//!
//! hash_values array: `entry count` entries, u16 little-endian each,
//!                    aligned 1:1 with hash_index. `hash_values[p]` holds
//!                    the lower 16 bits of the hash of `hash_index[p]`'s
//!                    string; since entries are grouped by bucket (the
//!                    upper 16 bits) and then sorted by hash overall,
//!                    each bucket's slice of this array is itself sorted
//!                    ascending, which is what makes binary search in
//!                    [StringPool::lookup] possible.
//!
//! starts array:      `entry count + 1` entries, u64 little-endian each.
//!                    `starts[i]` is the byte offset into the chars array
//!                    where the string at original index `i` begins;
//!                    `starts[entry count]` is a sentinel equal to the
//!                    size of the chars array. The string at index `i`
//!                    thus occupies `chars[starts[i]..starts[i + 1]]`.
//!
//! chars array:       the UTF-8 bytes of every string, concatenated in
//!                    original insertion order.
//! ```
//!
//! Lookup by value first computes the 32-bit hash of the query string,
//! uses its upper 16 bits to find the bucket's range in `hash_values` via
//! `buckets`, then binary-searches that range for the lower 16 bits of
//! the hash and scans the (usually very short) run of equal values,
//! comparing full strings to rule out 16-bit hash collisions.

use anyhow::{Context, Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use memmap2::Mmap;
use std::{
    fs::{File, remove_file, rename},
    hash::{DefaultHasher, Hash, Hasher},
    io,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// Read-only, memory-mapped table of strings, retrievable both by
/// original insertion index ([StringPool::get]) and by value
/// ([StringPool::lookup]). See the "File format" section above.
pub struct StringPool<'a> {
    _file: File,
    _mmap: Mmap,
    len: usize,
    buckets: &'a [u32],
    hash_index: &'a [u32],
    hash_values: &'a [u16],
    chars: &'a [u8],
    starts: &'a [u64],
}

/// Size of the file header, in bytes: see the "File format" section above.
const HEADER_SIZE: usize = 16 * 8;

/// Magic bytes identifying a `StringPool` file, written as the first
/// eight bytes of the file header.
const FILE_SIGNATURE: &[u8; 8] = b"strpool0";

/// Number of hash buckets, keyed by the upper 16 bits of a 32-bit hash.
const BUCKET_COUNT: usize = 65536;

type Buckets = Vec<u32>;

impl<'a> StringPool<'a> {
    /// Builds a `StringPool` from `strings`, written to `path` in
    /// iteration order (`strings.next()` becomes index 0, and so on).
    ///
    /// Building requires sorting every entry's hash, which may spill
    /// intermediate data to `workdir`; see [Writer::close].
    pub fn create(
        strings: impl Iterator<Item = String>,
        workdir: &Path,
        path: &Path,
    ) -> Result<StringPool<'a>> {
        let mut writer = Writer::create(workdir, path)?;
        for s in strings {
            writer.write(&s)?;
        }
        writer.close()?;
        Self::open(path)
    }

    /// Opens a `StringPool` previously written by [StringPool::create],
    /// mapping it into memory rather than reading it into a
    /// heap-allocated buffer.
    pub fn open(path: &Path) -> Result<StringPool<'a>> {
        let file = File::open(path)?;

        // SAFETY: We don’t modify the file while it is mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || &mmap[0..8] != FILE_SIGNATURE {
            anyhow::bail!("not a StringPool: {}", path.display());
        }

        // SAFETY: mmap.len() checked above.
        let header = unsafe {
            let ptr = mmap.as_ptr() as *const u64;
            std::slice::from_raw_parts(ptr, HEADER_SIZE / size_of::<u64>())
        };
        let len = usize::try_from(header[1])?;

        let buckets = {
            let offset = usize::try_from(header[2])?;
            let size = usize::try_from(header[3])?;
            if offset + size <= mmap.len()
                && offset.is_multiple_of(64)
                && size == (BUCKET_COUNT + 1) * 4
            {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u32;
                    std::slice::from_raw_parts(ptr, size / 4)
                }
            } else {
                anyhow::bail!(
                    "misplaced buckets in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };
        if !is_u32_slice_sorted_little_endian(buckets) {
            anyhow::bail!("buckets not sorted: {}", path.display());
        }

        let hash_index = {
            let offset = usize::try_from(header[4])?;
            let size = usize::try_from(header[5])?;
            if offset + size <= mmap.len() && offset.is_multiple_of(4) && size.is_multiple_of(4) {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u32;
                    std::slice::from_raw_parts(ptr, size / 4)
                }
            } else {
                anyhow::bail!(
                    "misplaced hash_index in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };

        let hash_values = {
            let offset = usize::try_from(header[6])?;
            let size = usize::try_from(header[7])?;
            if offset + size <= mmap.len() && offset.is_multiple_of(2) && size.is_multiple_of(2) {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u16;
                    std::slice::from_raw_parts(ptr, size / 2)
                }
            } else {
                anyhow::bail!(
                    "misplaced hash_values in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };

        let starts = {
            let offset = usize::try_from(header[8])?;
            let size = usize::try_from(header[9])?;
            if offset + size <= mmap.len() && offset.is_multiple_of(8) && size.is_multiple_of(8) {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u64;
                    std::slice::from_raw_parts(ptr, size / 8)
                }
            } else {
                anyhow::bail!(
                    "misplaced starts in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };

        let chars = {
            let offset = usize::try_from(header[10])?;
            let size = usize::try_from(header[11])?;
            if offset + size <= mmap.len() {
                // SAFETY: Verified length; no alignment constraints of &[u8].
                unsafe {
                    let ptr = mmap.as_ptr().add(offset);
                    std::slice::from_raw_parts(ptr, size)
                }
            } else {
                anyhow::bail!(
                    "misplaced chars in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };

        Ok(StringPool {
            _file: file,
            _mmap: mmap,
            len,
            buckets,
            hash_index,
            hash_values,
            chars,
            starts,
        })
    }

    /// Returns the string that was written at index `idx` in
    /// [StringPool::create], in `O(1)`.
    ///
    /// Panics if `idx >= self.len()`.
    #[allow(unused)]
    pub fn get(&self, idx: usize) -> &'a str {
        let start = u64::from_le(self.starts[idx]) as usize;
        let end = u64::from_le(self.starts[idx + 1]) as usize;
        // SAFETY: Writer API only takes Rust strings, which are valid UTF-8.
        unsafe { str::from_utf8_unchecked(&self.chars[start..end]) }
    }

    /// Returns the number of entries in the table.
    #[allow(unused)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the index under which `key` was stored (as accepted by
    /// [StringPool::get]), or `None` if `key` is not in the pool.
    ///
    /// Looks `key` up via the on-disk hash table described in the "File
    /// format" section above, rather than scanning every entry, so this
    /// is close to `O(1)` rather than `O(n)`. If `key` was written more
    /// than once, the smallest matching index is returned.
    #[allow(unused)]
    pub fn lookup(&self, key: &str) -> Option<usize> {
        let hash_value: u32 = Self::hash(key);
        let bucket = (hash_value >> 16) as usize;
        let hash_16 = hash_value as u16;
        let lo = u32::from_le(self.buckets[bucket]) as usize;
        let hi = u32::from_le(self.buckets[bucket + 1]) as usize;

        let mut p = lo + self.hash_values[lo..hi].partition_point(|&x| u16::from_le(x) < hash_16);
        while p < hi && u16::from_le(self.hash_values[p]) == hash_16 {
            let candidate = self.hash_index[p] as usize;
            let start = self.starts[candidate] as usize;
            let end = self.starts[candidate + 1] as usize;
            // SAFETY: Writer API only takes Rust strings, which are valid UTF-8.
            let candidate_str = unsafe { str::from_utf8_unchecked(&self.chars[start..end]) };
            if key == candidate_str {
                return Some(candidate);
            }
            p += 1;
        }

        None
    }

    /// Hashes `s` down to 32 bits, using Rust's default (SipHash) hasher.
    /// The same function is used both when writing entries ([Writer::write])
    /// and when looking them up ([StringPool::lookup]), so the two agree on
    /// which bucket and hash value a given string maps to.
    fn hash(s: &str) -> u32 {
        // We did not explore faster hashers (such as xxhash or ahash)
        // because StringPool lookup is not a bottleneck. On a 2026 MacBook
        // Air with 10 Apple M5 CPU cores, looking up every tag of every node
        // in our conflation pipleline takes 8 seconds; even if another hasher
        // was twice as fast, the difference would not be noticeable.
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish() as u32
    }
}

/// Writes a [StringPool] file, one string at a time.
///
/// Characters, string-start offsets, and hashes are appended to separate
/// temporary files as entries arrive; [Writer::close] then sorts the
/// hashes (spilling to `workdir` as needed, see [Writer::sort_hashes]) and
/// concatenates everything behind a fixed-size header, finally renaming
/// the result atomically into place. Splitting the files this way avoids
/// seeking back and forth in the output file, since offsets such as where
/// the hash-derived arrays begin are not known until all entries have
/// been written.
struct Writer {
    path: PathBuf,
    tmp_path: PathBuf,
    workdir: PathBuf,
    entry_count: usize,

    writer: BufWriter<File>,

    chars_path: PathBuf,
    chars_writer: BufWriter<File>,

    starts_path: PathBuf,
    starts_writer: BufWriter<File>,

    hashes_path: PathBuf,
    hashes_writer: BufWriter<File>,
}

impl Writer {
    /// Creates the temporary files that back a `Writer`. `path` is the
    /// final destination of the `StringPool`; `workdir` is where
    /// [Writer::sort_hashes] may spill intermediate external-sort data.
    ///
    /// The output itself is built up in a `path`-with-`.tmp`-suffix file
    /// in the same directory as `path`, and only [Writer::close] renames
    /// it into place atomically; until then, `path` is untouched. This
    /// makes `path` a checkpoint: if the process crashes mid-write, only
    /// the `.tmp` file is left behind, and restarting the pipeline finds
    /// `path` absent (or complete) and redoes the work cleanly, rather
    /// than picking up a half-written file.
    pub fn create(workdir: &Path, path: &Path) -> Result<Writer> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let mut writer = BufWriter::with_capacity(32 * 1024, File::create(&tmp_path)?);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut chars_path = PathBuf::from(path);
        chars_path.add_extension("chars.tmp");
        let chars_writer = BufWriter::with_capacity(32 * 1024, File::create(&chars_path)?);

        let mut starts_path = PathBuf::from(path);
        starts_path.add_extension("starts.tmp");
        let starts_writer = BufWriter::with_capacity(32 * 1024, File::create(&starts_path)?);

        let mut hashes_path = PathBuf::from(path);
        hashes_path.add_extension("hashes.tmp");
        let hashes_writer = BufWriter::with_capacity(32 * 1024, File::create(&hashes_path)?);

        Ok(Writer {
            path: PathBuf::from(path),
            tmp_path,
            workdir: PathBuf::from(workdir),
            entry_count: 0,
            writer,
            chars_path,
            chars_writer,
            starts_path,
            starts_writer,
            hashes_path,
            hashes_writer,
        })
    }

    /// Appends `s` as the next entry, at index `self.entry_count` before
    /// the call.
    pub fn write(&mut self, s: &str) -> Result<()> {
        let start: u64 = self.chars_writer.stream_position()?;
        self.starts_writer.write_all(&start.to_le_bytes())?;
        self.chars_writer.write_all(s.as_bytes())?;

        let hash_value: u32 = StringPool::hash(s);
        self.hashes_writer.write_all(&hash_value.to_le_bytes())?;

        self.entry_count += 1;
        Ok(())
    }

    /// Finishes writing: sorts the accumulated hashes into the
    /// buckets/hash_index/hash_values arrays, assembles the file (header,
    /// buckets, hash_index, hash_values, starts, chars, in that order —
    /// see the "File format" section at the top of this module), and
    /// atomically renames it into place at `self.path`.
    pub fn close(mut self) -> Result<()> {
        // Sort hashes.
        let (buckets, hash_index_path, hash_values_path) = {
            self.hashes_writer.flush()?;
            assert_eq!(
                self.hashes_writer.stream_position()?,
                self.entry_count as u64 * 4
            );
            drop(self.hashes_writer.into_inner()?);
            Self::sort_hashes(&self.workdir, &self.hashes_path)?
        };
        remove_file(&self.hashes_path)?;

        // Write sentinel to end of starts array.
        let chars_size: u64 = self.chars_writer.stream_position()?;
        self.starts_writer.write_all(&chars_size.to_le_bytes())?;

        self.writer.seek(SeekFrom::Start(HEADER_SIZE as u64))?;

        // Write buckets array into the output file.
        let (buckets_pos, buckets_size): (u64, u64) = {
            // Align to 64-byte cache line.
            Self::write_padding(&mut self.writer, 64)?;
            let pos = self.writer.stream_position()?;
            for bucket in &buckets {
                self.writer.write_all(&bucket.to_le_bytes())?;
            }
            drop(buckets);
            (pos, self.writer.stream_position()? - pos)
        };

        // Copy hash_index array into the output file.
        let (hash_index_pos, hash_index_size): (u64, u64) = {
            Self::write_padding(&mut self.writer, 8)?;
            let pos = self.writer.stream_position()?;
            std::io::copy(&mut File::open(&hash_index_path)?, &mut self.writer)?;
            remove_file(&hash_index_path)?;
            drop(hash_index_path);
            (pos, self.writer.stream_position()? - pos)
        };

        // Copy hash_values array into the output file.
        let (hash_values_pos, hash_values_size): (u64, u64) = {
            Self::write_padding(&mut self.writer, 4)?;
            let pos = self.writer.stream_position()?;
            std::io::copy(&mut File::open(&hash_values_path)?, &mut self.writer)?;
            remove_file(&hash_values_path)?;
            drop(hash_values_path);
            (pos, self.writer.stream_position()? - pos)
        };

        // Copy starts array into the output file.
        let (starts_pos, starts_size): (u64, u64) = {
            Self::write_padding(&mut self.writer, 8)?;
            let pos = self.writer.stream_position()?;
            drop(self.starts_writer.into_inner()?);
            std::io::copy(&mut File::open(&self.starts_path)?, &mut self.writer)?;
            remove_file(&self.starts_path)?;
            (pos, self.writer.stream_position()? - pos)
        };

        // Copy characters into the output file.
        let chars_pos: u64 = {
            let pos = self.writer.stream_position()?;
            drop(self.chars_writer.into_inner()?);
            std::io::copy(&mut File::open(&self.chars_path)?, &mut self.writer)?;
            remove_file(&self.chars_path)?;
            pos
        };

        // Write file header.
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(FILE_SIGNATURE)?; // header[0] = magic
        self.writer.write_all(&self.entry_count.to_le_bytes())?; // header[1] = len
        self.writer.write_all(&buckets_pos.to_le_bytes())?; // header[2] = buckets.pos
        self.writer.write_all(&buckets_size.to_le_bytes())?; // header[3] = buckets.size
        self.writer.write_all(&hash_index_pos.to_le_bytes())?; // header[4] = hash_index.pos
        self.writer.write_all(&hash_index_size.to_le_bytes())?; // header[5] = hash_index.size
        self.writer.write_all(&hash_values_pos.to_le_bytes())?; // header[6] = hash_values.pos
        self.writer.write_all(&hash_values_size.to_le_bytes())?; // header[7] = hash_values.size
        self.writer.write_all(&starts_pos.to_le_bytes())?; // header[8] = starts.pos
        self.writer.write_all(&starts_size.to_le_bytes())?; // header[9] = starts.size
        self.writer.write_all(&chars_pos.to_le_bytes())?; // header[10] = chars.pos
        self.writer.write_all(&chars_size.to_le_bytes())?; // header[11] = chars.size
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        self.writer.into_inner()?.sync_all()?;
        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }

    /// Reads the 32-bit hashes written to `path` (one per entry, in
    /// original insertion order, via [HashFileIter]), externally sorts
    /// them by `(hash value, original index)`, and writes the result out
    /// as two new temporary files: the hash_index array and the
    /// hash_values array (see the "File format" section at the top of
    /// this module). Also returns the buckets array, computed as the
    /// sort progresses from the upper 16 bits of each hash.
    fn sort_hashes(workdir: &Path, path: &Path) -> Result<(Buckets, PathBuf, PathBuf)> {
        let mut buckets = vec![0; BUCKET_COUNT + 1]; // last is sentinel
        let index_path = {
            let mut p = PathBuf::from(path);
            p.add_extension("index.tmp");
            p
        };
        let hash_values_path = {
            let mut p = PathBuf::from(path);
            p.add_extension("sorted.tmp");
            p
        };
        let mut index_writer = BufWriter::with_capacity(32 * 1024, File::create(&index_path)?);
        let mut hash_values_writer =
            BufWriter::with_capacity(32 * 1024, File::create(&hash_values_path)?);

        let sorter: ExternalSorter<(u32, usize), std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        let sorted = sorter.sort(HashFileIter::create(path)?)?;

        let mut last_hash_value: u32 = 0;
        let mut last_bucket: usize = 0;
        let mut item_count: u32 = 0;
        for item in sorted {
            let (hash_value, index) = item?;

            if hash_value < last_hash_value {
                anyhow::bail!(
                    "hash_values not sorted: {} < {}",
                    hash_value,
                    last_hash_value
                );
            }
            last_hash_value = hash_value;

            let index = {
                if index <= u32::MAX as usize {
                    index as u32
                } else {
                    anyhow::bail!("StringPool cannot have more than 2^32 entries");
                }
            };

            let bucket = ((hash_value >> 16) & 0xffff) as usize;
            if bucket < last_bucket {
                anyhow::bail!(
                    "StringPool buckets not sorted: {} < {}",
                    bucket,
                    last_bucket
                );
            }

            if bucket != last_bucket {
                buckets[(last_bucket + 1)..=bucket].fill(item_count);
                last_bucket = bucket;
            }
            let lower_16_bits = (hash_value & 0xffff) as u16;
            hash_values_writer.write_all(&lower_16_bits.to_le_bytes())?;
            index_writer.write_all(&index.to_le_bytes())?;

            item_count += 1;
        }
        buckets[(last_bucket + 1)..=BUCKET_COUNT].fill(item_count);

        index_writer.flush()?;
        index_writer.into_inner()?.sync_all()?;

        hash_values_writer.flush()?;
        hash_values_writer.into_inner()?.sync_all()?;
        Ok((buckets, index_path, hash_values_path))
    }

    /// Writes zero bytes to `writer` until its position is a multiple of
    /// `alignment`, so that the array written next can be reinterpreted
    /// as a slice of its element type directly on the mmap'd bytes.
    fn write_padding(writer: &mut BufWriter<File>, alignment: usize) -> Result<()> {
        if alignment > 1 {
            let pos = writer.stream_position()?;
            let alignment = alignment as u64;
            let num_bytes = ((alignment - (pos % alignment)) % alignment) as usize;
            if num_bytes > 0 {
                let padding = vec![0; num_bytes];
                writer.write_all(&padding)?;
            }
        }
        Ok(())
    }
}

/// Reads back the hashes written by [Writer::write] to the file at
/// `hashes_path`, pairing each with its original insertion index, for
/// consumption by the external sorter in [Writer::sort_hashes].
pub struct HashFileIter {
    reader: BufReader<File>,
    count: usize,
}

impl HashFileIter {
    pub fn create(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("failed to open file: {}", path.display()))?;
        let reader = BufReader::new(file);
        Ok(Self { reader, count: 0 })
    }
}

impl Iterator for HashFileIter {
    type Item = io::Result<(u32, usize)>;

    fn next(&mut self) -> Option<Self::Item> {
        use std::result::Result::Ok;
        let mut buf = [0u8; 4];
        match self.reader.read_exact(&mut buf) {
            Ok(()) => {
                let hash_value = u32::from_le_bytes(buf);
                let index = self.count;
                self.count += 1;
                Some(Ok((hash_value, index)))
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
            Err(e) => Some(Err(e)),
        }
    }
}

/// Returns whether `slice`, interpreted as little-endian `u32` values, is
/// sorted in non-decreasing order. Used to validate the buckets array
/// when opening a `StringPool` file (see [StringPool::open]).
fn is_u32_slice_sorted_little_endian(slice: &[u32]) -> bool {
    slice.windows(2).all(|window| {
        let a = u32::from_le(window[0]);
        let b = u32::from_le(window[1]);
        a <= b
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tempfile::TempDir;

    const TEST_POOL: LazyLock<StringPool> = LazyLock::new(|| {
        let entries = &["zero", "one", "two", "hello world"];
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("test.StringPool");
        StringPool::create(
            entries.into_iter().map(|&s| String::from(s)),
            &workdir.path(),
            &path,
        )
        .expect("StringPool::create() failed")
    });

    #[test]
    fn test_get() {
        assert_eq!(TEST_POOL.get(0), "zero");
        assert_eq!(TEST_POOL.get(1), "one");
        assert_eq!(TEST_POOL.get(2), "two");
        assert_eq!(TEST_POOL.get(3), "hello world");
    }

    #[test]
    fn test_len() {
        assert_eq!(TEST_POOL.len(), 4);
    }

    #[test]
    fn test_lookup() {
        assert_eq!(TEST_POOL.lookup(""), None);
        assert_eq!(TEST_POOL.lookup("not in table"), None);
        assert_eq!(TEST_POOL.lookup("zero"), Some(0));
        assert_eq!(TEST_POOL.lookup("one"), Some(1));
        assert_eq!(TEST_POOL.lookup("two"), Some(2));
        assert_eq!(TEST_POOL.lookup("hello world"), Some(3));
    }

    fn make_pool(entries: &[&str]) -> StringPool<'static> {
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("test.StringPool");
        StringPool::create(
            entries.iter().map(|&s| String::from(s)),
            workdir.path(),
            &path,
        )
        .expect("StringPool::create() failed")
    }

    #[test]
    fn test_empty_pool() {
        let pool = make_pool(&[]);
        assert_eq!(pool.len(), 0);
        assert_eq!(pool.lookup(""), None);
        assert_eq!(pool.lookup("anything"), None);
    }

    #[test]
    fn test_empty_string_entry() {
        let pool = make_pool(&["", "non-empty", ""]);
        assert_eq!(pool.get(0), "");
        assert_eq!(pool.get(1), "non-empty");
        assert_eq!(pool.get(2), "");
        // Two entries are "", at indices 0 and 2; lookup must return the
        // smaller one.
        assert_eq!(pool.lookup(""), Some(0));
        assert_eq!(pool.lookup("non-empty"), Some(1));
    }

    #[test]
    fn test_duplicate_strings_lookup_returns_smallest_index() {
        let pool = make_pool(&["a", "b", "a", "c", "a", "b"]);
        assert_eq!(pool.len(), 6);
        assert_eq!(pool.lookup("a"), Some(0));
        assert_eq!(pool.lookup("b"), Some(1));
        assert_eq!(pool.lookup("c"), Some(3));
        for (i, s) in ["a", "b", "a", "c", "a", "b"].iter().enumerate() {
            assert_eq!(pool.get(i), *s);
        }
    }

    #[test]
    fn test_unicode_strings() {
        let entries = &["café", "北京", "😀🎉", "Straße"];
        let pool = make_pool(entries);
        for (i, s) in entries.iter().enumerate() {
            assert_eq!(pool.get(i), *s);
            assert_eq!(pool.lookup(s), Some(i));
        }
    }

    #[test]
    fn test_many_strings_exercise_hash_buckets() {
        // With enough entries spread across BUCKET_COUNT (65536) buckets,
        // some buckets are very likely to end up with more than one
        // entry, exercising the binary-search-then-scan path in
        // `lookup` and the multi-entry bucket ranges in `buckets`,
        // which the small TEST_POOL above (4 entries) almost never hits.
        let entries: Vec<String> = (0..5000).map(|i| format!("string-{i}")).collect();
        let pool = make_pool(&entries.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(pool.len(), 5000);
        for (i, s) in entries.iter().enumerate() {
            assert_eq!(pool.get(i), s.as_str());
            assert_eq!(pool.lookup(s), Some(i));
        }
        assert_eq!(pool.lookup("string-5000"), None);
    }

    #[test]
    fn test_open_rejects_file_without_signature() {
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("not-a-pool");
        std::fs::write(&path, [0u8; HEADER_SIZE]).expect("write failed");
        assert!(StringPool::open(&path).is_err());
    }

    #[test]
    fn test_open_rejects_truncated_file() {
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("too-short");
        std::fs::write(&path, FILE_SIGNATURE).expect("write failed");
        assert!(StringPool::open(&path).is_err());
    }
}
