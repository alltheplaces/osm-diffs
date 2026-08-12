//! Assembles this pipeline's provenance -- which input data, and which
//! version of the pipeline itself, produced a given output file -- as a
//! CycloneDX 1.7 Bill of Materials describing data lineage rather than
//! code dependencies. See issue #644 for the target shape and the
//! rationale behind each field (and its container-image counterpart,
//! `scripts/sbom/pipeline.jq`, for how this repo already models "data"
//! components and external tools in CycloneDX elsewhere).
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
use uuid::Uuid;

/// GitHub repository this pipeline is published from -- used to build
/// `metadata.tools.components[0]`'s external references and purl.
const REPO_URL: &str = "https://github.com/alltheplaces/osm-diffs";

/// Builds the CycloneDX provenance document for one pipeline run, from
/// whatever `import_atp`/`import_osm` already left behind in `workdir`.
pub fn build(workdir: &Path) -> Result<Value> {
    let atp_metadata =
        atp::read_cached_metadata(workdir).context("could not read AllThePlaces provenance")?;
    let osm_planet = workdir.join(pipeline::PLANET_PBF_FILENAME);
    let osm_metadata =
        pipeline::read_header(&osm_planet).context("could not read OpenStreetMap provenance")?;

    let run_timestamp = format_rfc3339(UtcDateTime::now());
    // One UUID, reused for both: `serialNumber` identifies this BOM
    // *document*, `formulation[].workflows[].uid` identifies the
    // workflow *execution* it documents ("the unique identifier for the
    // resource instance within its deployment context", per the
    // CycloneDX 1.7 schema) -- different concerns in general, but we
    // never run the same workflow twice, so document and execution are
    // always in 1:1 correspondence here.
    let run_id = Uuid::new_v4();
    let serial_number = run_id.urn().to_string();
    let workflow_uid = run_id.to_string();

    Ok(json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.7",
        "serialNumber": serial_number,
        "version": 1,
        "metadata": {
            "timestamp": run_timestamp,
            "tools": {
                "components": [tool_component()],
            },
            "component": output_component(&run_timestamp),
        },
        "components": [
            atp_component(&atp_metadata),
            osm_component(&osm_metadata),
        ],
        "formulation": [formulation(&workflow_uid)],
    }))
}

/// The `osm-diffs` pipeline itself, as the tool that produced the output
/// (distinct from `metadata.component`, which describes that output).
fn tool_component() -> Value {
    let version = env!("CARGO_PKG_VERSION");
    json!({
        "bom-ref": "tool-osm-diffs",
        "type": "application",
        "name": "osm-diffs",
        "version": version,
        "purl": format!("pkg:github/alltheplaces/osm-diffs@{version}"),
        "externalReferences": [
            {"type": "vcs", "url": REPO_URL},
            {"type": "release-notes", "url": format!("{REPO_URL}/releases/tag/{version}")},
        ],
    })
}

/// `conflated.parquet` itself -- the data file this BOM is embedded
/// into, described as data (not as the `osm-diffs` tool that built it,
/// which is `tool_component()` instead).
fn output_component(run_timestamp: &str) -> Value {
    json!({
        "bom-ref": "output-conflated",
        "type": "data",
        "name": "conflated.parquet",
        "mime-type": "application/vnd.apache.parquet",
        "version": run_timestamp,
        "description": "Conflated AllThePlaces/OpenStreetMap dataset.",
        "externalReferences": [
            // TODO(#645): docs/CONFLATED_OUTPUT.md doesn't exist yet --
            // this link is intentionally broken until it's written.
            {"type": "documentation", "url": format!("{REPO_URL}/blob/main/docs/CONFLATED_OUTPUT.md")},
        ],
        "data": [{"type": "dataset"}],
    })
}

fn atp_component(atp: &AtpMetadata) -> Value {
    json!({
        "bom-ref": "src-alltheplaces",
        "type": "data",
        "name": "alltheplaces",
        "version": format_rfc3339(atp.start_time),
        "data": [{"type": "dataset", "contents": {"url": atp.output_url}}],
        "externalReferences": [
            {"type": "distribution", "url": atp.output_url},
        ],
        // TODO(#646): add a "hashes" entry (SHA-256 of the downloaded
        // zip) once we compute one. Not done today.
    })
}

fn osm_component(osm: &OsmMetadata) -> Value {
    json!({
        "bom-ref": "src-osm-planet",
        "type": "data",
        "name": "openstreetmap-planet",
        "version": format_rfc3339(osm.replication_timestamp),
        "data": [{"type": "dataset"}],
        // TODO(#646): add a "hashes" entry (SHA-256 of the downloaded
        // .osm.pbf) once we compute one. Not done today.
    })
}

