//! Disk-based, potentially very large map with `u64` keys and [geo::Geometry] values.
//!
//! `GeometryTable` wraps [BlobTable], adding geometry-specific encoding on
//! top of it: values passed to [GeometryTable::create] and returned by
//! [GeometryTable::lookup] are [geo::Geometry] values, not raw bytes.
//! Internally, each geometry is stored as a WKB
//! (Well-Known Binary) blob in little-endian encoding, so on disk a
//! `GeometryTable` is indistinguishable from a `BlobTable` of WKB blobs.
//!
//! Aside from its value type, the public API of `GeometryTable` mirrors
//! that of `BlobTable`.

use crate::tables::BlobTable;
use anyhow::Result;
use geo::Geometry;
use geo_traits::to_geo::ToGeoGeometry;
use std::{path::Path, time::SystemTime};
use wkb::{
    Endianness,
    reader::read_wkb,
    writer::{WriteOptions, write_geometry},
};

const WKB_WRITE_OPTIONS: WriteOptions = WriteOptions {
    endianness: Endianness::LittleEndian,
};

pub struct GeometryTable<'a> {
    blobs: BlobTable<'a>,
}

impl<'a> GeometryTable<'a> {
    /// Builds a `GeometryTable` from `geometries`, which may be in any order.
    ///
    /// Each geometry is encoded as a WKB blob and handed to the embedded
    /// [BlobTable], so the same ordering and external-sorting behavior as
    /// [BlobTable::create] applies -- including `chunk_bytes`, forwarded
    /// straight through; see [BlobTable::create] for what it means.
    pub fn create(
        geometries: impl Iterator<Item = (u64, Geometry)>,
        workdir: &Path,
        out: &Path,
        chunk_bytes: usize,
    ) -> Result<GeometryTable<'a>> {
        let blobs = BlobTable::create(
            geometries.map(|(key, geometry)| (key, encode_wkb(&geometry))),
            workdir,
            out,
            chunk_bytes,
        )?;
        Ok(GeometryTable { blobs })
    }

    pub fn open(path: &Path) -> Result<GeometryTable<'a>> {
        Ok(GeometryTable {
            blobs: BlobTable::open(path)?,
        })
    }

    pub fn lookup(&self, key: u64) -> Option<Geometry> {
        self.blobs.lookup(key).map(|wkb| decode_wkb(key, wkb))
    }

    pub fn len(&self) -> usize {
        self.blobs.len()
    }

    /// Returns the modification time of the backing file.
    ///
    /// Currently unused -- part of the memoization surface every table in
    /// this module exposes, but no pipeline stage's staleness check reads
    /// it yet. See https://github.com/alltheplaces/osm-diffs/issues/704.
    #[allow(unused)]
    pub fn modified(&self) -> Result<SystemTime> {
        self.blobs.modified()
    }
}

fn encode_wkb(geometry: &Geometry) -> Vec<u8> {
    let mut buf = Vec::new();
    write_geometry(&mut buf, geometry, &WKB_WRITE_OPTIONS).expect("wkb encoding failed");
    buf
}

fn decode_wkb(key: u64, wkb: &[u8]) -> Geometry {
    read_wkb(wkb)
        .unwrap_or_else(|_| panic!("invalid WKB for key {}", key))
        .to_geometry()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{LineString, Polygon, point};
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn test_create_sorts_unsorted_input_and_round_trips_geometries() -> Result<()> {
        let workdir = TempDir::new()?;
        let file = NamedTempFile::new()?;
        let geometries: Vec<(u64, Geometry)> = vec![
            (44, Geometry::Point(point!(x: 6.63, y: 46.52))),
            (17, Geometry::Point(point!(x: 7.45, y: 46.95))),
            (
                42,
                Geometry::LineString(LineString::from(vec![(0.0, 0.0), (1.0, 1.0)])),
            ),
        ];

        let table = GeometryTable::create(
            geometries.into_iter(),
            workdir.path(),
            file.path(),
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;
        assert_eq!(table.len(), 3);
        assert_eq!(table.lookup(0), None);
        assert_eq!(
            table.lookup(17),
            Some(Geometry::Point(point!(x: 7.45, y: 46.95)))
        );
        assert_eq!(
            table.lookup(42),
            Some(Geometry::LineString(LineString::from(vec![
                (0.0, 0.0),
                (1.0, 1.0)
            ])))
        );
        assert_eq!(
            table.lookup(44),
            Some(Geometry::Point(point!(x: 6.63, y: 46.52)))
        );
        assert_eq!(table.lookup(99), None);

        Ok(())
    }

    #[test]
    fn test_open_reads_geometries_written_by_create() -> Result<()> {
        let workdir = TempDir::new()?;
        let file = NamedTempFile::new()?;
        let polygon = Polygon::new(
            LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            vec![],
        );
        let geometries = vec![(1_u64, Geometry::Polygon(polygon.clone()))];
        GeometryTable::create(
            geometries.into_iter(),
            workdir.path(),
            file.path(),
            crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
        )?;

        let table = GeometryTable::open(file.path())?;
        assert_eq!(
            table.modified()?,
            std::fs::metadata(file.path())?.modified()?
        );
        assert_eq!(table.len(), 1);
        assert_eq!(table.lookup(1), Some(Geometry::Polygon(polygon)));

        Ok(())
    }
}
