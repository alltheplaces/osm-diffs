//! Builds `data/datapackage.json` -- a
//! [Frictionless Data Package v2](https://datapackage.org/) descriptor,
//! the server-independent way a client discovers the current release and
//! verifies each file.
//!
//! It is the one object the pipeline overwrites in place at a stable URL
//! (the CDN caches it for 60 s; everything else under `data/` is
//! immutable and long-cached -- see `docs/PRODUCTION.md`). A client
//! GETs ~1 KB, compares `version`, and resolves each `resources[].path`
//! (a bare dated basename) against the manifest's own URL.
//!
//! Hand-rolled with `serde_json`, the same style as `provenance.rs`, and
//! deterministic: every field is derived from the input-anchored
//! timestamp or from the files themselves, so a rebuild from the same
//! inputs is byte-identical. Conventions (`$schema`, a frozen `id`,
//! `version` = build date, relative `resources[].path`) match the
//! sibling project `brawer/osmviews` (its issue #110), so a downstream
//! client can treat both datasets the same way.

use crate::pipeline::{self, provenance};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;

const SCHEMA: &str = "https://datapackage.org/profiles/2.0/datapackage.json";
const NAME: &str = "osmdiffs";

/// The dataset's stable identity -- the same value for every build,
/// forever; a downstream catalog keys on it. Generated once as
/// `UUIDv5(URL, "https://github.com/alltheplaces/osm-diffs#datapackage")`
/// -- the repo's pre-2026-09 path, kept verbatim because the value is
/// frozen -- and frozen here. **Never regenerate this.**
const ID: &str = "5a01ca49-df2a-5645-8108-bb28c7ab246f";

const TITLE: &str = "AllThePlaces \u{2194} OpenStreetMap conflation";
const DESCRIPTION: &str = "For every AllThePlaces feature that plausibly maps to OpenStreetMap, \
    one row pairing it with its matched OpenStreetMap feature (or with no match). GeoParquet 2.0, \
    with an AllThePlaces geometry column and an OpenStreetMap one.";
const HOMEPAGE: &str = "https://github.com/brawer/osmdiffs";

/// One file published under `data/` -- carries everything both the S3
/// upload and the manifest's `resources[]` entry need. Built by
/// `pipeline::upload`.
#[derive(Clone)]
pub struct PublishedFile {
    /// Frictionless resource name: lowercase, `[a-z0-9._-]`.
    pub name: &'static str,
    /// Bare dated basename, e.g. `conflated-20260901-a1b2c3d4.parquet`.
    /// Relative, so it resolves next to `datapackage.json`.
    pub path: String,
    pub title: &'static str,
    pub description: &'static str,
    /// Frictionless resource `type` (`"file"` / `"json"`).
    pub frictionless_type: &'static str,
    pub format: &'static str,
    pub mediatype: &'static str,
    pub bytes: u64,
    /// Lowercase hex.
    pub sha256: String,
    /// Lowercase hex; `Some` only for `conflated.parquet` (its standalone
    /// BOM needs it). Never emitted into the manifest.
    pub sha512: Option<String>,
    /// `resources[].describes` -- set on the BOM resource, naming the
    /// resource it documents.
    pub describes: Option<&'static str>,
}

/// Builds the descriptor. `anchor` is the pipeline's input-anchored
/// timestamp (`provenance::read_anchor`): `version` is its calendar
/// date, `created` its full RFC 3339 form.
pub fn build_datapackage(
    workdir: &std::path::Path,
    anchor: UtcDateTime,
    files: &[PublishedFile],
) -> Result<Value> {
    let atp = pipeline::read_cached_atp_metadata(workdir)
        .context("could not read AllThePlaces provenance")?;
    let osm = pipeline::read_cached_metadata(workdir)
        .context("could not read OpenStreetMap provenance")?;

    Ok(json!({
        "$schema": SCHEMA,
        "name": NAME,
        "id": ID,
        "title": TITLE,
        "description": DESCRIPTION,
        "version": anchor.date().to_string(),
        "created": anchor.format(&Rfc3339).expect("UtcDateTime always formats as RFC 3339"),
        "homepage": HOMEPAGE,
        "licenses": [{
            "name": "ODbL-1.0",
            "path": provenance::ODBL_URL,
            "title": "Open Data Commons Open Database License v1.0",
        }],
        "sources": [
            {
                "title": format!(
                    "AllThePlaces run {}",
                    atp.start_time.format(&Rfc3339).expect("RFC 3339")
                ),
                "path": atp.output_url,
            },
            {
                "title": format!(
                    "OpenStreetMap planet {}",
                    osm.replication_timestamp.format(&Rfc3339).expect("RFC 3339")
                ),
                "path": "https://planet.openstreetmap.org/",
            },
        ],
        "resources": files.iter().map(resource).collect::<Vec<_>>(),
    }))
}

