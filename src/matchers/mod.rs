//! Logic for matching AllThePlaces with OpenStreetMap features.

use crate::places::Place;
use deepsize::DeepSizeOf;
use s2::{cell::Cell, cellid::CellID, point::Point, s1::ChordAngle};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::LazyLock};

/// A bitmask to speed up the matching of AllThePlaces with OpenStreetMap.
///
/// The mask provides a first, very rough way to exclude matches. For example,
/// the bitmask for a shop (`MatchMask::SHOP`) typically does not intersect the bitmask
/// for a gas station (`MachMask::FUEL`) or a tree (`MatchMask::SHRUBBERY`).
///
/// For example, when we look for OSM features in the spatial neighborhood of a clothes
/// shop in AllThePlaces, this bitmask makes it possible to very efficiently skip over
/// any features that cannot possibly match, such as nearby benches, gas stations
/// or trees. Note that multiple bits may be set in a bitmask, such as when
/// an OpenStreetMap feature is tagged as both `shop=supermarket` and `amenity=fuel`.
///
/// # Examples
///
/// ```text
/// let mut shop = MatchMask::default();
/// shop.add_tag("shop", "clothes");
/// shop.add_tag("name", "Capybara T-Shirts");
///
/// let mut bench = MatchMask::default();
/// bench.add_tag("amenity", "bench");
/// bench.add_tag("color", "red");
///
/// assert_eq!(!shop.intersects(bench), false);
/// ```
#[derive(
    Clone, Copy, Debug, DeepSizeOf, Default, Deserialize, PartialEq, Eq, Ord, PartialOrd, Serialize,
)]
#[repr(transparent)]
pub struct MatchMask(pub u16);

impl MatchMask {
    pub const SHOP: MatchMask = MatchMask(1 << 0);
    pub const RESTAURANT: MatchMask = MatchMask(1 << 1);
    pub const LODGING: MatchMask = MatchMask(1 << 2);
    pub const SCHOOL: MatchMask = MatchMask(1 << 3);
    pub const TRANSIT: MatchMask = MatchMask(1 << 4);
    pub const PARKING: MatchMask = MatchMask(1 << 5);
    pub const FUEL: MatchMask = MatchMask(1 << 6);
    pub const SHRUBBERY: MatchMask = MatchMask(1 << 7);
    pub const STREET_FURNITURE: MatchMask = MatchMask(1 << 8);

