use crate::MatchMask;
use deepsize::DeepSizeOf;
use geo::Coord;
use serde::{Deserialize, Serialize};

mod place_iter;
mod s2_row_group_index;
mod writer;
pub use place_iter::PlaceIter;
pub use s2_row_group_index::S2RowGroupIndex;
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
}

#[cfg(test)]
mod tests {
    use super::Place;
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
