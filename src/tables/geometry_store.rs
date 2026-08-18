//! In-memory-indexed, disk-spooled map with `u64` keys and [geo::Geometry]
//! values, mutable at any time.
//!
//! `GeometryStore` is used while assembling the geometry of OSM
//! super-relations: a relation may itself have relations as members, so
//! its geometry can only be built once the geometries of its member
//! relations are known. That means we need a map that can be written to
//! incrementally, as more super-relations get resolved, and read from at
//! any point in between -- unlike [GeometryTable], whose file is built
//! once, in full, and is read-only from then on.
//!
//! As of July 2026, a full run over the OpenStreetMap planet dump produces
//! only about 19,000 super-relations, so keeping a `(u64, u64)` offset and
//! length per entry in memory is cheap. But the geometries themselves can
//! get quite large for super-relations (some contain thousands of member
//! ways), so we still don't want to keep them all in RAM: each geometry is
//! WKB-encoded and spooled to a plain file, and only its `(offset, length)`
//! is kept in an in-memory `HashMap`.
//!
//! # Differences to [GeometryTable]
//!
//! - **Modifiable:** `GeometryTable` is built once with `create()` and is
//!   read-only afterwards. `GeometryStore` supports [GeometryStore::insert]
//!   at any time, interleaved with [GeometryStore::lookup] calls.
//! - **Not memory-mapped:** `GeometryTable` memory-maps its file, so
//!   lookups are zero-copy. `GeometryStore`'s index lives in a `HashMap` in
//!   RAM, and [GeometryStore::lookup] performs a `seek` and a `read` on the
//!   spool file for every call.
//! - **Not compressed:** Like `GeometryTable`, the WKB blobs are written
//!   uncompressed; `GeometryStore` does not add compression either, since
//!   its geometries are read back only a handful of times during
//!   conflation, and being modifiable already rules out approaches like
//!   compressing the whole file as one block.
//! - **Internally synchronized file access:** A `GeometryTable` is
//!   read-only once built, so its memory-mapped bytes can safely be read
//!   from multiple threads at once. `GeometryStore` instead performs a
//!   `seek` followed by a `read` (or `write`) on a plain file for every
//!   call, and those two steps are not atomic on their own; interleaving
//!   them across threads would let one thread's `seek` clobber another's
//!   before its `read` runs. To make [GeometryStore::lookup] safe to call
//!   as `&self` from multiple threads, the file handle is wrapped in a
//!   [std::sync::Mutex], so each `seek`+`read`/`write` sequence runs to
//!   completion while holding the lock. [GeometryStore::insert] still
//!   requires `&mut self`, per normal Rust aliasing rules, so mutating a
//!   `GeometryStore` concurrently with other `insert`s or `lookup`s still
//!   needs the caller to synchronize at a higher level (e.g. by wrapping
//!   the whole store in a `Mutex` or `RwLock`), same as for any other type
//!   with `&mut self` methods.
//!
//! The spool file itself has no header and no on-disk index: it is simply
//! the WKB encoding of each inserted geometry, one after another, in
//! insertion order. Overwriting a key with [GeometryStore::insert] appends
//! the new encoding at the end of the file and leaves the previous bytes
//! in place as dead space, since the file is append-only.

use anyhow::Result;
use geo::Geometry;
use geo_traits::to_geo::ToGeoGeometry;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};
use wkb::{
    Endianness,
    reader::read_wkb,
    writer::{WriteOptions, write_geometry},
};

const WKB_WRITE_OPTIONS: WriteOptions = WriteOptions {
    endianness: Endianness::LittleEndian,
};

/// Offset and length, in bytes, of a WKB blob within the spool file.
#[derive(Clone, Copy)]
struct Location {
    offset: u64,
    len: u64,
}

pub struct GeometryStore {
    path: PathBuf,
    file: Mutex<File>,
    index: HashMap<u64, Location>,
}

