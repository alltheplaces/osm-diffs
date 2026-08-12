//! Assembles this pipeline's provenance -- which input data, and which
//! version of the pipeline itself, produced a given output file -- as a
//! JSON document modeled on a CycloneDX 1.7 Bill of Materials, but
//! describing data lineage rather than code dependencies (component
//! `type: "data"`, one per input source, following the same shape this
//! repo already uses for non-crate "data" components in the build-time
//! code SBOM, see `scripts/sbom/pipeline.jq`). See issue #638.
//!
//! Deliberately reads each source's metadata straight back from
//! `workdir` -- [`AtpMetadata`] via [`crate::atp::read_cached_metadata`],
//! [`OsmMetadata`] via [`crate::pipeline::read_header`] -- rather than
//! having it threaded through from `import_atp`/`import_osm`'s return
//! values. That keeps BOM assembly decoupled from however those
//! importers (and whatever indexing they feed into) end up wired
//! together.

use crate::atp::{self, AtpMetadata};
use crate::pipeline::{self, OsmMetadata};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;

/// Builds the CycloneDX provenance document for one pipeline run, from
/// whatever `import_atp`/`import_osm` already left behind in `workdir`.
pub fn build(workdir: &Path) -> Result<Value> {
    let atp_metadata =
        atp::read_cached_metadata(workdir).context("could not read AllThePlaces provenance")?;
    let osm_planet = workdir.join(pipeline::PLANET_PBF_FILENAME);
    let osm_metadata =
        pipeline::read_header(&osm_planet).context("could not read OpenStreetMap provenance")?;

    // TODO: metadata.component isn't modeled correctly yet. We're
    // describing the provenance of a *data* file here (which sources,
    // and which software, built our output), not shipping a BOM for a
    // piece of software -- so `type: "application"` is wrong for this
    // component. To be fixed in a follow-up.
    Ok(json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.7",
        "metadata": {
            "component": {
                "type": "data",
                "name": "osm-diffs",
                "version": env!("CARGO_PKG_VERSION"),
            }
        },
        "components": [
            atp_component(&atp_metadata),
            osm_component(&osm_metadata),
        ],
    }))
}

fn atp_component(atp: &AtpMetadata) -> Value {
    json!({
        "type": "data",
        "bom-ref": format!("alltheplaces-{}", atp.run_id),
        "name": "alltheplaces",
        "version": atp.run_id,
        "properties": [
            {"name": "alltheplaces:run_id", "value": atp.run_id},
            {"name": "alltheplaces:output_url", "value": atp.output_url},
            {"name": "alltheplaces:history_url", "value": atp.history_url},
            {"name": "alltheplaces:start_time", "value": format_rfc3339(atp.start_time)},
            {"name": "alltheplaces:end_time", "value": format_rfc3339(atp.end_time)},
            {"name": "alltheplaces:spiders", "value": atp.spiders.to_string()},
            {"name": "alltheplaces:total_lines", "value": atp.total_lines.to_string()},
            {"name": "alltheplaces:size_bytes", "value": atp.size_bytes.to_string()},
        ],
    })
}

fn osm_component(osm: &OsmMetadata) -> Value {
    let replication_timestamp = format_rfc3339(osm.replication_timestamp);
    let mut properties = vec![json!({
        "name": "osm:replication_timestamp",
        "value": replication_timestamp,
    })];
    if let Some(source) = &osm.source {
        properties.push(json!({"name": "osm:source", "value": source}));
    }
    if let Some(writing_program) = &osm.writing_program {
        properties.push(json!({"name": "osm:writing_program", "value": writing_program}));
    }
    json!({
        "type": "data",
        "bom-ref": format!("openstreetmap-planet-{replication_timestamp}"),
        "name": "openstreetmap-planet",
        "version": replication_timestamp,
        "properties": properties,
    })
}

fn format_rfc3339(t: UtcDateTime) -> String {
    t.format(&Rfc3339)
        .expect("UtcDateTime should always format as RFC3339")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_build() -> Result<()> {
        let workdir = TempDir::new()?;

        fs::write(
            workdir.path().join("alltheplaces.meta.json"),
            r#"{
                "run_id": "2026-03-04-15-16-17",
                "output_url": "https://example.org/output.zip",
                "history_url": "https://data.alltheplaces.xyz/runs/history.json",
                "start_time": "2026-03-04T15:16:17Z",
                "end_time": "2026-03-04T18:42:03Z",
                "spiders": 3512,
                "total_lines": 3812044,
                "size_bytes": 812345678
            }"#,
        )?;

        let mut pbf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        pbf_path.push("tests/test_data/zugerland.osm.pbf");
        std::os::unix::fs::symlink(
            &pbf_path,
            workdir.path().join(pipeline::PLANET_PBF_FILENAME),
        )?;

        let bom = build(workdir.path())?;
        assert_eq!(bom["bomFormat"], "CycloneDX");
        assert_eq!(bom["specVersion"], "1.7");
        assert_eq!(bom["metadata"]["component"]["name"], "osm-diffs");
        assert_eq!(
            bom["metadata"]["component"]["version"],
            env!("CARGO_PKG_VERSION")
        );

        let components = bom["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);

        let atp = &components[0];
        assert_eq!(atp["type"], "data");
        assert_eq!(atp["name"], "alltheplaces");
        assert_eq!(atp["version"], "2026-03-04-15-16-17");
        assert_eq!(
            atp["properties"]
                .as_array()
                .expect("atp properties")
                .iter()
                .find(|p| p["name"] == "alltheplaces:spiders")
                .expect("alltheplaces:spiders property")["value"],
            "3512"
        );

        let osm = &components[1];
        assert_eq!(osm["type"], "data");
        assert_eq!(osm["name"], "openstreetmap-planet");
        assert_eq!(osm["version"], "2026-01-27T08:11:02Z");
        assert_eq!(
            osm["properties"]
                .as_array()
                .expect("osm properties")
                .iter()
                .find(|p| p["name"] == "osm:writing_program")
                .expect("osm:writing_program property")["value"],
            "osmx"
        );

        Ok(())
    }

    #[test]
    fn test_build_missing_atp_metadata() {
        let workdir = TempDir::new().expect("tempdir");
        assert!(build(workdir.path()).is_err());
    }
}