    /// To compute match_distance for large objects such as railway platforms.
    const LARGE: MatchMask = MatchMask(1 << 9);

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn add_tag(&mut self, key: &str, value: &str) {
        self.0 |= match (key, value) {
            ("amenity", "arts_centre") => Self::SHOP.0,
            ("amenity", "bank") => Self::SHOP.0,
            ("amenity", "bench") => Self::STREET_FURNITURE.0,
            ("amenity", "bicycle_rental") => Self::SHOP.0,
            ("amenity", "bbq") => Self::STREET_FURNITURE.0,
            ("amenity", "bicycle_repair_station") => Self::STREET_FURNITURE.0,
            ("amenity", "bureau_de_change") => Self::SHOP.0,
            ("amenity", "childcare") => Self::SCHOOL.0,
            ("amenity", "dentist") => Self::SHOP.0,
            ("amenity", "drinking_water") => Self::STREET_FURNITURE.0,
            ("amenity", "driving_school") => Self::SCHOOL.0,
            ("amenity", "charging_station") => Self::FUEL.0,
            ("amenity", "ferry_terminal") => Self::TRANSIT.0,
            ("amenity", "fountain") => Self::STREET_FURNITURE.0,
            ("amenity", "fuel") => Self::FUEL.0,
            ("amenity", "grit_bin") => Self::STREET_FURNITURE.0,
            ("amenity", "ice_cream") => Self::SHOP.0,
            ("amenity", "kindergarten") => Self::SCHOOL.0,
            ("amenity", "letter_box") => Self::STREET_FURNITURE.0,
            ("amenity", "library") => Self::SHOP.0,
            ("amenity", "motorcycle_parking") => Self::PARKING.0,
            ("amenity", "nightclub") => Self::SHOP.0,
            ("amenity", "parking") => Self::PARKING.0,
            ("amenity", "pharmacy") => Self::SHOP.0,
            ("amenity", "place_of_worship") => Self::SHOP.0,
            ("amenity", "post_box") => Self::STREET_FURNITURE.0,
            ("amenity", "post_office") => Self::SHOP.0,
            ("amenity", "public_bookcase") => Self::STREET_FURNITURE.0,
            ("amenity", "pub") => Self::RESTAURANT.0,
            ("amenity", "recycling") => Self::STREET_FURNITURE.0,
            ("amenity", "studio") => Self::SHOP.0,
            ("amenity", "taxi") => Self::TRANSIT.0,
            ("amenity", "telephone") => Self::STREET_FURNITURE.0,
            ("amenity", "theatre") => Self::SHOP.0,
            ("amenity", "trolley_bay") => Self::STREET_FURNITURE.0,
            ("amenity", "restaurant") => Self::RESTAURANT.0,
            ("amenity", "university") => Self::SCHOOL.0,
            ("amenity", "vending_machine") => Self::STREET_FURNITURE.0,
            ("amenity", "waste_basket") => Self::STREET_FURNITURE.0,
            ("amenity", "water_point") => Self::STREET_FURNITURE.0,
            ("amenity", "watering_place") => Self::STREET_FURNITURE.0,

            ("barrier", "fence") => Self::STREET_FURNITURE.0,
            ("barrier", "gate") => Self::STREET_FURNITURE.0,
            ("barrier", "hedge") => Self::SHRUBBERY.0,
            ("barrier", "stile") => Self::STREET_FURNITURE.0,

            ("emergency", "acess_point") => Self::STREET_FURNITURE.0,
            ("emergency", "assembly_point") => Self::STREET_FURNITURE.0,
            ("emergency", "bleed_control_kit") => Self::STREET_FURNITURE.0,
            ("emergency", "defibrillator") => Self::STREET_FURNITURE.0,
            ("emergency", "disaster_help_point") => Self::STREET_FURNITURE.0,
            ("emergency", "fire_alarm_box") => Self::STREET_FURNITURE.0,
            ("emergency", "fire_hydrant") => Self::STREET_FURNITURE.0,
            ("emergency", "first_aid_kit") => Self::STREET_FURNITURE.0,
            ("emergency", "key_depot") => Self::STREET_FURNITURE.0,
            ("emergency", "rescue_box") => Self::STREET_FURNITURE.0,
            ("emergency", "shower") => Self::STREET_FURNITURE.0,

            ("highway", "bus_stop") => Self::TRANSIT.0,
            ("highway", "elevator") => Self::STREET_FURNITURE.0,
            ("highway", "fake_speed_camera") => Self::STREET_FURNITURE.0,
            ("highway", "ladder") => Self::STREET_FURNITURE.0,
            ("highway", "milestone") => Self::STREET_FURNITURE.0,
            ("highway", "speed_camera") => Self::STREET_FURNITURE.0,
            ("highway", "street_lamp") => Self::STREET_FURNITURE.0,
            ("highway", "traffic_sign") => Self::STREET_FURNITURE.0,
            ("highway", "traffic_signals") => Self::STREET_FURNITURE.0,

            ("historic", "boundary_stone") => Self::STREET_FURNITURE.0,
            ("historic", "wayside_cross") => Self::STREET_FURNITURE.0,
            ("historic", "wayside_shrine") => Self::STREET_FURNITURE.0,

            ("leisure", "adult_gaming_centre") => Self::SHOP.0,
            ("leisure", "amusement_arcade") => Self::SHOP.0,
            ("leisure", "bandstand") => Self::STREET_FURNITURE.0,
            ("leisure", "bird_hide") => Self::STREET_FURNITURE.0,
            ("leisure", "bowling_alley") => Self::SHOP.0,
            ("leisure", "dance") => Self::SHOP.0,
            ("leisure", "minigolf") => Self::SHOP.0,
            ("leisure", "firepit") => Self::STREET_FURNITURE.0,
            ("leisure", "hot_tub") => Self::STREET_FURNITURE.0,
            ("leisure", "playground") => Self::SHOP.0,
            ("leisure", "picnic_table") => Self::STREET_FURNITURE.0,
            ("leisure", "resort") => Self::SHOP.0,
            ("leisure", "water_park") => Self::SHOP.0,

            ("man_made", "cutline") => 0,
            ("man_made", _) => Self::STREET_FURNITURE.0,

            ("natural", "bush") => Self::SHRUBBERY.0,
            ("natural", "heath") => Self::SHRUBBERY.0,
            ("natural", "plant") => Self::SHRUBBERY.0,
            ("natural", "shrub") => Self::SHRUBBERY.0,
            ("natural", "shrubbery") => Self::SHRUBBERY.0,
            ("natural", "tree") => Self::SHRUBBERY.0,
            ("natural", "trees") => Self::SHRUBBERY.0,
            ("natural", "tree_group") => Self::SHRUBBERY.0,
            ("natural", "tree_row") => Self::SHRUBBERY.0,

            ("power", "cable") => 0,
            ("power", "line") => 0,
            ("power", "minor_cable") => 0,
            ("power", "minor_line") => 0,
            ("power", "no") => 0,
            ("power", "yes") => 0,
            ("power", _) => Self::STREET_FURNITURE.0,

            ("public_transport", "no") => 0,
            ("public_transport", "platform") => Self::TRANSIT.0 | Self::LARGE.0,
            ("public_transport", _) => Self::TRANSIT.0,

            ("railway", "platform") => Self::TRANSIT.0 | Self::LARGE.0,

            ("school", "no") => 0,
            ("school", _) => Self::SCHOOL.0,

            ("shop", "no") => 0,
            ("shop", _) => Self::SHOP.0,

            ("tourism", "alpine_hut") => Self::LODGING.0,
            ("tourism", "apartment") => Self::LODGING.0,
            ("tourism", "apartments") => Self::LODGING.0,
            ("tourism", "cabin") => Self::LODGING.0,
            ("tourism", "camp_site") => Self::LODGING.0 | Self::LARGE.0,
            ("tourism", "caravan_site") => Self::LODGING.0 | Self::LARGE.0,
            ("tourism", "guest_house") => Self::LODGING.0,
            ("tourism", "hostel") => Self::LODGING.0,
            ("tourism", "hotel") => Self::LODGING.0,
            ("tourism", "hotel;motel") => Self::LODGING.0,
            ("tourism", "motel") => Self::LODGING.0,
            ("tourism", "wilderness_hut") => Self::LODGING.0,

            ("tower", _) => Self::STREET_FURNITURE.0,

            _ => 0,
        }
    }

    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        (self.0 & other.0) != 0
    }
}