impl GeometryStore {
    /// Creates a new, empty `GeometryStore` whose spool file is `path`.
    ///
    /// If a file already exists at `path`, it is truncated.
    pub fn create(path: &Path) -> Result<GeometryStore> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(GeometryStore {
            path: path.to_path_buf(),
            file: Mutex::new(file),
            index: HashMap::new(),
        })
    }

    /// Inserts `geometry` under `key`, appending its WKB encoding to the
    /// spool file.
    ///
    /// If `key` was already present, [GeometryStore::lookup] returns the
    /// new geometry from now on; the bytes of the previous geometry stay
    /// in the spool file as dead space.
    pub fn insert(&mut self, key: u64, geometry: &Geometry) -> Result<()> {
        let wkb = encode_wkb(geometry);
        let offset = {
            let mut file = self.file.lock().expect("geometry store file lock poisoned");
            let offset = file.seek(SeekFrom::End(0))?;
            file.write_all(&wkb)?;
            offset
        };
        self.index.insert(
            key,
            Location {
                offset,
                len: wkb.len() as u64,
            },
        );
        Ok(())
    }

    /// Returns the geometry stored under `key`, or `None` if `key` was
    /// never inserted.
    ///
    /// Safe to call as `&self` from multiple threads at once: the
    /// underlying `seek`+`read` sequence is serialized behind an internal
    /// lock (see the module documentation).
    pub fn lookup(&self, key: u64) -> Result<Option<Geometry>> {
        let Some(&Location { offset, len }) = self.index.get(&key) else {
            return Ok(None);
        };

        let mut buf = vec![0_u8; len as usize];
        {
            let mut file = self.file.lock().expect("geometry store file lock poisoned");
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(&mut buf)?;
        }

        let err = format!("invalid WKB for key {} in {}", key, self.path.display());
        Ok(Some(read_wkb(&buf).expect(&err).to_geometry()))
    }
}

fn encode_wkb(geometry: &Geometry) -> Vec<u8> {
    let mut buf = Vec::new();
    write_geometry(&mut buf, geometry, &WKB_WRITE_OPTIONS).expect("wkb encoding failed");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{LineString, Point, Polygon, point};
    use tempfile::NamedTempFile;

    #[test]
    fn test_insert_and_lookup_round_trip_geometries() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut store = GeometryStore::create(file.path())?;

        store.insert(17, &Geometry::Point(point!(x: 7.45, y: 46.95)))?;
        store.insert(
            42,
            &Geometry::LineString(LineString::from(vec![(0.0, 0.0), (1.0, 1.0)])),
        )?;

        assert_eq!(
            store.lookup(17)?,
            Some(Geometry::Point(point!(x: 7.45, y: 46.95)))
        );
        assert_eq!(
            store.lookup(42)?,
            Some(Geometry::LineString(LineString::from(vec![
                (0.0, 0.0),
                (1.0, 1.0)
            ])))
        );

        Ok(())
    }

    #[test]
    fn test_lookup_returns_none_for_missing_key() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut store = GeometryStore::create(file.path())?;
        store.insert(17, &Geometry::Point(point!(x: 1.0, y: 1.0)))?;

        assert_eq!(store.lookup(0)?, None);
        assert_eq!(store.lookup(99)?, None);

        Ok(())
    }

    #[test]
    fn test_insert_overwrites_previous_geometry_for_same_key() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut store = GeometryStore::create(file.path())?;

        store.insert(1, &Geometry::Point(point!(x: 1.0, y: 1.0)))?;
        assert_eq!(
            store.lookup(1)?,
            Some(Geometry::Point(point!(x: 1.0, y: 1.0)))
        );

        store.insert(1, &Geometry::Point(point!(x: 2.0, y: 2.0)))?;
        assert_eq!(
            store.lookup(1)?,
            Some(Geometry::Point(point!(x: 2.0, y: 2.0)))
        );

        Ok(())
    }

    #[test]
    fn test_insert_and_lookup_interleaved_with_varying_geometry_sizes() -> Result<()> {
        let file = NamedTempFile::new()?;
        let mut store = GeometryStore::create(file.path())?;

        let polygon = Polygon::new(
            LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            vec![],
        );

        store.insert(1, &Geometry::Point(Point::new(1.0, 1.0)))?;
        assert_eq!(
            store.lookup(1)?,
            Some(Geometry::Point(Point::new(1.0, 1.0)))
        );

        store.insert(2, &Geometry::Polygon(polygon.clone()))?;
        store.insert(3, &Geometry::Point(Point::new(3.0, 3.0)))?;

        assert_eq!(
            store.lookup(1)?,
            Some(Geometry::Point(Point::new(1.0, 1.0)))
        );
        assert_eq!(store.lookup(2)?, Some(Geometry::Polygon(polygon)));
        assert_eq!(
            store.lookup(3)?,
            Some(Geometry::Point(Point::new(3.0, 3.0)))
        );

        Ok(())
    }
}
