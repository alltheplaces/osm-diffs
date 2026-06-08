use crate::MatchMask;
use deepsize::DeepSizeOf;
use geo::Coord;
use serde::{Deserialize, Serialize};

mod matcher;
mod place_index;
mod writer;

pub use matcher::create_matcher;
pub use place_index::PlaceIndex;
pub use writer::ParquetWriter;

#[derive(Debug, DeepSizeOf, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Place {
    pub s2_cell_id: u64,
    pub osm_id: u64,
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
            osm_id: 0,
            source,
            mask,
            tags,
        })
    }

    pub fn deep_clone(&self) -> Self {
        Place {
            s2_cell_id: self.s2_cell_id,
            osm_id: self.osm_id,
            source: self.source.clone(),
            mask: self.mask,
            tags: self.tags.clone(),
        }
    }

    pub fn to_geojson(&self) -> geojson::Feature {
        let s2_cell_id = s2::cellid::CellID(self.s2_cell_id);
        let lat_lon = s2::latlng::LatLng::from(s2_cell_id);
        // Let's not emit coordinates with fake micrometer precision.
        let rounded_lon = (lat_lon.lng.deg() * 1e7).round() / 1e7;
        let rounded_lat = (lat_lon.lat.deg() * 1e7).round() / 1e7;
        let point = geo::point!(x: rounded_lon, y: rounded_lat);

        let id = if self.osm_id > 0 {
            Some(geojson::feature::Id::Number(self.osm_id.into()))
        } else {
            // TODO: Generate a unique ID from an AtomicU64. Use
            // counter value * 10, so it does not conflict with OSM
            // nodes (id * 10 + 1), ways (id * 10 + 2) or relations
            // (id * 10 + 3). For now, this is not an issue:
            // Currently, we never emit edit suggestions that aren't
            // for existing OSM features.  At some point in the
            // future, we’ll likely want to suggest creating new
            // features (from AllThePlaces features that don’t match
            // anything existing in OSM), and then we’ll need to give
            // them feature IDs that don’t conflict with anything else
            // in the generated PMTiles file.
            None
        };
        geojson::Feature {
            bbox: None,
            geometry: Some(geojson::Geometry::from(&point)),
            id,
            properties: Some(
                self.tags
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            ),
            foreign_members: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MatchMask;
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
    fn test_to_geojson() {
        let p = Coord {
            x: 7.447_812_3,
            y: 46.947_980_1,
        };
        let source = "test/source".to_string();
        let tags = vec![
            ("building".to_string(), "tower".to_string()),
            ("name:gsw".to_string(), "Zytglogge".to_string()),
        ];
        let mut place = Place::new(&p, source, MatchMask::SHOP, tags.clone()).unwrap();
        place.osm_id = 7891;
        let mut got_geojson = place.to_geojson();
        let (got_lon, got_lat) = point_coords(got_geojson.geometry.as_ref().unwrap());
        assert!((got_lon - p.x).abs() < 1e-6);
        assert!((got_lat - p.y).abs() < 1e-6);
        got_geojson.geometry = None;
        let expected_geojson: geojson::Feature = r#"{
            "type": "Feature",
            "id": 7891,
            "properties": { "building": "tower", "name:gsw": "Zytglogge" }
        }"#
        .parse()
        .unwrap();
        assert_eq!(got_geojson, expected_geojson);
    }

    fn point_coords(geometry: &geojson::Geometry) -> (f64, f64) {
        match &geometry.value {
            geojson::GeometryValue::Point { coordinates: c } => (c[0], c[1]),
            _ => panic!("expected a Point geometry"),
        }
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
        assert_eq!(a.eq(&b), false);
        assert_eq!(a.eq(&a), true);
        assert_eq!(a.cmp(&b), a.s2_cell_id.cmp(&b.s2_cell_id));
        assert_eq!(a.partial_cmp(&b), a.s2_cell_id.partial_cmp(&b.s2_cell_id));
    }
}
