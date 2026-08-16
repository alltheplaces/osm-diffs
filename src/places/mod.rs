use crate::matchers::MatchMask;
use deepsize::DeepSizeOf;
use geo::Coord;
use serde::{Deserialize, Serialize};
use std::num::{NonZeroU32, NonZeroU64};

mod reader;
mod writer;

pub use reader::PlaceReader;
pub use writer::ParquetWriter;

#[derive(Debug, DeepSizeOf, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Place {
    pub s2_cell_id: u64,
    pub osm_id: Option<NonZeroU64>,
    pub osm_changeset: Option<NonZeroU64>,
    pub osm_version: Option<NonZeroU32>,
    pub source: String,
    pub mask: MatchMask,
    pub tags: Vec<(String, String)>,
}

impl Place {
    pub fn new(
        coord: &Coord,
        source: String,
        mask: MatchMask,
        tags: Vec<(String, String)>,
    ) -> Option<Place> {
        let s2_lat_lng = s2::latlng::LatLng::from_degrees(coord.y, coord.x);
        if !s2_lat_lng.is_valid() || mask.is_empty() {
            return None;
        }

        let s2_cell_id = s2::cellid::CellID::from(s2_lat_lng).0;
        Some(Place {
            s2_cell_id,
            osm_id: None,
            osm_changeset: None,
            osm_version: None,
            source,
            mask,
            tags,
        })
    }

    pub fn deep_clone(&self) -> Self {
        Place {
            s2_cell_id: self.s2_cell_id,
            osm_id: self.osm_id,
            osm_changeset: self.osm_changeset,
            osm_version: self.osm_version,
            source: self.source.clone(),
            mask: self.mask,
            tags: self.tags.clone(),
        }
    }

    pub fn shape(&self) -> geo::Geometry<f64> {
        let s2_cell_id = s2::cellid::CellID(self.s2_cell_id);
        let lat_lon = s2::latlng::LatLng::from(s2_cell_id);
        let rounded_lon = (lat_lon.lng.deg() * 1e7).round() / 1e7;
        let rounded_lat = (lat_lon.lat.deg() * 1e7).round() / 1e7;
        geo::Geometry::from(geo::Point::<f64>::new(rounded_lon, rounded_lat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Coord;

    #[test]
    fn test_new() {
        let p = Coord {
            x: 7.447_812_3,
            y: 46.947_980_1,
        };
        let source = "test/source".to_string();
        let tags = vec![
            ("building".to_string(), "tower".to_string()),
            ("name:gsw".to_string(), "Zytglogge".to_string()),
        ];
        let place = Place::new(&p, source, MatchMask::SHOP, tags.clone()).unwrap();
        assert_eq!(place.s2_cell_id, 5156122125915201443);
        assert_eq!(place.source, "test/source");
        assert_eq!(place.tags, tags);
    }

    #[test]
    fn test_cmp() {
        let a = Place::new(
            &Coord {
                x: 7.4478123,
                y: 46.9479801,
            },
            "test/source".to_string(),
            MatchMask::SHOP,
            vec![],
        )
        .unwrap();
        let b = Place::new(
            &Coord {
                x: -122.4630042,
                y: 37.8045878,
            },
            "test/source".to_string(),
            MatchMask::SHOP,
            vec![],
        )
        .unwrap();
        assert!(!a.eq(&b));
        assert!(a.eq(&a));
        assert_eq!(a.cmp(&b), a.s2_cell_id.cmp(&b.s2_cell_id));
        assert_eq!(a.partial_cmp(&b), a.s2_cell_id.partial_cmp(&b.s2_cell_id));
    }

    #[test]
    fn test_shape() {
        let place = Place::new(
            &Coord {
                x: 7.4478123,
                y: 46.9479801,
            },
            "test/source".to_string(),
            MatchMask::SHOP,
            vec![],
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
}
