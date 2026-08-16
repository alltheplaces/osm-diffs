use super::EditSuggester;
use std::collections::HashMap;

/// Suggests tag edits for POI-like OSM features (shops today; other
/// categories once matching supports them -- see the module doc for why
/// that doesn't imply an edit suggester per matcher). Ported from the
/// old `PoiMatcher::suggest_edit` (see git history), unchanged in logic:
/// only *where* it lives changed, not *what* it does.
pub struct PoiEditSuggester;

impl PoiEditSuggester {
    /// Tells whether we trust an AllThePlaces tag enough to suggest an
    /// OpenStreetMap edit. AllThePlaces mostly propagates whatever comes
    /// from spidered websites, so we use an allowlist to prevent
    /// spamming human OpenStreetMap editors.
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

impl EditSuggester for PoiEditSuggester {
    fn suggest_edit(
        &self,
        atp_tags: &[(String, String)],
        osm_tags: &[(String, String)],
    ) -> Option<Vec<(String, String)>> {
        let osm_tags: HashMap<&str, &str> = osm_tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let tag_edits: Vec<(String, String)> = atp_tags
            .iter()
            .filter(|(key, _atp_value)| Self::is_atp_tag_trustworthy(key))
            .filter(|(key, atp_value)| {
                if let Some(osm_value) = osm_tags.get(key.as_str()) {
                    atp_value != osm_value
                } else {
                    true // OSM feature has no value yet for this key
                }
            })
            .cloned()
            .collect();
        if tag_edits.is_empty() {
            None
        } else {
            Some(tag_edits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(t: &[(&str, &str)]) -> Vec<(String, String)> {
        t.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn suggests_edit_for_trustworthy_changed_tag() {
        let atp = tags(&[
            ("opening_hours", "Mo-Fr 09:00-20:00"),
            ("name", "New Yorker"),
        ]);
        let osm = tags(&[
            ("opening_hours", "Mo-Fr 08:00-18:00"),
            ("name", "New Yorker"),
        ]);
        let edit = PoiEditSuggester
            .suggest_edit(&atp, &osm)
            .expect("should suggest an edit");
        assert_eq!(
            edit,
            vec![("opening_hours".to_string(), "Mo-Fr 09:00-20:00".to_string())]
        );
    }

    #[test]
    fn no_edit_when_tags_already_match() {
        let atp = tags(&[("opening_hours", "Mo-Fr 09:00-20:00")]);
        let osm = tags(&[("opening_hours", "Mo-Fr 09:00-20:00")]);
        assert!(PoiEditSuggester.suggest_edit(&atp, &osm).is_none());
    }

    #[test]
    fn ignores_untrustworthy_atp_tags() {
        // `name` is not on the trustworthy allowlist -- even though ATP
        // and OSM disagree, we don't want to propose spamming a human
        // editor with an unreviewed name change.
        let atp = tags(&[("name", "Something Else")]);
        let osm = tags(&[("name", "Original Name")]);
        assert!(PoiEditSuggester.suggest_edit(&atp, &osm).is_none());
    }

    #[test]
    fn suggests_edit_for_tag_missing_on_osm_side() {
        let atp = tags(&[("website", "https://example.com")]);
        let osm = tags(&[]);
        let edit = PoiEditSuggester
            .suggest_edit(&atp, &osm)
            .expect("should suggest an edit");
        assert_eq!(
            edit,
            vec![("website".to_string(), "https://example.com".to_string())]
        );
    }

    #[test]
    fn no_edit_when_no_trustworthy_tags_present() {
        let atp = tags(&[("brand", "New Yorker")]);
        let osm = tags(&[]);
        assert!(PoiEditSuggester.suggest_edit(&atp, &osm).is_none());
    }
}