/// Return the maximal spatial distance we allow so that another feature
/// is still considered close enough to be a potential match.
/// The returned distance depends on the MatchMask, which is derived
/// from the feature’s tags. Currently, the distances are as follows:
///
/// * 10 meters for ‘small’ features such as trees, benches or trash cans;
/// * 400 meters for ‘medium’ features such as shops or schools;
/// * 1000 meters for ‘large’ features such as railway platforms.
///
/// For performance reasons, the result is not returned in meters,
/// but as an equivalent ChordAngle of the S2 spherical geometry library.
pub fn match_distance(mask: &MatchMask) -> ChordAngle {
    const SMALL_BITS: u16 = MatchMask::SHRUBBERY.0 | MatchMask::STREET_FURNITURE.0;
    let has_only_small_bits = (mask.0 & SMALL_BITS) == mask.0 && (mask.0 & SMALL_BITS) != 0;
    if has_only_small_bits {
        *SMALL_DISTANCE
    } else if mask.intersects(&MatchMask::LARGE) {
        *LARGE_DISTANCE
    } else {
        *MEDIUM_DISTANCE
    }
}

static SMALL_DISTANCE: LazyLock<ChordAngle> = LazyLock::new(|| meters_to_chord_angle(10.0));
static MEDIUM_DISTANCE: LazyLock<ChordAngle> = LazyLock::new(|| meters_to_chord_angle(400.0));
static LARGE_DISTANCE: LazyLock<ChordAngle> = LazyLock::new(|| meters_to_chord_angle(1000.0));

fn meters_to_chord_angle(radius_meters: f64) -> ChordAngle {
    use s2::s1::angle::{Angle, Rad};
    const EARTH_RADIUS_METERS: f64 = 6_371_000.0;
    ChordAngle::from(Angle::from(Rad(radius_meters / EARTH_RADIUS_METERS)))
}

/// Trait for objects that can score a `Place`.
pub trait Matcher {
    /// Returns a score between 0.0 and 1.0 indicating how well the place matches.
    /// A high score means a good match; 0.0 means the place is clearly not a match.
    fn score(&self, place: &Place) -> f64;

    fn suggest_edit(&self, osm_feature: &Place) -> Option<Place>;
}

