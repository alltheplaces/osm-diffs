use anyhow::{Ok, Result};
use memmap2::Mmap;
use std::{
    fs::{File, remove_file, rename},
    hash::{DefaultHasher, Hash, Hasher},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[allow(unused)]
pub struct StringPool<'a> {
    file: File,
    _mmap: Mmap,
    len: usize,
    chars: &'a [u8],
    starts: &'a [u64],
}

const HEADER_SIZE: usize = 8 * 8;
const FILE_SIGNATURE: &[u8; 8] = b"strpool0";

impl<'a> StringPool<'a> {
    pub fn create(strings: impl Iterator<Item = String>, path: &Path) -> Result<StringPool<'a>> {
        let mut writer = Writer::create(path)?;
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

        let starts = {
            let offset = usize::try_from(header[2])?;
            let size = usize::try_from(header[3])?;
            if offset + size <= mmap.len() && offset.is_multiple_of(8) {
                // SAFETY: Verified size and alignment.
                unsafe {
                    let ptr = mmap.as_ptr().add(offset) as *const u64;
                    std::slice::from_raw_parts(ptr, size)
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
            let offset = usize::try_from(header[4])?;
            let size = usize::try_from(header[5])?;
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
}

struct Writer {
    path: PathBuf,
    tmp_path: PathBuf,
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
    pub fn create(path: &Path) -> Result<Writer> {
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

        let hash_value: u64 = {
            let mut hasher = DefaultHasher::new();
            s.hash(&mut hasher);
            hasher.finish()
        };
        self.hashes_writer.write_all(&hash_value.to_le_bytes())?;

        self.entry_count += 1;
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        // TODO: Sort hashes. For now, we discard them.
        assert_eq!(
            self.hashes_writer.stream_position()?,
            self.entry_count as u64 * 8
        );
        drop(self.hashes_writer.into_inner()?);
        remove_file(&self.hashes_path)?;

        let chars_size: u64 = self.chars_writer.stream_position()?;

        self.starts_writer.write_all(&chars_size.to_le_bytes())?;
        let starts_size: u64 = self.starts_writer.stream_position()?;
        let starts_pos: u64 = HEADER_SIZE as u64;

        let chars_pos: u64 = starts_pos + starts_size;

        // Write file header.
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(FILE_SIGNATURE)?; // header[0]
        self.writer.write_all(&self.entry_count.to_le_bytes())?; // header[1]
        self.writer.write_all(&starts_pos.to_le_bytes())?; // header[2]
        self.writer.write_all(&starts_size.to_le_bytes())?; // header[3]
        self.writer.write_all(&chars_pos.to_le_bytes())?; // header[4]
        self.writer.write_all(&chars_size.to_le_bytes())?; // header[5]	
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        // Copy starts array into the output file.
        self.writer.seek(SeekFrom::Start(starts_pos))?;
        drop(self.starts_writer.into_inner()?);
        std::io::copy(&mut File::open(&self.starts_path)?, &mut self.writer)?;
        remove_file(&self.starts_path)?;

        // Copy characters into the output file.
        assert_eq!(self.writer.stream_position()?, chars_pos);
        drop(self.chars_writer.into_inner()?);
        std::io::copy(&mut File::open(&self.chars_path)?, &mut self.writer)?;
        remove_file(&self.chars_path)?;

        self.writer.into_inner()?.sync_all()?;
        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tempfile::TempDir;

    const TEST_POOL: LazyLock<StringPool> = LazyLock::new(|| {
        let entries = &["zero", "one", "two", "three"];
        let workdir = TempDir::new().expect("TempDir::new() failed");
        let path = workdir.path().join("test.StringPool");
        StringPool::create(entries.into_iter().map(|&s| String::from(s)), &path)
            .expect("StringPool::create() failed")
    });

    #[test]
    fn test_get() {
        assert_eq!(TEST_POOL.get(0), "zero");
        assert_eq!(TEST_POOL.get(1), "one");
        assert_eq!(TEST_POOL.get(2), "two");
        assert_eq!(TEST_POOL.get(3), "three");
    }

    #[test]
    fn test_len() {
        assert_eq!(TEST_POOL.len(), 4);
    }
}
