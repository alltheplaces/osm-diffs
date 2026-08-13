//! Assembles the provenance of this pipeline's public output: a
//! machine-readable document describing which version of this pipeline,
//! using exactly what input, and at what time, produced a given output
//! file. We use the industry-standard CycloneDX JSON format for this,
//! and embed the document into the output file's own metadata (for
//! Parquet, that's key-value metadata -- see
//! `pipeline::conflate::writer`).
//!
//! Reads each input source's metadata straight back from `workdir` --
//! [`AtpMetadata`] via [`crate::atp::read_cached_metadata`],
//! [`OsmMetadata`] via [`crate::pipeline::read_header`] -- rather than
//! having it threaded through from `import_atp`/`import_osm`'s return
//! values, so this stays decoupled from however those importers (and
//! whatever indexing they feed into) end up wired together.

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

/// Supplier declared for this BOM and both input components. Copied
/// verbatim from `scripts/sbom/merge.jq`'s `metadata.supplier` (the
/// container-image SBOM) rather than kept in sync programmatically --
/// the project name won't change, and if it ever does, a grep finds
/// both places.
fn supplier() -> Value {
    json!({
        "name": "All The Places",
        "url": ["https://github.com/alltheplaces/"]
    })
}

/// A CycloneDX `licenses` array for a single SPDX-recognized license,
/// e.g. `license("CC0-1.0")`.
fn license(spdx_id: &str) -> Value {
    json!([{"license": {"id": spdx_id}}])
}

/// Builds the CycloneDX provenance document for `conflated.parquet`,
/// from whatever `import_atp`/`import_osm` already left behind in
/// `workdir`. `pipeline_run_id` becomes
/// `formulation[].workflows[].uid` -- the identifier this pipeline
/// invocation was given from outside (e.g. a Kubernetes Job run ID), if
/// any; the empty string when run locally/interactively without one.
/// CycloneDX's `uid` is *not* necessarily a UUID -- unlike
/// `serialNumber` (this BOM document's own identity, which we do mint
/// here), it identifies the actual workflow execution "within its
/// deployment context", something this code has no way to know on its
/// own.
pub fn build_bom_for_conflated_parquet(workdir: &Path, pipeline_run_id: &str) -> Result<Value> {
    let atp_metadata =
        atp::read_cached_metadata(workdir).context("could not read AllThePlaces provenance")?;
    let osm_planet = workdir.join(pipeline::PLANET_PBF_FILENAME);
    let osm_metadata =
        pipeline::read_header(&osm_planet).context("could not read OpenStreetMap provenance")?;

    let run_timestamp = format_rfc3339(UtcDateTime::now());
    let serial_number = Uuid::new_v4().urn().to_string();

    Ok(json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.7",
        "serialNumber": serial_number,
        "version": 1,
        "metadata": {
            "timestamp": run_timestamp,
            "supplier": supplier(),
            "tools": {
                "components": [tool_component()],
            },
            "component": output_component(&run_timestamp),
        },
        "components": [
            atp_component(&atp_metadata),
            osm_component(&osm_metadata),
        ],
        // Declares conflated.parquet's two inputs as *its* dependencies,
        // so the dependency graph has a single root (conflated.parquet)
        // instead of three unlinked ones -- an NTIA-minimum-elements
        // validator flags components no other component depends on as
        // extra roots. Same pattern as scripts/sbom/pipeline.jq uses for
        // the container-image SBOM.
        "dependencies": [{
            "ref": "conflated.parquet",
            "dependsOn": ["alltheplaces.zip", pipeline::PLANET_PBF_FILENAME],
        }],
        "formulation": [formulation(pipeline_run_id)],
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
        "bom-ref": "conflated.parquet",
        "type": "data",
        "name": "conflated.parquet",
        "mime-type": "application/vnd.apache.parquet",
        "version": run_timestamp,
        "description": "Conflated AllThePlaces/OpenStreetMap dataset.",
        // ODbL, not CC0: ODbL's share-alike clause propagates to any
        // produced/derivative work incorporating OpenStreetMap data,
        // regardless of what else (here, CC0-licensed AllThePlaces
        // data) was combined with it.
        "licenses": license("ODbL-1.0"),
        "externalReferences": [
            // TODO(#645): docs/CONFLATED_OUTPUT.md doesn't exist yet --
            // this link is intentionally broken until it's written.
            {"type": "documentation", "url": format!("{REPO_URL}/blob/main/docs/CONFLATED_OUTPUT.md")},
        ],
        "data": [{"type": "dataset"}],
    })
}

