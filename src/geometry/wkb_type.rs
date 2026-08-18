//! Reads a WKB buffer's geometry type straight off its header, without
//! decoding any coordinates -- used for GeoParquet's `geometry_types`
//! metadata (see `pipeline::conflate::writer::GEO_METADATA_KEY`) and for
//! logging what kinds of geometry ended up in `conflated.parquet`,
//! without paying for a full `geo::Geometry` decode just to find out.

use std::fmt;

/// The seven basic OGC Simple Features geometry types, ignoring Z/M
/// dimensionality and SRID -- the only distinction GeoParquet's
/// `geometry_types` metadata (or a log line tallying what we wrote)
/// needs. `std::mem::discriminant` was considered instead of a plain
/// enum for the same purpose, but it isn't `Debug`/`Display`-printable,
/// which would make both the metadata and the log output harder to
/// follow for no real benefit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WkbGeometryType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    GeometryCollection,
}

impl WkbGeometryType {
    /// All seven variants, in declaration order -- lets a caller build a
    /// zero-initialized tally of every type up front, so a log line's
    /// set of fields stays the same across runs even when a type never
    /// occurs.
    pub const ALL: [WkbGeometryType; 7] = [
        Self::Point,
        Self::LineString,
        Self::Polygon,
        Self::MultiPoint,
        Self::MultiLineString,
        Self::MultiPolygon,
        Self::GeometryCollection,
    ];

    /// The GeoParquet `geometry_types` string for this type, e.g.
    /// `"Point"`, `"MultiPolygon"` -- see "geometry_types" in the
    /// [GeoParquet 2.0-rc.1 spec](https://geoparquet.org/releases/v2.0.0-rc.1/)
    /// (that page has no anchor links yet to point at the section
    /// directly).
    pub fn geoparquet_name(&self) -> &'static str {
        match self {
            Self::Point => "Point",
            Self::LineString => "LineString",
            Self::Polygon => "Polygon",
            Self::MultiPoint => "MultiPoint",
            Self::MultiLineString => "MultiLineString",
            Self::MultiPolygon => "MultiPolygon",
            Self::GeometryCollection => "GeometryCollection",
        }
    }
}

impl fmt::Display for WkbGeometryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.geoparquet_name())
    }
}

/// Reads the geometry type straight off a WKB buffer's header (byte 0 =
/// byte order, bytes 1..5 = geometry-type code) -- no validation or
/// decoding of any coordinates.
///
/// Only handles what this pipeline's own WKB writer
/// (`pipeline::conflate::writer::wkb`) ever produces: little-endian,
/// plain 2-D, no SRID flag. Panics on anything else -- we only ever
/// read WKB we wrote ourselves, so there's no reason to carry decoder
/// complexity (big-endian, Z/M dimensions, EWKB SRID) for encodings
/// that can't occur in this pipeline.
pub fn wkb_geometry_type(wkb: &[u8]) -> WkbGeometryType {
    assert!(
        wkb.len() >= 5,
        "WKB buffer too short for a header: {} bytes",
        wkb.len()
    );
    assert_eq!(
        wkb[0], 1,
        "expected little-endian WKB (byte order 1), got {}",
        wkb[0]
    );
    let code = u32::from_le_bytes(wkb[1..5].try_into().unwrap());
    match code {
        1 => WkbGeometryType::Point,
        2 => WkbGeometryType::LineString,
        3 => WkbGeometryType::Polygon,
        4 => WkbGeometryType::MultiPoint,
        5 => WkbGeometryType::MultiLineString,
        6 => WkbGeometryType::MultiPolygon,
        7 => WkbGeometryType::GeometryCollection,
        other => panic!("unsupported WKB geometry type code {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checked against the `wkb` crate's own writer
    /// (`wkb::common::WkbType::as_geometry_code`): byte 0 is `1` for
    /// little-endian, and a plain 2-D geometry's type code is exactly
    /// 1..=7 (no `Dimension` offset), so these minimal headers are
    /// exactly what `pipeline::conflate::writer::wkb` actually emits.
    #[test]
    fn reads_all_seven_base_types() {
        for (code, expected) in [
            (1u32, WkbGeometryType::Point),
            (2, WkbGeometryType::LineString),
            (3, WkbGeometryType::Polygon),
            (4, WkbGeometryType::MultiPoint),
            (5, WkbGeometryType::MultiLineString),
            (6, WkbGeometryType::MultiPolygon),
            (7, WkbGeometryType::GeometryCollection),
        ] {
            let mut buf = vec![1u8]; // little-endian
            buf.extend_from_slice(&code.to_le_bytes());
            assert_eq!(wkb_geometry_type(&buf), expected);
            assert_eq!(expected.geoparquet_name(), expected.to_string());
        }
    }

    #[test]
    #[should_panic(expected = "expected little-endian")]
    fn rejects_big_endian() {
        wkb_geometry_type(&[0, 0, 0, 0, 1]);
    }

    #[test]
    #[should_panic(expected = "unsupported WKB geometry type code")]
    fn rejects_unknown_type_code() {
        wkb_geometry_type(&[1, 99, 0, 0, 0]);
    }

    #[test]
    fn all_matches_geoparquet_name_uniquely() {
        let names: std::collections::HashSet<_> = WkbGeometryType::ALL
            .iter()
            .map(|t| t.geoparquet_name())
            .collect();
        assert_eq!(names.len(), WkbGeometryType::ALL.len());
    }
}