fn resource(f: &PublishedFile) -> Value {
    let mut r = json!({
        "name": f.name,
        "path": f.path,
        "type": f.frictionless_type,
        "title": f.title,
        "description": f.description,
        "format": f.format,
        "mediatype": f.mediatype,
        "bytes": f.bytes,
        "hash": format!("sha256:{}", f.sha256),
    });
    if let Some(describes) = f.describes {
        r["describes"] = json!(describes);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_input_metadata(workdir: &std::path::Path) {
        fs::write(
            workdir.join("alltheplaces.meta.json"),
            r#"{"run_id":"2026-08-29-00-00-00","output_url":"https://example.org/atp.zip",
                "history_url":"https://data.alltheplaces.xyz/runs/history.json",
                "start_time":"2026-08-29T13:32:16Z","end_time":"2026-08-30T04:14:22Z",
                "spiders":4856,"total_lines":38629108,"size_bytes":2537370317,
                "sha256":"1c86738e9e792a098aff313ea703ac86bf2b8a26eda75208b48b779b29f49157"}"#,
        )
        .unwrap();
        fs::write(
            workdir.join(format!("{}.meta.json", pipeline::PLANET_PBF_FILENAME)),
            r#"{"replication_timestamp":"2026-09-01T00:00:02Z","source":null,
                "writing_program":"osmx",
                "sha256":"56e12b62871018c7a969c9924bcfb1bdce15676bee706156260f101db809b9e1"}"#,
        )
        .unwrap();
    }

    fn sample_files() -> Vec<PublishedFile> {
        vec![
            PublishedFile {
                name: "conflated",
                path: "conflated-20260901-a1b2c3d4.parquet".to_string(),
                title: "Conflated AllThePlaces/OpenStreetMap dataset (GeoParquet)",
                description: "This dated object is immutable; the manifest's version advances.",
                frictionless_type: "file",
                format: "parquet",
                mediatype: "application/vnd.apache.parquet",
                bytes: 786_432_000,
                sha256: "a1b2c3d4".repeat(8),
                sha512: Some("ff".repeat(64)),
                describes: None,
            },
            PublishedFile {
                name: "bom",
                path: "conflated-20260901-a1b2c3d4.cdx.json".to_string(),
                title: "CycloneDX 1.7 provenance BOM",
                description: "Provenance for conflated.parquet.",
                frictionless_type: "json",
                format: "json",
                mediatype: "application/vnd.cyclonedx+json; version=1.7",
                bytes: 4096,
                sha256: "9f8e7d6c".repeat(8),
                sha512: None,
                describes: Some("conflated"),
            },
        ]
    }

    #[test]
    fn builds_a_valid_frictionless_v2_manifest() -> Result<()> {
        let workdir = TempDir::new()?;
        write_input_metadata(workdir.path());
        let anchor = UtcDateTime::parse("2026-09-01T00:00:02Z", &Rfc3339)?;

        let doc = build_datapackage(workdir.path(), anchor, &sample_files())?;

        assert!(
            doc["$schema"]
                .as_str()
                .unwrap()
                .starts_with("https://datapackage.org/profiles/2.0/")
        );
        assert_eq!(doc["name"], "osmdiffs");
        assert_eq!(doc["id"], ID);
        // version = the anchor's date; created = its full timestamp.
        assert_eq!(doc["version"], "2026-09-01");
        assert_eq!(doc["created"], "2026-09-01T00:00:02Z");
        assert_eq!(doc["licenses"][0]["name"], "ODbL-1.0");
        assert_eq!(
            doc["sources"][1]["path"],
            "https://planet.openstreetmap.org/"
        );

        let resources = doc["resources"].as_array().expect("resources array");
        assert_eq!(resources.len(), 2);
        for r in resources {
            let path = r["path"].as_str().unwrap();
            assert!(
                !path.contains('/') && !path.contains(':'),
                "resource path {path:?} must be a bare relative basename"
            );
            let hash = r["hash"].as_str().unwrap();
            assert!(
                hash.starts_with("sha256:") && hash.len() == "sha256:".len() + 64,
                "resource hash {hash:?} must be sha256:<64 hex>"
            );
        }
        assert_eq!(resources[1]["describes"], "conflated");
        assert!(resources[0].get("describes").is_none());
        // sha512 is an internal field, never serialized.
        assert!(resources[0].get("sha512").is_none());
        Ok(())
    }

    #[test]
    fn is_deterministic_for_identical_inputs() -> Result<()> {
        let workdir = TempDir::new()?;
        write_input_metadata(workdir.path());
        let anchor = UtcDateTime::parse("2026-09-01T00:00:02Z", &Rfc3339)?;

        let a = build_datapackage(workdir.path(), anchor, &sample_files())?;
        let b = build_datapackage(workdir.path(), anchor, &sample_files())?;
        assert_eq!(
            serde_json::to_string(&a)?,
            serde_json::to_string(&b)?,
            "build_datapackage must be byte-identical for identical inputs"
        );
        Ok(())
    }

    /// The full serialized manifest, pinned to a committed fixture so any
    /// change to its shape shows up in review. Regenerate with
    /// `UPDATE_GOLDEN=1 cargo test -p osm-diffs datapackage`.
    #[test]
    fn matches_golden() -> Result<()> {
        let workdir = TempDir::new()?;
        write_input_metadata(workdir.path());
        let anchor = UtcDateTime::parse("2026-09-01T00:00:02Z", &Rfc3339)?;
        let doc = build_datapackage(workdir.path(), anchor, &sample_files())?;
        let got = format!("{}\n", serde_json::to_string_pretty(&doc)?);

        let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/pipeline/testdata/datapackage.golden.json");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            fs::write(&golden, &got)?;
        }
        let want =
            fs::read_to_string(&golden).with_context(|| format!("read {}", golden.display()))?;
        assert_eq!(got, want, "manifest differs from {}", golden.display());
        Ok(())
    }
}
