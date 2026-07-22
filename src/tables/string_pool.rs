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
    hash_index: &'a [u64],
    hash_values: &'a [u32],
    chars: &'a [u8],
    starts: &'a [u64],
}

const HEADER_SIZE: usize = 10 * 8;
const FILE_SIGNATURE: &[u8; 8] = b"strpool0";

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

        let hash_index = {
            let offset = usize::try_from(header[2])?;
            let size = usize::try_from(header[3])?;
            if offset + size <= mmap.len() && offset.is_multiple_of(8) && size.is_multiple_of(8) {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u64;
                    std::slice::from_raw_parts(ptr, size / 8)
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
                    "misplaced hash_values in {}: mmap.len={}, offset={}, size={}",
                    path.display(),
                    mmap.len(),
                    offset,
                    size
                );
            }
        };

        let starts = {
            let offset = usize::try_from(header[6])?;
            let size = usize::try_from(header[7])?;
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
            let offset = usize::try_from(header[8])?;
            let size = usize::try_from(header[9])?;
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
        let (hash_index_path, hash_values_path) = {
            self.hashes_writer.flush()?;
            assert_eq!(
                self.hashes_writer.stream_position()?,
                self.entry_count as u64 * 4
            );
            drop(self.hashes_writer.into_inner()?);
            Self::sort_hashes(&self.workdir, &self.hashes_path)?
        };
        remove_file(&self.hashes_path)?;
        let hash_index_size = std::fs::metadata(&hash_index_path)?.len();
        let hash_values_size = std::fs::metadata(&hash_values_path)?.len();

        let chars_size: u64 = self.chars_writer.stream_position()?;
        self.starts_writer.write_all(&chars_size.to_le_bytes())?;
        let starts_size: u64 = self.starts_writer.stream_position()?;

        self.writer.seek(SeekFrom::Start(HEADER_SIZE as u64))?;

        // Copy hash_index array into the output file.
        let hash_index_pos: u64 = {
            Self::write_padding(&mut self.writer, 8)?;
            let pos = self.writer.stream_position()?;
            std::io::copy(&mut File::open(&hash_index_path)?, &mut self.writer)?;
            remove_file(&hash_index_path)?;
            drop(hash_index_path);
            pos
        };

        // Copy hash_values array into the output file.
        let hash_values_pos: u64 = {
            Self::write_padding(&mut self.writer, 4)?;
            let pos = self.writer.stream_position()?;
            std::io::copy(&mut File::open(&hash_values_path)?, &mut self.writer)?;
            remove_file(&hash_values_path)?;
            drop(hash_values_path);
            pos
        };

        // Copy starts array into the output file.
        let starts_pos: u64 = {
            Self::write_padding(&mut self.writer, 8)?;
            let pos = self.writer.stream_position()?;
            drop(self.starts_writer.into_inner()?);
            std::io::copy(&mut File::open(&self.starts_path)?, &mut self.writer)?;
            remove_file(&self.starts_path)?;
            pos
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
        self.writer.write_all(&hash_index_pos.to_le_bytes())?; // header[2] = hash_index.pos
        self.writer.write_all(&hash_index_size.to_le_bytes())?; // header[3] = hash_index.len
        self.writer.write_all(&hash_values_pos.to_le_bytes())?; // header[4] = hash_values.pos
        self.writer.write_all(&hash_values_size.to_le_bytes())?; // header[5] = hash_values.len
        self.writer.write_all(&starts_pos.to_le_bytes())?; // header[6] = starts.pos
        self.writer.write_all(&starts_size.to_le_bytes())?; // header[7] = starts.len
        self.writer.write_all(&chars_pos.to_le_bytes())?; // header[8] = chars.pos
        self.writer.write_all(&chars_size.to_le_bytes())?; // header[9]	= chars.len
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        self.writer.into_inner()?.sync_all()?;
        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }

    fn sort_hashes(workdir: &Path, path: &Path) -> Result<(PathBuf, PathBuf)> {
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
                    4 * 1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        let sorted = sorter.sort(HashFileIter::create(path)?)?;
        for item in sorted {
            let (hash_value, index) = item?;
            hash_values_writer.write_all(&hash_value.to_le_bytes())?;
            index_writer.write_all(&(index as u64).to_le_bytes())?;
        }
        index_writer.flush()?;
        index_writer.into_inner()?.sync_all()?;

        hash_values_writer.flush()?;
        hash_values_writer.into_inner()?.sync_all()?;

        Ok((index_path, hash_values_path))
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
