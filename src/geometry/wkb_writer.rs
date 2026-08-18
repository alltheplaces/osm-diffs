//! Encodes a `geo::Geometry` as WKB (Well-Known Binary -- see
//! `geometry_tally`'s module comment).

use geo::Geometry;

/// Encodes `shape` as little-endian WKB.
pub fn write_wkb(shape: &Geometry<f64>) -> Vec<u8> {
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
        let wkb = write_wkb(&shape);
        // byte order (1 = little-endian), type code 1 (Point), x, y.
        assert_eq!(wkb[0], 1);
        assert_eq!(u32::from_le_bytes(wkb[1..5].try_into().unwrap()), 1);
        assert_eq!(f64::from_le_bytes(wkb[5..13].try_into().unwrap()), 1.5);
        assert_eq!(f64::from_le_bytes(wkb[13..21].try_into().unwrap()), 2.5);
    }
}
