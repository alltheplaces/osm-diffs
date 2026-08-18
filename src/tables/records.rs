//! A spool file for a sequential stream of opaque, length-prefixed records.
//!
//! Unlike the other tables in [crate::tables], which are mmap'd for random
//! access, a record spool is meant to be written once, then read back
//! sequentially in full, so it uses buffered, streaming I/O throughout.
//!
//! # File format
//!
//! ```text
//! byte 0..8:   magic "record_0"
//! byte 8..16:  record count, u64 little-endian
//! byte 16..:   lz4 frame, decompressing to a stream of records, each
//!              encoded as a varint length followed by that many bytes
//! ```
//!
//! The header is written uncompressed, ahead of the lz4 frame, so that
//! [RecordReader::len] is available without decompressing the file.

use std::fs::{File, rename};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lz4_flex::frame::{FrameDecoder, FrameEncoder};

/// Magic bytes identifying a record spool file, written as the first
/// eight bytes of the file header.
const FILE_SIGNATURE: &[u8; 8] = b"record_0";

/// Size of the file header: an 8-byte magic followed by an 8-byte,
/// little-endian record count. The header is written uncompressed, ahead
/// of the lz4 frame, so that [RecordReader::len] is available without
/// decompressing the file.
const HEADER_SIZE: usize = 16;

/// A spool file: a file header, followed by an lz4-compressed stream of
/// length-prefixed records.
pub struct RecordReader {
    path: PathBuf,
    count: u64,
}

impl RecordReader {
    /// Open a spool file for reading. This reads and validates the file
    /// header; the actual decompression happens lazily, once per call to
    /// `iter()`.
    pub fn open(path: &Path) -> Result<RecordReader> {
        let mut file = File::open(path)
            .with_context(|| format!("failed to open spool file {}", path.display()))?;

        let mut header = [0u8; HEADER_SIZE];
        file.read_exact(&mut header)
            .with_context(|| format!("failed to read header of spool file {}", path.display()))?;
        if header[0..8] != *FILE_SIGNATURE {
            anyhow::bail!("not a record spool file: {}", path.display());
        }
        let count = u64::from_le_bytes(header[8..16].try_into().unwrap());

        Ok(RecordReader {
            path: path.to_path_buf(),
            count,
        })
    }

    /// The number of records in this spool file.
    pub fn len(&self) -> usize {
        self.count as usize
    }

    /// Iterate over the records in this spool file.
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<Vec<u8>>>> {
        let mut file = File::open(&self.path)
            .with_context(|| format!("failed to open spool file {}", self.path.display()))?;
        file.seek(SeekFrom::Start(HEADER_SIZE as u64))
            .with_context(|| format!("failed to seek spool file {}", self.path.display()))?;
        let decoder = FrameDecoder::new(file);
        Ok(RecordIter {
            reader: BufReader::new(decoder),
        })
    }
}

// Private: not part of the public API, just the concrete iterator type
// hidden behind `impl Iterator` in `RecordReader::iter()`.
struct RecordIter {
    reader: BufReader<FrameDecoder<File>>,
}

impl RecordIter {
    /// Reads the raw bytes of a single varint off the stream (at most 10,
    /// since a u64 varint never needs more), then decodes them with prost's
    /// `decode_varint`. Returns `Ok(None)` on clean EOF at a record boundary.
    fn read_varint(reader: &mut impl Read) -> Result<Option<u64>> {
        let mut buf = [0u8; 10];
        let mut len = 0usize;
        let mut byte = [0u8; 1];

        loop {
            let n = reader.read(&mut byte)?;
            if n == 0 {
                if len == 0 {
                    // EOF right at a record boundary: normal end of stream.
                    return Ok(None);
                } else {
                    anyhow::bail!("unexpected EOF in the middle of a varint");
                }
            }

            if len >= buf.len() {
                anyhow::bail!("varint too long (more than 10 bytes)");
            }

            buf[len] = byte[0];
            len += 1;

            if byte[0] & 0x80 == 0 {
                break;
            }
        }

        let mut slice = &buf[..len];
        let value = prost::encoding::decode_varint(&mut slice)
            .context("failed to decode varint record length")?;
        Ok(Some(value))
    }
}