/// Construct a matcher for a given AllThePlaces feature.
///
/// Use the returned matcher to score nearby OpenStreetMap features
/// how likely they are to be about the same real-world object.
/// For example, a matcher for brand stores will look at the brand
/// of OpenStreetMap features, and only match when the candidate
/// is of the same brand; a matcher for trees might consider
/// the plant species.
pub fn create_matcher(place: &Place) -> Option<Box<dyn Matcher + '_>> {
    if let Some(matcher) = ShopMatcher::for_place(place) {
        Some(Box::new(matcher))
    } else {
        // TODO: Implement matchers for fire hydrants, defibrillators, etc.
        None
    }
}

fn distance(pt: &Point, place: &Place) -> f64 {
    let pt2 = Point(CellID(place.s2_cell_id).raw_point().normalize());
    pt.distance(&pt2).rad() * 6_371_000.0
}

fn distance_score(pt: &Point, place: &Place, max_meters: f64) -> f64 {
    let dist = distance(pt, place);
    if dist <= max_meters {
        (max_meters - dist) / max_meters
    } else {
        0.0
    }
}

fn parse_wikidata_id(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    let digits = trimmed
        .strip_prefix('Q')
        .or_else(|| trimmed.strip_prefix('q'))?;
    digits.parse::<u64>().ok()
}

struct ShopMatcher<'a> {
    atp_place: &'a Place,
    center: Point,
    brand_wikidata: u64,
}

impl<'a> ShopMatcher<'a> {
    // TODO: For now, we only match based on brand:wikidata.
    // We should also look at names, by computing the token sort ratio,
    // but we should do this in a way that works for CJK. So we need
    // a decent segmenter that works for CJK languages. Maybe Lindera?
    fn for_place(place: &'a Place) -> Option<ShopMatcher<'a>> {
        if !place.mask.intersects(&MatchMask::SHOP) {
            return None;
        }
        let mut brand_wikidata: Option<u64> = None;
        for (k, v) in place.tags.iter() {
            if k.as_str() == "brand:wikidata" {
                brand_wikidata = parse_wikidata_id(v);
                break;
            }
        }

        if let Some(brand_wikidata) = brand_wikidata {
            let center = Cell::from(CellID(place.s2_cell_id)).center();
            Some(ShopMatcher {
                atp_place: place,
                center,
                brand_wikidata,
            })
        } else {
            None
        }
    }

    /// Tells whether we trust an AllThePlaces tag enough to suggest an OpenStreetMap edit.
    /// AllThePlaces mostly propagates whatever comes from spidered websites,
    /// so we use an allowlist to prevent spamming human OpenStreetMap editors.
    fn is_atp_tag_trustworthy(key: &str) -> bool {
        // Before you add entries to this list, please make sure that the quality
        // is good. To evaluate, look at the diff of workdir/shops.jsonl
        // from before and after your change to this code.
        matches!(
            key,
            "email" | "end_date" | "fax" | "opening_hours" | "phone" | "start_date" | "website"
        )
    }
}

impl<'a> Matcher for ShopMatcher<'a> {
    fn score(&self, candidate: &Place) -> f64 {
        let mut candidate_brand_wikidata: Option<u64> = None;
        for (k, v) in candidate.tags.iter() {
            if k.as_str() == "brand:wikidata" {
                candidate_brand_wikidata = parse_wikidata_id(v);
                break;
            }
        }

        let distance_score = distance_score(&self.center, candidate, 400.0);
        if Some(self.brand_wikidata) == candidate_brand_wikidata {
            distance_score
        } else {
            0.0
        }
    }

