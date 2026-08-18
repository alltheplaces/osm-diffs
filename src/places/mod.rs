//! `Place`, this pipeline's own working copy of AllThePlaces feature
//! data.
//!
//! The actual durable representation is AllThePlaces' weekly-run ZIP
//! file: one GeoJSON `FeatureCollection` per spider, one feature per
//! line, with the `FeatureCollection`'s own first line carrying
//! metadata like the collection timestamp and license (`pipeline::atp`
//! fetches and parses it). `alltheplaces.parquet`, this module's own
//! file, is not that durable copy -- it's a spatially sorted,
//! license-filtered (data whose license doesn't clear conflation with
//! OpenStreetMap, per OSM Licensing Working Group advice, is dropped --
//! see the BOM "evidence" link in `crate::provenance`) Parquet file
//! that lives only in the pipeline's local working directory and is
//! never uploaded anywhere. Written by `pipeline::atp`, read back by
//! `pipeline::conflate`/`pipeline::atp::wikidata_ids`, and consumed
//! directly by `matchers` when scoring a candidate match.
//!
//! Plays the same cross-pipeline-stage-storage role `crate::tables`
//! plays for OSM data, but isn't one of `tables`' mmap'd structures:
//! AllThePlaces' data volume is nowhere near planet scale, so plain
//! Arrow/Parquet (compressed, decoded batch by batch) is simpler and
//! sufficient here -- no need for `tables`' mmap + page-cache design.

use crate::matchers::MatchMask;
use crate::utils::UtcTimestamp;
use deepsize::DeepSizeOf;
use geo::Coord;
use geo_traits::to_geo::ToGeoGeometry;
use serde::{Deserialize, Serialize};

mod reader;
mod writer;

pub use reader::PlaceReader;
pub use writer::ParquetWriter;

/// An AllThePlaces feature.
#[derive(Debug, DeepSizeOf, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Place {
    /// A single S2 cell, derived from one representative point --
    /// `pipeline::atp::find_point`'s reduction of this feature's actual
    /// geometry (see `shape_wkb` below), regardless of whether that
    /// geometry is itself a point. Unlike `tables::OsmFeatureIndex`
    /// (which indexes real OSM geometry by its full multi-cell
    /// `coverage_s2_cell_id`), `Place` has no equivalent multi-cell
    /// coverage of its own -- not because it's assumed to be small
    /// enough to ignore, but because the `s2` crate this pipeline uses
    /// doesn't yet support computing S2 coverage for lines/polygons,
    /// only points. `pipeline::conflate` works around that by searching
    /// a radius around this cell instead of querying by actual coverage
    /// -- see
    /// [alltheplaces/osm-diffs#700](https://github.com/alltheplaces/osm-diffs/issues/700)
    /// for the plan to fix this once the library gains that support.
    pub s2_cell_id: u64,
    pub spider: String,
    pub mask: MatchMask,
    pub tags: Vec<(String, String)>,

    /// This feature's actual shape, as given by AllThePlaces' upstream
    /// source -- almost always a point, but not necessarily (some
    /// sources provide lines or polygons). Stored as WKB, not
    /// `geo::Geometry`, so `Place` can keep deriving `Eq`/`Ord` (needed
    /// for the external sort in `pipeline::atp::process_places`) --
    /// `f64` coordinates don't implement those. Distinct from
    /// `s2_cell_id` (see above), which is always a single cell derived
    /// from one representative point regardless of this field's actual
    /// geometry type. Use `shape()` for the decoded `geo::Geometry`.
    pub shape_wkb: Vec<u8>,

    /// When AllThePlaces fetched this feature from its upstream source
    /// (a spider's `spider:collection_time`, shared by every feature in
    /// that spider's run). Exposed in `conflated.parquet` as `atp.fetched`.
    pub fetched: UtcTimestamp,
}

impl Place {
    pub fn new(
        coord: &Coord,
        spider: String,
        mask: MatchMask,
        tags: Vec<(String, String)>,
        shape: &geo::Geometry<f64>,
        fetched: UtcTimestamp,
    ) -> Option<Place> {
        let s2_lat_lng = s2::latlng::LatLng::from_degrees(coord.y, coord.x);
        if !s2_lat_lng.is_valid() || mask.is_empty() {
            return None;
        }

        let s2_cell_id = s2::cellid::CellID::from(s2_lat_lng).0;
        Some(Place {
            s2_cell_id,
            spider,
            mask,
            tags,
            shape_wkb: crate::geometry::encode_wkb(shape),
            fetched,
        })
    }