/// Ties `tool_component()`, the two input `*_component()`s, and
/// `output_component()` together into one workflow: which tool consumed
/// which inputs to produce which output. `uid` is required by the
/// CycloneDX 1.7 schema even though it's absent from #644's example
/// shape (confirmed via `cyclonedx-cli validate` against the real
/// schema) -- see its doc comment in `build()` for what it identifies.
fn formulation(workflow_uid: &str) -> Value {
    json!({
        "bom-ref": "formula-osm-diffs-build",
        "workflows": [{
            "bom-ref": "workflow-osm-diffs-build",
            "uid": workflow_uid,
            "name": "osm-diffs conflation",
            "taskTypes": ["build"],
            "resourceReferences": [{"ref": "tool-osm-diffs"}],
            "inputs": [
                {"resource": {"ref": "src-alltheplaces"}},
                {"resource": {"ref": "src-osm-planet"}},
            ],
            "outputs": [{"resource": {"ref": "output-conflated"}}],
        }],
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
        assert_eq!(bom["version"], 1);
        let serial_number = bom["serialNumber"].as_str().expect("serialNumber");
        assert!(serial_number.starts_with("urn:uuid:"));
        assert!(bom["metadata"]["timestamp"].as_str().is_some());

        let tool = &bom["metadata"]["tools"]["components"][0];
        assert_eq!(tool["bom-ref"], "tool-osm-diffs");
        assert_eq!(tool["type"], "application");
        assert_eq!(tool["name"], "osm-diffs");
        assert_eq!(tool["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            tool["purl"],
            format!(
                "pkg:github/alltheplaces/osm-diffs@{}",
                env!("CARGO_PKG_VERSION")
            )
        );

        let output = &bom["metadata"]["component"];
        assert_eq!(output["bom-ref"], "output-conflated");
        assert_eq!(output["type"], "data");
        assert_eq!(output["name"], "conflated.parquet");
        assert_eq!(output["mime-type"], "application/vnd.apache.parquet");
        assert!(output["version"].as_str().is_some());

        let components = bom["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);

        let atp = &components[0];
        assert_eq!(atp["bom-ref"], "src-alltheplaces");
        assert_eq!(atp["type"], "data");
        assert_eq!(atp["name"], "alltheplaces");
        assert_eq!(atp["version"], "2026-03-04T15:16:17Z");
        assert_eq!(
            atp["data"][0]["contents"]["url"],
            "https://example.org/output.zip"
        );

        let osm = &components[1];
        assert_eq!(osm["bom-ref"], "src-osm-planet");
        assert_eq!(osm["type"], "data");
        assert_eq!(osm["name"], "openstreetmap-planet");
        assert_eq!(osm["version"], "2026-01-27T08:11:02Z");

        // None of the deliberately-dropped ATP/OSM fields (run-health
        // stats, or values constant across every run) should leak into
        // the BOM anywhere -- serialize the whole document and check.
        // "source" is quoted: bare `source` is a substring of
        // `resource`/`resourceReferences`, which legitimately appear all
        // over `formulation`.
        let dump = bom.to_string();
        for leaked in [
            "run_id",
            "end_time",
            "spiders",
            "total_lines",
            "size_bytes",
            "history_url",
            "writing_program",
            "\"source\"",
        ] {
            assert!(!dump.contains(leaked), "BOM unexpectedly contains {leaked}");
        }

        // Every bom-ref the formulation refers to must actually exist.
        let known_refs = [
            "tool-osm-diffs",
            "output-conflated",
            "src-alltheplaces",
            "src-osm-planet",
        ];
        let formulation = &bom["formulation"][0]["workflows"][0];
        assert_eq!(
            formulation["resourceReferences"][0]["ref"],
            "tool-osm-diffs"
        );
        for input in formulation["inputs"].as_array().expect("inputs") {
            let r = input["resource"]["ref"].as_str().expect("ref");
            assert!(known_refs.contains(&r), "unknown bom-ref {r}");
        }
        assert_eq!(
            formulation["outputs"][0]["resource"]["ref"],
            "output-conflated"
        );

        Ok(())
    }

    #[test]
    fn test_build_missing_atp_metadata() {
        let workdir = TempDir::new().expect("tempdir");
        assert!(build(workdir.path()).is_err());
    }

    #[test]
    fn test_build_is_fresh_per_run() -> Result<()> {
        // serialNumber must not be reused across BOMs.
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

        let first = build(workdir.path())?;
        let second = build(workdir.path())?;
        assert_ne!(first["serialNumber"], second["serialNumber"]);
        Ok(())
    }
}