    fn suggest_edit(&self, osm_feature: &Place) -> Option<Place> {
        let osm_tags: HashMap<&str, &str> = osm_feature
            .tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let tag_edits: Vec<(String, String)> = self
            .atp_place
            .tags
            .iter()
            .filter(|(key, _atp_value)| Self::is_atp_tag_trustworthy(key))
            .filter(|(key, atp_value)| {
                if let Some(osm_value) = osm_tags.get::<str>(key.as_ref()) {
                    atp_value != osm_value
                } else {
                    true // OSM feature has no value yet for this key
                }
            })
            .cloned()
            .collect();
        if tag_edits.is_empty() {
            return None;
        }
        Some(Place {
            s2_cell_id: osm_feature.s2_cell_id,
            osm_id: osm_feature.osm_id,
            source: self.atp_place.source.clone(),
            mask: osm_feature.mask,
            tags: tag_edits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroU64, sync::LazyLock};

    #[test]
    fn test_match_distance() {
        assert!(match_distance(&MatchMask::SHRUBBERY) < match_distance(&MatchMask::RESTAURANT));

        let mut mask = MatchMask::default();
        mask.add_tag("amenity", "bench");
        mask.add_tag("shop", "yes");
        assert!(match_distance(&mask) == match_distance(&MatchMask::SHOP));
    }

    static CH_CLOTHES_ATP: LazyLock<Place> = LazyLock::new(|| Place {
        s2_cell_id: 5159637664633565895,
        osm_id: None,
        source: String::from("atp/newyorker"),
        mask: MatchMask::SHOP,
        tags: tags(&[
            ("addr:city", "Rapperswil"),
            ("addr:country", "CH"),
            ("addr:full", "Zürcherstrasse 4, 8640 Rapperswil"),
            ("addr:postcode", "8640"),
            ("brand", "New Yorker"),
            ("brand:wikidata", "Q706421"),
            ("name", "NEW YORKER | EKZ Sonnenhof"),
            ("opening_hours", "Mo-Fr 09:00-20:00; Sa 08:00-18:00"),
            ("phone", "+41 55 210 63 10"),
            ("ref", "5b3cb3fac00458481b4375e1"),
            ("shop", "clothes"),
        ]),
    });

    static CH_CLOTHES_OSM: LazyLock<Place> = LazyLock::new(|| Place {
        s2_cell_id: 5159637664662121729,
        osm_id: NonZeroU64::new(10761965859),
        source: String::from("osm"),
        mask: MatchMask(1),
        tags: tags(&[
            ("branch", "Rapperswil Sonnenhof"),
            ("brand", "New Yorker"),
            ("brand:wikidata", "Q706421"),
            ("level", "0"),
            ("name", "New Yorker"),
            ("opening_hours", "Mo-Fr 09:00-20:00; Sa 08:00-18:00"),
            ("shop", "clothes"),
            ("website", "https://www.newyorker.de/ch/"),
        ]),
    });

    static CH_KIOSK_ATP: LazyLock<Place> = LazyLock::new(|| Place {
        s2_cell_id: 5159637400739491865,
        osm_id: None,
        source: String::from("atp/valora"),
        mask: MatchMask::SHOP,
        tags: tags(&[
            ("addr:city", "Rapperswil"),
            ("addr:country", "CH"),
            ("addr:postcode", "8640"),
            ("addr:street_address", "Unterführung Bahnhof 1"),
            ("brand", "k kiosk"),
            ("brand:wikidata", "Q60381703"),
            ("name", "kkiosk Rapperswil BHF"),
            ("opening_hours", "Mo-Fr 05:30-20:00; Sa-Su 07:00-20:00"),
            ("ref", "730"),
            ("shop", "newsagent"),
        ]),
    });

    static CH_KIOSK_OSM: LazyLock<Place> = LazyLock::new(|| Place {
        s2_cell_id: 5159637400743919515,
        osm_id: NonZeroU64::new(6028968648),
        source: String::from("osm"),
        mask: MatchMask::SHOP,
        tags: tags(&[
            ("brand", "k kiosk"),
            ("brand:wikidata", "Q60381703"),
            ("brand:wikipedia", "it:K Kiosk"),
            ("cash_withdrawal", "yes"),
            ("cash_withdrawal:fee", "no"),
            ("cash_withdrawal:operator", "sonect"),
            ("cash_withdrawal:purchase_required", "no"),
            ("cash_withdrawal:type", "checkout"),
            ("level", "-1"),
            ("name", "k kiosk"),
            (
                "opening_hours",
                "Mo-Fr 05:45-19:00; Sa 08:00-18:00; Su 09:00-18:00",
            ),
            ("shop", "kiosk"),
            ("wheelchair", "yes"),
        ]),
    });

    fn tags(t: &[(&str, &str)]) -> Vec<(String, String)> {
        t.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_shop_matcher() {
        let ch_clothes_matcher = create_matcher(&CH_CLOTHES_ATP).expect("should create matcher");
        let ch_kiosk_matcher = create_matcher(&CH_KIOSK_ATP).expect("should create matcher");
        assert!(ch_clothes_matcher.score(&CH_CLOTHES_OSM) > 0.5);
        assert!(ch_clothes_matcher.score(&CH_KIOSK_OSM) == 0.0);
        assert!(ch_kiosk_matcher.score(&CH_CLOTHES_OSM) == 0.0);
        assert!(ch_kiosk_matcher.score(&CH_KIOSK_OSM) > 0.5);
    }
}