impl Iterator for RecordIter {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let len = match Self::read_varint(&mut self.reader) {
            Ok(Some(len)) => len,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };

        let mut buf = vec![0u8; len as usize];
        if let Err(e) = self.reader.read_exact(&mut buf) {
            return Some(Err(
                anyhow::Error::new(e).context("failed to read record body (truncated stream?)")
            ));
        }

        Some(Ok(buf))
    }
}

/// Writer for a spool file: writes a file header, followed by an
/// lz4-compressed stream of length-prefixed records.
///
/// Writes go to a temporary file beside `path`; [RecordWriter::close]
/// finishes it and atomically renames it into place, so a reader never
/// observes a partially written file.
pub struct RecordWriter {
    encoder: FrameEncoder<File>,
    count: u64,
    path: PathBuf,
    tmp_path: PathBuf,
}

impl RecordWriter {
    /// Create a new spool file for writing, truncating it if it already exists.
    pub fn create(path: &Path) -> Result<RecordWriter> {
        let mut tmp_path = path.to_path_buf();
        tmp_path.add_extension("tmp");
        let mut file = File::create(&tmp_path)
            .with_context(|| format!("failed to create spool file {}", tmp_path.display()))?;

        // Reserve space for the header; `close()` seeks back to fill in the
        // real record count once it is known.
        file.write_all(FILE_SIGNATURE)
            .context("failed to write spool file magic")?;
        file.write_all(&0u64.to_le_bytes())
            .context("failed to write placeholder record count")?;

        Ok(RecordWriter {
            encoder: FrameEncoder::new(file),
            count: 0,
            path: path.to_path_buf(),
            tmp_path,
        })
    }

    /// Write a single record: its varint-encoded length, followed by its bytes.
    pub fn write(&mut self, record: &[u8]) -> Result<()> {
        let mut len_buf = Vec::with_capacity(10);
        prost::encoding::encode_varint(record.len() as u64, &mut len_buf);

        self.encoder
            .write_all(&len_buf)
            .context("failed to write record length")?;
        self.encoder
            .write_all(record)
            .context("failed to write record body")?;

        self.count += 1;
        Ok(())
    }

    /// Flush all buffered data, finish the lz4 frame, patch the file header
    /// with the final record count, and atomically rename the temporary
    /// file into place. Consumes the writer, since no more writes are
    /// possible after this.
    pub fn close(self) -> Result<()> {
        let mut file = self
            .encoder
            .finish()
            .context("failed to finish lz4 frame")?;
        file.flush().context("failed to flush underlying file")?;

        file.seek(SeekFrom::Start(8))
            .context("failed to seek back to header")?;
        file.write_all(&self.count.to_le_bytes())
            .context("failed to write final record count")?;
        file.flush().context("failed to flush underlying file")?;
        drop(file);

        rename(&self.tmp_path, &self.path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                self.tmp_path.display(),
                self.path.display()
            )
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_back_two_records() -> Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        let path = tmp.path();

        let mut writer = RecordWriter::create(path)?;
        writer.write(b"hello")?;
        writer.write(b"world, a slightly longer second record")?;
        writer.close()?;

        let spool = RecordReader::open(path)?;
        assert_eq!(spool.len(), 2);

        let records: Result<Vec<Vec<u8>>> = spool.iter()?.collect();
        let records = records?;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0], b"hello");
        assert_eq!(records[1], b"world, a slightly longer second record");

        Ok(())
    }

    #[test]
    fn empty_spool_file() -> Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        let path = tmp.path();

        let writer = RecordWriter::create(path)?;
        writer.close()?;

        let spool = RecordReader::open(path)?;
        assert_eq!(spool.len(), 0);
        assert_eq!(spool.iter()?.count(), 0);

        Ok(())
    }

    #[test]
    fn open_rejects_files_with_wrong_magic() -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(b"not_a_record_spool_file")?;

        assert!(RecordReader::open(tmp.path()).is_err());

        Ok(())
    }

    #[test]
    fn open_rejects_truncated_header() -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new()?;
        tmp.write_all(FILE_SIGNATURE)?; // magic, but no count follows

        assert!(RecordReader::open(tmp.path()).is_err());

        Ok(())
    }
}
