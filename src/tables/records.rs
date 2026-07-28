use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lz4_flex::frame::{FrameDecoder, FrameEncoder};

/// A spool file: an lz4-compressed stream of length-prefixed records.
pub struct RecordsReader {
    path: PathBuf,
}

impl RecordsReader {
    /// Open a spool file for reading. This validates the file is accessible;
    /// the actual decompression happens lazily, once per call to `iter()`.
    pub fn open(path: &Path) -> Result<RecordsReader> {
        File::open(path)
            .with_context(|| format!("failed to open spool file {}", path.display()))?;

        Ok(RecordsReader {
            path: path.to_path_buf(),
        })
    }

    /// Iterate over the records in this spool file.
    pub fn iter(&self) -> Result<impl Iterator<Item = Result<Vec<u8>>>> {
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open spool file {}", self.path.display()))?;
        let decoder = FrameDecoder::new(file);
        Ok(RecordsIter {
            reader: BufReader::new(decoder),
        })
    }
}

// Private: not part of the public API, just the concrete iterator type
// hidden behind `impl Iterator` in `RecordsReader::iter()`.
struct RecordsIter {
    reader: BufReader<FrameDecoder<File>>,
}

impl RecordsIter {
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

impl Iterator for RecordsIter {
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

/// Writer for a spool file: writes an lz4-compressed stream of
/// length-prefixed records.
pub struct RecordsWriter {
    encoder: FrameEncoder<File>,
}

impl RecordsWriter {
    /// Create a new spool file for writing, truncating it if it already exists.
    pub fn create(path: &Path) -> Result<RecordsWriter> {
        let file = File::create(path)
            .with_context(|| format!("failed to create spool file {}", path.display()))?;
        Ok(RecordsWriter {
            encoder: FrameEncoder::new(file),
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

        Ok(())
    }

    /// Flush all buffered data and finish the lz4 frame, ensuring the file
    /// is complete and readable. Consumes the writer, since no more writes
    /// are possible after this.
    pub fn close(self) -> Result<()> {
        let mut file = self
            .encoder
            .finish()
            .context("failed to finish lz4 frame")?;
        file.flush().context("failed to flush underlying file")?;
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

        let mut writer = RecordsWriter::create(path)?;
        writer.write(b"hello")?;
        writer.write(b"world, a slightly longer second record")?;
        writer.close()?;

        let spool = RecordsReader::open(path)?;
        let records: Result<Vec<Vec<u8>>> = spool.iter()?.collect();
        let records = records?;

        assert_eq!(records.len(), 2);
        assert_eq!(records[0], b"hello");
        assert_eq!(records[1], b"world, a slightly longer second record");

        Ok(())
    }
}
