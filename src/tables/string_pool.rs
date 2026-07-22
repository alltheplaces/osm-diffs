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

#[allow(unused)]
pub struct StringPool<'a> {
    file: File,
    _mmap: Mmap,
    len: usize,
    buckets: &'a [u32],
    hash_index: &'a [u32],
    hash_values: &'a [u16],
    chars: &'a [u8],
    starts: &'a [u64],
}

const HEADER_SIZE: usize = 16 * 8;
const FILE_SIGNATURE: &[u8; 8] = b"strpool0";
const BUCKET_COUNT: usize = 65536;

type Buckets = Vec<u32>;

impl<'a> StringPool<'a> {
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
            file,
            _mmap: mmap,
            len,
            buckets,
            hash_index,
            hash_values,
            chars,
            starts,
        })
    }

    #[allow(unused)]
    pub fn get(&self, idx: usize) -> &'a str {
        let start = u64::from_le(self.starts[idx]) as usize;
        let end = u64::from_le(self.starts[idx + 1]) as usize;
        // SAFETY: Writer API only accepts Rust strings, which are valid UTF-8.
        unsafe { str::from_utf8_unchecked(&self.chars[start..end]) }
    }

    #[allow(unused)]
    pub fn len(&self) -> usize {
        self.len
    }

    fn hash(s: &str) -> u32 {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish() as u32
    }
}

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

    pub fn write(&mut self, s: &str) -> Result<()> {
        let start: u64 = self.chars_writer.stream_position()?;
        self.starts_writer.write_all(&start.to_le_bytes())?;
        self.chars_writer.write_all(s.as_bytes())?;

        let hash_value: u32 = StringPool::hash(s);
        self.hashes_writer.write_all(&hash_value.to_le_bytes())?;

        self.entry_count += 1;
        Ok(())
    }

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
}
