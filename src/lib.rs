use deepsize::DeepSizeOf;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use s2::s1::ChordAngle;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::LazyLock;

mod atp;
mod coverage;
mod diff_places;
mod matchers;
mod osm;
mod places;
mod s2_util;
mod tiles;
mod u64_table;
mod upload;

// Re-exported for main.rs.
pub use atp::import_atp;
pub use coverage::build_coverage;
pub use diff_places::suggest_edits;
pub use osm::import_osm;
pub use tiles::render_tiles;
pub use upload::upload_tiles;

#[cfg(fuzzing)]
pub use import_atp::fuzz::fuzz_process_geojson;

const PROGRESS_BAR_STYLE: &str =
    "{prefix} [{elapsed_precise}] {bar:42.blue} {pos:>7}/{len:7} {msg}";

const DOWNLOAD_BAR_STYLE: &str = "{prefix} [{elapsed_precise}] {bar:42.blue} {bytes_per_sec} {eta}";

const SPINNER_STYLE: &str = "{prefix} [{elapsed_precise}] {spinner:blue} {msg} {bytes_per_sec}";

fn make_progress_bar(
    progress: &MultiProgress,
    phase: &str,
    max_value: u64,
    message: &str,
) -> ProgressBar {
    let bar = progress.add(ProgressBar::new(max_value));
    bar.set_prefix(String::from(phase));
    bar.set_message(String::from(message));
    let style = ProgressStyle::with_template(PROGRESS_BAR_STYLE).expect("bad PROGRESS_BAR_STYLE");
    bar.set_style(style);
    bar
}

fn make_download_bar(
    progress: &MultiProgress,
    phase: &str,
    content_length: Option<u64>,
) -> ProgressBar {
    if let Some(content_length) = content_length {
        let bar = progress.add(ProgressBar::new(content_length));
        bar.set_prefix(String::from(phase));
        let style =
            ProgressStyle::with_template(DOWNLOAD_BAR_STYLE).expect("bad DOWNLOAD_BAR_STYLE");
        bar.set_style(style);
        bar
    } else {
        let bar = progress.add(ProgressBar::new_spinner());
        bar.set_prefix(String::from(phase));
        let style = ProgressStyle::with_template(SPINNER_STYLE).expect("bad SPINNER_STYLE");
        bar.set_style(style);
        bar
    }
}

#[derive(
    Clone, Copy, Debug, DeepSizeOf, Default, Deserialize, PartialEq, Eq, Ord, PartialOrd, Serialize,
)]
#[repr(transparent)]
pub struct MatchMask(u16);

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

fn match_distance(mask: &MatchMask) -> ChordAngle {
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

/// A single layer to be rendered into tiles with Tippecanoe.
/// Instances are passed from `suggest_edits()` to `tiles::render()`.
#[derive(Debug)]
pub struct TileLayer {
    pub name: String,
    pub path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_distance() {
        assert!(match_distance(&MatchMask::SHRUBBERY) < match_distance(&MatchMask::RESTAURANT));

        let mut mask = MatchMask::default();
        mask.add_tag("amenity", "bench");
        mask.add_tag("shop", "yes");
        assert!(match_distance(&mask) == match_distance(&MatchMask::RESTAURANT));
    }
}
