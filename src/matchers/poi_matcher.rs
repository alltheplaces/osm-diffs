use super::{MatchMask, Matcher, distance_score, parse_wikidata_id};
use crate::places::Place;
use s2::{cell::Cell, cellid::CellID, point::Point};
use std::collections::HashMap;

pub struct PoiMatcher<'a> {
    atp_place: &'a Place,
    center: Point,
    brand_wikidata: u64,
}

impl<'a> PoiMatcher<'a> {
    // TODO: For now, we only match based on brand:wikidata.
    // We should also look at names, by computing the token sort ratio,
    // but we should do this in a way that works for CJK. So we need
    // a decent segmenter that works for CJK languages. Maybe Lindera?
    pub fn for_place(place: &'a Place) -> Option<PoiMatcher<'a>> {
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
            Some(PoiMatcher {
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

impl<'a> Matcher for PoiMatcher<'a> {
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

    // TODO: This is not really a Matcher task. Move this elsewhere,
    // once we figure out what the final output format should be
    // for the pipeline.
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
            osm_changeset: osm_feature.osm_changeset,
            osm_version: osm_feature.osm_version,
            source: self.atp_place.source.clone(),
            mask: osm_feature.mask,
            tags: tag_edits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        num::{NonZeroI32, NonZeroI64, NonZeroU64},
        sync::LazyLock,
    };

    static CH_CLOTHES_ATP: LazyLock<Place> = LazyLock::new(|| Place {
        s2_cell_id: 5159637664633565895,
        osm_id: None,
        osm_changeset: None,
        osm_version: None,
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
        osm_changeset: NonZeroI64::new(149971213),
        osm_version: NonZeroI32::new(4),
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
        osm_changeset: None,
        osm_version: None,
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
        osm_changeset: NonZeroI64::new(157167503),
        osm_version: NonZeroI32::new(6),
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
    fn test_poi_matcher() {
        let ch_clothes_matcher =
            PoiMatcher::for_place(&CH_CLOTHES_ATP).expect("should create matcher");
        let ch_kiosk_matcher = PoiMatcher::for_place(&CH_KIOSK_ATP).expect("should create matcher");
        assert!(ch_clothes_matcher.score(&CH_CLOTHES_OSM) > 0.5);
        assert!(ch_clothes_matcher.score(&CH_KIOSK_OSM) == 0.0);
        assert!(ch_kiosk_matcher.score(&CH_CLOTHES_OSM) == 0.0);
        assert!(ch_kiosk_matcher.score(&CH_KIOSK_OSM) > 0.5);
    }
}