fn atp_component(atp: &AtpMetadata) -> Value {
    let start_time = format_rfc3339(atp.start_time);
    json!({
        "bom-ref": "alltheplaces.zip",
        "type": "data",
        "name": "alltheplaces",
        "version": &start_time,
        "supplier": supplier(),
        // Using AllThePlaces data in OpenStreetMap (this pipeline's
        // purpose) was discussed with -- and not objected to by -- the
        // OSM Foundation's Licensing Working Group:
        // https://osmfoundation.org/wiki/Licensing_Working_Group/Minutes/2023-08-14#Ticket%232023081110000064_%E2%80%94_First_party_websites_as_sources
        "licenses": license("CC0-1.0"),
        // TODO(#646): checksum is a placeholder until we compute a real
        // SHA-256 hash of the downloaded zip; the purl is invalid until
        // then, expected to be fixed before this ever runs in
        // production.
        "purl": format!(
            "pkg:generic/alltheplaces.zip@{start_time}?download_url={}&checksum=sha256:TODO",
            atp.output_url
        ),
        "data": [{"type": "dataset", "contents": {"url": atp.output_url}}],
        "externalReferences": [
            {"type": "distribution", "url": atp.output_url},
        ],
    })
}

fn osm_component(osm: &OsmMetadata) -> Value {
    let replication_timestamp = format_rfc3339(osm.replication_timestamp);
    json!({
        // Reference PLANET_PBF_FILENAME rather than a literal, so this
        // can't drift from the file's actual local name (see #648 for a
        // proposal to rename it to match upstream's own convention).
        "bom-ref": pipeline::PLANET_PBF_FILENAME,
        "type": "data",
        "name": pipeline::PLANET_PBF_FILENAME,
        "version": &replication_timestamp,
        "supplier": supplier(),
        "licenses": license("ODbL-1.0"),
        // TODO(#646): checksum is a placeholder until we compute a real
        // SHA-256 hash of the downloaded .osm.pbf; the purl is invalid
        // until then, expected to be fixed before this ever runs in
        // production.
        "purl": format!("pkg:generic/openstreetmap/planet@{replication_timestamp}?checksum=sha256:TODO"),
        "data": [{"type": "dataset"}],
    })
}

