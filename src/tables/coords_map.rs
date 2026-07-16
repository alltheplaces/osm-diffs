//! Disk-based, potentially very large map with `u64` keys and [geo::Coord] values.

use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use geo::Coord;
use memmap2::Mmap;
use std::{
    fs::{File, rename},
    io::{BufWriter, Seek, SeekFrom, Write},
    mem::size_of,
    path::{Path, PathBuf},
    time::SystemTime,
};

const HEADER_SIZE: usize = 8 * 8;
const FILE_SIGNATURE: &[u8; 8] = b"coords_0";

pub struct CoordsMap<'a> {
    file: File,
    mmap: Mmap,
    entries_count: usize,
    keys: &'a [u64],
    coords: &'a [u64],
}

impl CoordsMap<'_> {
    pub fn create<'a>(
        entries: impl Iterator<Item = (u64, Coord)>,
        workdir: &Path,
        out: &'a Path,
    ) -> Result<CoordsMap<'a>> {
        let mut writer = Writer::create(out)?;
        let sorter: ExternalSorter<(u64, Coord), std::io::Error, LimitedBufferBuilder> =
            ExternalSorterBuilder::new()
                .with_tmp_dir(workdir)
                .with_buffer(LimitedBufferBuilder::new(
                    16 * 1024 * 1024,
                    /* preallocate */ true,
                ))
                .build()?;
        Self::open(out)
    }

    pub fn open(path: &Path) -> Result<CoordsMap<'_>> {
        let file = File::open(path)?;

        // SAFETY: We don’t modify the file while it is mapped into memory.
        let mmap = unsafe { Mmap::map(&file)? };
        if mmap.len() < HEADER_SIZE || &mmap[0..8] != FILE_SIGNATURE {
            anyhow::bail!("not a CoordsMap: {}", path.display());
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
                anyhow::bail!("misaligned keys in CoordsMap: {}", path.display());
            }
        };

        let coords = {
            let coords_count = entries_count;
            let coords_offset = usize::try_from(header[3])?;
            if coords_offset.is_multiple_of(8) && coords_offset + coords_count * 8 <= mmap.len() {
                // SAFETY: Verified alignment and length.
                unsafe {
                    let ptr = mmap.as_ptr().add(coords_offset) as *const u64;
                    std::slice::from_raw_parts(ptr, coords_count)
                }
            } else {
                anyhow::bail!("misaligned coords in CoordsMap: {}", path.display());
            }
        };

        Ok(CoordsMap {
            file,
            mmap,
            entries_count,
            keys,
            coords,
        })
    }

    pub fn get(&self, key: u64) -> Option<Coord> {
        let idx = self.keys.partition_point(|&k| u64::from_le(k) < key);
        if idx < self.keys.len() && self.keys[idx] == key {
            let val = u64::from_le(self.coords[idx]);
            Some(Coord {
                x: (((val >> 32) as i32) as f64) * 1e-7,
                y: ((val as i32) as f64) * 1e-7,
            })
        } else {
            None
        }
    }

    pub fn len(&self) -> usize {
        self.entries_count
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
    coords_path: PathBuf,
    coords_writer: BufWriter<File>,
    coords_count: u64,
    last_key: u64,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Writer> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let mut writer = BufWriter::with_capacity(32 * 1024, File::create(&tmp_path)?);
        writer.write_all(&[0_u8; HEADER_SIZE])?;

        let mut coords_path = PathBuf::from(path);
        coords_path.add_extension(".coords.tmp");
        let coords_writer = BufWriter::with_capacity(32 * 1024, File::create(&coords_path)?);

        Ok(Writer {
            path: PathBuf::from(path),
            tmp_path,
            writer,
            coords_path,
            coords_writer,
            coords_count: 0,
            last_key: 0,
        })
    }

    pub fn write(&mut self, key: u64, coord: Coord) -> Result<()> {
        if key <= self.last_key {
            anyhow::bail!(
                "keys must be written in ascending order, but {} <= {}",
                key,
                self.last_key,
            );
        }

        self.writer.write_all(&key.to_le_bytes())?;
        self.last_key = key;

        let x_i32 = (coord.x * 1e7) as i32;
        let y_i32 = (coord.y * 1e7) as i32;
        let encoded = (x_i32 as u64) << 32 | ((y_i32 as u32) as u64);
        self.coords_writer.write_all(&encoded.to_le_bytes())?;
        self.coords_count += 1;

        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        let keys_offset = HEADER_SIZE as u64;
        let coords_offset = keys_offset + self.coords_count * 8;
        assert_eq!(self.writer.stream_position()?, coords_offset);

        // Write file header.
        self.writer.seek(SeekFrom::Start(0))?;
        self.writer.write_all(FILE_SIGNATURE);
        self.writer.write_all(&self.coords_count.to_le_bytes())?;
        self.writer.write_all(&keys_offset.to_le_bytes())?;
        self.writer.write_all(&coords_offset.to_le_bytes())?;
        assert!(self.writer.stream_position()? <= HEADER_SIZE as u64);

        // Copy coordinates from coords file into the output file.
        self.writer.seek(SeekFrom::Start(coords_offset))?;
        self.coords_writer.flush()?; // flush() returns errors
        drop(self.coords_writer); // drop() does not return errors
        std::io::copy(&mut File::open(&self.coords_path)?, &mut self.writer)?;

        self.writer.flush()?; // flush() returns errors
        drop(self.writer); // drop() does not return errors

        rename(&self.tmp_path, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn almost_equal(a: Coord, b: Coord) -> bool {
        const EPSILON: f64 = 1e-10;
        (a.x - b.x).abs() < EPSILON && (a.y - b.y).abs() < EPSILON
    }

    #[test]
    fn test_coords_table() -> Result<()> {
        // Test coordinates in every quadrant.
        const OTTAWA: Coord = Coord {
            x: -75.69812,
            y: 45.41117,
        };
        const BERN: Coord = Coord {
            x: 7.44744,
            y: 46.94809,
        };
        const USHUAIA: Coord = Coord {
            x: -68.31591,
            y: -54.81084,
        };
        const MELBOURNE: Coord = Coord {
            x: 144.96332,
            y: -37.814,
        };
        let file = NamedTempFile::new()?;
        let mut writer = Writer::create(&file.path())?;
        writer.write(17, BERN)?;
        writer.write(41, OTTAWA)?;
        writer.write(42, BERN)?;
        writer.write(43, USHUAIA)?;
        writer.write(44, MELBOURNE)?;
        writer.close()?;
        let file_metadata = std::fs::metadata(file.path())?;

        let table = CoordsMap::open(&file.path())?;
        assert_eq!(table.modified()?, file_metadata.modified()?);
        assert_eq!(table.len(), 5);

        assert_eq!(table.get(0), None);
        assert_eq!(table.get(16), None);
        assert!(almost_equal(table.get(17).unwrap(), BERN));
        assert_eq!(table.get(18), None);
        assert_eq!(table.get(23), None);
        assert!(almost_equal(table.get(41).unwrap(), OTTAWA));
        assert!(almost_equal(table.get(42).unwrap(), BERN));
        assert!(almost_equal(table.get(43).unwrap(), USHUAIA));
        assert!(almost_equal(table.get(44).unwrap(), MELBOURNE));
        assert_eq!(table.get(99), None);

        Ok(())
    }
}