    pub fn deep_clone(&self) -> Self {
        Place {
            s2_cell_id: self.s2_cell_id,
            spider: self.spider.clone(),
            mask: self.mask,
            tags: self.tags.clone(),
            shape_wkb: self.shape_wkb.clone(),
            fetched: self.fetched,
        }
    }

    /// This feature's actual shape -- see `shape_wkb`'s doc comment.
    pub fn shape(&self) -> geo::Geometry<f64> {
        wkb::reader::read_wkb(&self.shape_wkb)
            .expect("Place.shape_wkb should always be valid WKB -- we wrote it ourselves")
            .to_geometry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, Point, Polygon};

    fn test_timestamp() -> UtcTimestamp {
        UtcTimestamp(time::UtcDateTime::from_unix_timestamp(1_770_000_000).unwrap())
    }

    fn point_shape(x: f64, y: f64) -> geo::Geometry<f64> {
        geo::Geometry::from(Point::new(x, y))
    }

    #[test]
    fn test_new() {
        let p = Coord {
            x: 7.447_812_3,
            y: 46.947_980_1,
        };
        let spider = "test/spider".to_string();
        let tags = vec![
            ("building".to_string(), "tower".to_string()),
            ("name:gsw".to_string(), "Zytglogge".to_string()),
        ];
        let shape = point_shape(p.x, p.y);
        let place = Place::new(
            &p,
            spider,
            MatchMask::SHOP,
            tags.clone(),
            &shape,
            test_timestamp(),
        )
        .unwrap();
        assert_eq!(place.s2_cell_id, 5156122125915201443);
        assert_eq!(place.spider, "test/spider");
        assert_eq!(place.tags, tags);
        assert_eq!(place.fetched, test_timestamp());
    }

    #[test]
    fn test_cmp() {
        let coord_a = Coord {
            x: 7.4478123,
            y: 46.9479801,
        };
        let coord_b = Coord {
            x: -122.4630042,
            y: 37.8045878,
        };
        let a = Place::new(
            &coord_a,
            "test/spider".to_string(),
            MatchMask::SHOP,
            vec![],
            &point_shape(coord_a.x, coord_a.y),
            test_timestamp(),
        )
        .unwrap();
        let b = Place::new(
            &coord_b,
            "test/spider".to_string(),
            MatchMask::SHOP,
            vec![],
            &point_shape(coord_b.x, coord_b.y),
            test_timestamp(),
        )
        .unwrap();
        assert!(!a.eq(&b));
        assert!(a.eq(&a));
        assert_eq!(a.cmp(&b), a.s2_cell_id.cmp(&b.s2_cell_id));
        assert_eq!(a.partial_cmp(&b), a.s2_cell_id.partial_cmp(&b.s2_cell_id));
    }

    #[test]
    fn test_shape_round_trips_a_point() {
        let coord = Coord {
            x: 7.4478123,
            y: 46.9479801,
        };
        let place = Place::new(
            &coord,
            "test/spider".to_string(),
            MatchMask::SHOP,
            vec![],
            &point_shape(coord.x, coord.y),
            test_timestamp(),
        )
        .unwrap();
        let shape = place.shape();
        if let geo::Geometry::Point(p) = shape {
            assert_eq!(p.x(), 7.4478123);
            assert_eq!(p.y(), 46.9479801);
        } else {
            panic!("expected a point, got {:?}", shape);
        };
    }

    /// Regression test for
    /// alltheplaces/osm-diffs#690: `Place::shape()` must return this
    /// feature's real geometry, not a point reconstructed from
    /// `s2_cell_id` -- so this pins a *non-point* shape and a `coord`
    /// that's deliberately not one of its vertices (a real representative
    /// point -- e.g. an interior point -- would be, but the two are
    /// conceptually independent: `coord` only ever feeds `s2_cell_id`).
    #[test]
    fn test_shape_round_trips_a_polygon_not_reconstructed_from_s2_cell_id() {
        let coord = Coord { x: 0.0, y: 0.0 };
        let polygon = Polygon::new(
            geo::LineString::from(vec![
                (7.0, 46.0),
                (7.0, 47.0),
                (8.0, 47.0),
                (8.0, 46.0),
                (7.0, 46.0),
            ]),
            vec![],
        );
        let shape = geo::Geometry::from(polygon.clone());
        let place = Place::new(
            &coord,
            "test/spider".to_string(),
            MatchMask::SHOP,
            vec![],
            &shape,
            test_timestamp(),
        )
        .unwrap();
        assert_eq!(place.shape(), geo::Geometry::from(polygon));
    }
}