/// Ties `tool_component()`, the two input `*_component()`s, and
/// `output_component()` together into one workflow: which tool consumed
/// which inputs to produce which output.
fn formulation(pipeline_run_id: &str) -> Value {
    json!({
        "bom-ref": "formula-osm-diffs-build",
        "workflows": [{
            "bom-ref": "workflow-osm-diffs-build",
            "uid": pipeline_run_id,
            "name": "osm-diffs conflation",
            "taskTypes": ["build"],
            "resourceReferences": [{"ref": "tool-osm-diffs"}],
            "inputs": [
                {"resource": {"ref": "alltheplaces.zip"}},
                {"resource": {"ref": pipeline::PLANET_PBF_FILENAME}},
            ],
            "outputs": [{"resource": {"ref": "conflated.parquet"}}],
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

        let bom = build_bom_for_conflated_parquet(workdir.path(), "k8s-job-42")?;

        assert_eq!(bom["bomFormat"], "CycloneDX");
        assert_eq!(bom["specVersion"], "1.7");
        assert_eq!(bom["version"], 1);
        let serial_number = bom["serialNumber"].as_str().expect("serialNumber");
        assert!(serial_number.starts_with("urn:uuid:"));
        assert!(bom["metadata"]["timestamp"].as_str().is_some());
        assert_eq!(bom["metadata"]["supplier"]["name"], "All The Places");

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
        assert_eq!(output["bom-ref"], "conflated.parquet");
        assert_eq!(output["type"], "data");
        assert_eq!(output["name"], "conflated.parquet");
        assert_eq!(output["mime-type"], "application/vnd.apache.parquet");
        assert!(output["version"].as_str().is_some());
        assert_eq!(output["licenses"][0]["license"]["id"], "ODbL-1.0");

        let components = bom["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);

        let atp = &components[0];
        assert_eq!(atp["bom-ref"], "alltheplaces.zip");
        assert_eq!(atp["type"], "data");
        assert_eq!(atp["name"], "alltheplaces");
        assert_eq!(atp["version"], "2026-03-04T15:16:17Z");
        assert_eq!(atp["supplier"]["name"], "All The Places");
        assert_eq!(atp["licenses"][0]["license"]["id"], "CC0-1.0");
        assert_eq!(
            atp["purl"],
            "pkg:generic/alltheplaces.zip@2026-03-04T15:16:17Z\
             ?download_url=https://example.org/output.zip&checksum=sha256:TODO"
        );
        assert_eq!(
            atp["data"][0]["contents"]["url"],
            "https://example.org/output.zip"
        );

        let osm = &components[1];
        assert_eq!(osm["bom-ref"], pipeline::PLANET_PBF_FILENAME);
        assert_eq!(osm["type"], "data");
        assert_eq!(osm["name"], pipeline::PLANET_PBF_FILENAME);
        assert_eq!(osm["version"], "2026-01-27T08:11:02Z");
        assert_eq!(osm["supplier"]["name"], "All The Places");
        assert_eq!(osm["licenses"][0]["license"]["id"], "ODbL-1.0");
        assert_eq!(
            osm["purl"],
            "pkg:generic/openstreetmap/planet@2026-01-27T08:11:02Z?checksum=sha256:TODO"
        );

        // Single-rooted dependency graph: conflated.parquet depends on
        // both inputs, so neither shows up as an orphan root.
        assert_eq!(bom["dependencies"][0]["ref"], "conflated.parquet");
        let depends_on = bom["dependencies"][0]["dependsOn"]
            .as_array()
            .expect("dependsOn array")
            .iter()
            .map(|v| v.as_str().expect("dependsOn entry"))
            .collect::<Vec<_>>();
        assert_eq!(
            depends_on,
            vec!["alltheplaces.zip", pipeline::PLANET_PBF_FILENAME]
        );

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
            "conflated.parquet",
            "alltheplaces.zip",
            pipeline::PLANET_PBF_FILENAME,
        ];
        let formulation = &bom["formulation"][0]["workflows"][0];
        assert_eq!(formulation["uid"], "k8s-job-42");
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
            "conflated.parquet"
        );

        Ok(())
    }

    #[test]
    fn test_build_defaults_workflow_uid_to_empty_string() -> Result<()> {
        // No --run_id given (e.g. a local/interactive run): uid is the
        // empty string, not omitted or fabricated.
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

        let bom = build_bom_for_conflated_parquet(workdir.path(), "")?;
        assert_eq!(bom["formulation"][0]["workflows"][0]["uid"], "");
        Ok(())
    }

    #[test]
    fn test_build_missing_atp_metadata() {
        let workdir = TempDir::new().expect("tempdir");
        assert!(build_bom_for_conflated_parquet(workdir.path(), "").is_err());
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

        let first = build_bom_for_conflated_parquet(workdir.path(), "")?;
        let second = build_bom_for_conflated_parquet(workdir.path(), "")?;
        assert_ne!(first["serialNumber"], second["serialNumber"]);
        Ok(())
    }
}
