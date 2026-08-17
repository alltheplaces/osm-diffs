//! Extracts the set of Wikidata IDs referenced by AllThePlaces data (via
//! `wikidata`/`brand:wikidata`/`operator:wikidata`/`network:wikidata`
//! tags), writing them to a [U64Set] on disk.
//!
//! Not consumed by anything yet -- kept for a planned future feature:
//! flagging OSM-only `conflated.parquet` rows (no ATP match) whose
//! brand/operator/network Wikidata ID is one ATP *does* have data for
//! elsewhere. That's a much sharper "is this OSM feature worth a
//! mapper's look" signal than emitting every unmatched OSM feature
//! would be -- ATP will never cover every tree/hydrant/etc. in the
//! world, so most unmatched OSM features are unmatched for a mundane
//! reason, not because they're stale. Restricting to brands ATP
//! actually tracks keeps that signal meaningful. See
//! alltheplaces/osm-diffs#682.

use crate::places::PlaceReader;
use crate::tables::U64Set;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// True for tag keys whose value(s) may hold Wikidata QIDs we care
/// about here -- `wikidata`, `brand:wikidata`, `operator:wikidata`,
/// `network:wikidata`, etc. -- but not `species:wikidata`, a completely
/// different namespace (the Wikidata item for a plant/animal species,
/// not a business/brand/network).
pub fn is_wikidata_key(key: &str) -> bool {
    key == "wikidata" || (key.ends_with(":wikidata") && key != "species:wikidata")
}

/// Parses `;`-separated Wikidata QIDs out of a tag value -- OSM's
/// established convention for multi-valued tags, e.g. `"Q123;Q813"`.
pub fn parse_wikidata_ids(value: &str) -> impl Iterator<Item = u64> + '_ {
    value.split(';').filter_map(|part| {
        let trimmed = part.trim();
        let digits = trimmed
            .strip_prefix('Q')
            .or_else(|| trimmed.strip_prefix('q'))?;
        digits.parse::<u64>().ok()
    })
}

/// Scans `atp` (`alltheplaces.parquet`) for Wikidata IDs and writes them
/// to `workdir/alltheplaces.wikidata-ids` as a [U64Set].
pub fn collect_wikidata_ids(atp: &Path, workdir: &Path) -> Result<PathBuf> {
    let out = workdir.join("alltheplaces.wikidata-ids");
    if out.exists() {
        return Ok(out);
    }

    let reader = PlaceReader::open(atp)?;
    let mut ids = Vec::new();
    for batch in reader.read_all()? {
        for place in batch? {
            for (key, value) in &place.tags {
                if is_wikidata_key(key) {
                    ids.extend(parse_wikidata_ids(value));
                }
            }
        }
    }

    let count = ids.len();
    // Sole external sort in this step (see `run_step("collect_wikidata_ids",
    // ...)` in pipeline/mod.rs), so it gets the full chunk-size budget.
    U64Set::create(
        ids.into_iter(),
        workdir,
        &out,
        crate::pipeline::EXTERNAL_SORT_CHUNK_BYTES,
    )?;
    log::info!(count = count; "collect_wikidata_ids: done");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_wikidata_key() {
        assert!(is_wikidata_key("wikidata"));
        assert!(is_wikidata_key("brand:wikidata"));
        assert!(is_wikidata_key("network:wikidata"));
        assert!(is_wikidata_key("operator:wikidata"));

        assert!(!is_wikidata_key("highway"));
        assert!(!is_wikidata_key("species:wikidata"));
    }

    #[test]
    fn test_parse_wikidata_ids() {
        let ids: Vec<u64> = parse_wikidata_ids(" Q123;Q813 ; q21").collect();
        assert_eq!(ids, vec![123, 813, 21]);
    }

    #[test]
    fn test_collect_wikidata_ids() -> Result<()> {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let mut atp = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        atp.push("tests/test_data/alltheplaces.parquet");
        let workdir = TempDir::new()?;
        let linked_atp = workdir.path().join("alltheplaces.parquet");
        symlink(&atp, &linked_atp)?;

        let out = collect_wikidata_ids(&linked_atp, workdir.path())?;
        let set = U64Set::open(&out)?;
        // Verified against the fixture directly (via DuckDB) rather than
        // assumed: of the 7 places in this file, 3 carry
        // brand:wikidata=Q116151325 (Misenso) and 4 carry
        // operator:wikidata=Q56825906 (Stadtgrün Winterthur) -- 2 unique
        // IDs once deduplicated.
        assert_eq!(set.len(), 2);
        assert!(set.contains(116151325));
        assert!(set.contains(56825906));

        // Calling again should hit the "already exists" memoization
        // path, not fail or recompute.
        let out2 = collect_wikidata_ids(&linked_atp, workdir.path())?;
        assert_eq!(out, out2);

        Ok(())
    }
}
