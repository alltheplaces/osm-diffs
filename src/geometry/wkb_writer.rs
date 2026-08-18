//! Encodes a `geo::Geometry` as WKB.
//!
//! WKB is [Well-Known Binary](https://libgeos.org/specifications/wkb/),
//! a standard binary encoding for geometries (points, lines, polygons,
//! ...) used throughout this crate and the wider GIS world.

use geo::Geometry;

/// Encodes `shape` as little-endian WKB.
///
/// A thin wrapper around the `wkb` crate's own writer, not a direct
/// call to it: most of what we encode is a point (21 bytes), so this
/// pre-sizes the output buffer for that up front, saving a reallocation
/// in the common case; and since we only ever write our own data (never
/// re-encoding something read as big-endian), fixing the byte order
/// here once means call sites don't each have to repeat
/// `wkb::writer::WriteOptions { endianness: ... }` -- a simpler API,
/// and one less thing that could end up inconsistent between call sites.
pub fn encode_wkb(shape: &Geometry<f64>) -> Vec<u8> {
    // Most of our features have point geometry, which uses 21 bytes in WKB encoding.
    let mut buf = Vec::<u8>::with_capacity(21);
    let opts = wkb::writer::WriteOptions {
        endianness: wkb::Endianness::LittleEndian,
    };
    wkb::writer::write_geometry(&mut buf, shape, &opts).expect("wkb encoding failed");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Geometry, Point};

    #[test]
    fn encodes_little_endian_point() {
        let shape = Geometry::from(Point::new(1.5, 2.5));
        let wkb = encode_wkb(&shape);
        // byte order (1 = little-endian), type code 1 (Point), x, y.
        assert_eq!(wkb[0], 1);
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 1);
        assert_eq!(f64::from_le_bytes(wkb[5..13].try_into().unwrap()), 1.5);
        assert_eq!(f64::from_le_bytes(wkb[13..21].try_into().unwrap()), 2.5);
    }
}
