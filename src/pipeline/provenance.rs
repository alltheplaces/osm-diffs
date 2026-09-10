//! Assembles the provenance of this pipeline's public output: a
//! machine-readable document describing which version of this pipeline,
//! using exactly what input, and at what time, produced a given output
//! file. We use the industry-standard CycloneDX JSON format for this,
//! and embed the document into the output file's own metadata (for
//! Parquet, that's key-value metadata -- see
//! `pipeline::conflate::writer`).
//!
//! Reads each input source's metadata straight back from `workdir` --
//! [`AtpMetadata`] via [`crate::pipeline::read_cached_atp_metadata`],
//! [`OsmMetadata`] via [`crate::pipeline::read_cached_metadata`] -- rather than
//! having it threaded through from `import_atp`/`import_osm`'s return
//! values, so this stays decoupled from however those importers (and
//! whatever indexing they feed into) end up wired together.

use crate::pipeline::{self, AtpMetadata, OsmMetadata};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;
use time::UtcDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// GitHub repository this pipeline is published from -- used to build
/// `metadata.tools.components[0]`'s external references and purl.
const REPO_URL: &str = "https://github.com/brawer/osmdiffs";

/// Canonical license text URL for `ODbL-1.0`, shared with
/// `pipeline::datapackage` (the Frictionless manifest's `licenses`).
pub(crate) const ODBL_URL: &str = "https://opendatacommons.org/licenses/odbl/1-0/";

/// Extra facts a *standalone* BOM file can carry that the copy embedded
/// in `conflated.parquet` cannot: the finished Parquet file's own
/// digests (a file can't hash itself before it's written) and the URL it
/// is published at. Passed to [`build_bom_for_conflated_parquet`] as
/// `Some` only for the sidecar `conflated-<date>-<hash8>.cdx.json`.
pub(crate) struct OutputFileRef<'a> {
    pub sha256: &'a str,
    pub sha512: &'a str,
    pub distribution_url: &'a str,
}

/// The input-anchored timestamp every reproducible artifact keys off:
/// `max(AllThePlaces run start, OSM planet replication)`, read back from
/// the metadata `import_atp`/`import_osm` left in `workdir`. See
/// [`build_bom_for_conflated_parquet`].
pub(crate) fn read_anchor(workdir: &Path) -> Result<UtcDateTime> {
    let atp = pipeline::read_cached_atp_metadata(workdir)
        .context("could not read AllThePlaces provenance")?;
    let osm = pipeline::read_cached_metadata(workdir)
        .context("could not read OpenStreetMap provenance")?;
    Ok(anchor_timestamp(&atp, &osm))
}

/// Standard OSM attribution notice, distinct from the license itself:
/// ODbL requires reproducing this in any produced/derivative work, and
/// that requirement propagates to `conflated.parquet` the same way the
/// license itself does (see `output_component()`).
const OSM_COPYRIGHT: &str = "© OpenStreetMap contributors";

/// Canonical license text URL for `CC0-1.0`, confirmed against the SPDX
/// license list's own `seeAlso` references (the `ODbL-1.0` companion is
/// [`ODBL_URL`], defined above since it's shared with `datapackage`).
const CC0_URL: &str = "https://creativecommons.org/publicdomain/zero/1.0/legalcode";

/// Minutes of the OpenStreetMap Foundation's Licensing Working Group
/// meeting where using AllThePlaces data in OpenStreetMap (this
/// pipeline's purpose) was discussed and not objected to -- see
/// `atp_component()`, which cites this as `evidence` substantiating
/// that the ATP input component's CC0 license is actually compatible
/// with this pipeline's ODbL-licensed output.
const ATP_IN_OSM_LICENSING_DISCUSSION_URL: &str = "https://osmfoundation.org/wiki/Licensing_Working_Group/Minutes/2023-08-14#Ticket%232023081110000064_%E2%80%94_First_party_websites_as_sources";

/// Supplier declared for this BOM and the AllThePlaces component. Copied
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

/// Supplier declared for the OSM input component only -- distinct from
/// `supplier()`: the OpenStreetMap Foundation, not us, supplies the
/// planet dump. Confirmed against openstreetmap.org/copyright ("...
/// licensed under the Open Data Commons Open Database License (ODbL) by
/// the OpenStreetMap Foundation").
fn osm_supplier() -> Value {
    json!({
        "name": "OpenStreetMap Foundation",
        "url": ["https://osmfoundation.org/"]
    })
}

/// A CycloneDX `licenses` array for a single SPDX-recognized license,
/// with a link to its actual text. Pair with
/// `license_external_reference(url)` in the same component's
/// `externalReferences`: the schema's own description of `license.url`
/// asks for that, "for completeness".
fn license(spdx_id: &str, url: &str) -> Value {
    json!([{
        "license": {
            "id": spdx_id,
            "url": url,
            "acknowledgement": "declared",
        }
    }])
}

fn license_external_reference(url: &str) -> Value {
    json!({"type": "license", "url": url})
}

/// Builds the CycloneDX provenance document for `conflated.parquet`,
/// from whatever `import_atp`/`import_osm` already left behind in
/// `workdir`.
///
/// `pipeline_run_id` becomes `formulation[].workflows[].uid` (and its
/// `trigger.uid`) -- the identifier this pipeline invocation was given
/// from outside (e.g. a Kubernetes Job run ID), if any; the empty
/// string when run locally/interactively without one. CycloneDX's `uid`
/// is *not* necessarily a UUID -- unlike `serialNumber` (this BOM
/// document's own identity, which we do mint here), it identifies the
/// actual workflow execution "within its deployment context", something
/// this code has no way to know on its own.
///
/// Every timestamp in the document is anchored to the inputs, not the
/// wall clock: `metadata.timestamp`, the output component's `version`,
/// and the workflow's `timeStart`/`timeEnd` all become
/// [`anchor_timestamp`] (the freshness of the most-recently-updated
/// input). `serialNumber` is a version-5 UUID derived from the two
/// inputs' SHA-256s. So a rebuild from the same cached inputs -- e.g. a
/// Kubernetes retry that re-runs `conflate` on a partially populated
/// volume -- produces a byte-identical BOM. `pipeline_run_id` (from
/// `--run_id`) is the one field that legitimately identifies this
/// particular execution rather than the data.
///
/// `output` is `Some` only when building the standalone sidecar BOM
/// file (see [`OutputFileRef`]); `None` for the copy embedded into the
/// Parquet's own metadata.
pub fn build_bom_for_conflated_parquet(
    workdir: &Path,
    pipeline_run_id: &str,
    output: Option<OutputFileRef<'_>>,
) -> Result<Value> {
    let atp_metadata = pipeline::read_cached_atp_metadata(workdir)
        .context("could not read AllThePlaces provenance")?;
    let osm_metadata = pipeline::read_cached_metadata(workdir)
        .context("could not read OpenStreetMap provenance")?;

    let anchor = format_rfc3339(anchor_timestamp(&atp_metadata, &osm_metadata));
    let serial_number = deterministic_serial_number(&atp_metadata, &osm_metadata)?;

    Ok(json!({
        "bomFormat": "CycloneDX",
        "specVersion": "1.7",
        "serialNumber": serial_number,
        "version": 1,
        "metadata": {
            "timestamp": anchor,
            "supplier": supplier(),
            "tools": {
                "components": [tool_component()],
            },
            "component": output_component(&anchor, output.as_ref()),
        },
        "components": [
            atp_component(&atp_metadata)?,
            osm_component(&osm_metadata)?,
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
        "formulation": [formulation(pipeline_run_id, &anchor, &anchor)],
    }))
}

/// The BOM's single time anchor: the freshness of whichever input was
/// updated most recently. Used for every timestamp in the document
/// instead of the wall clock, so the BOM is reproducible from the cached
/// inputs alone (see [`build_bom_for_conflated_parquet`]).
fn anchor_timestamp(atp: &AtpMetadata, osm: &OsmMetadata) -> UtcDateTime {
    atp.start_time.max(osm.replication_timestamp)
}

/// Fixed, arbitrary namespace for the version-5 (SHA-1) UUID used as the
/// BOM's `serialNumber` -- a constant so the derivation
/// `UUIDv5(namespace, atp_sha256 ":" osm_sha256)` is entirely
/// self-contained. Generated once as `UUIDv5(URL,
/// "https://github.com/alltheplaces/osm-diffs#provenance-bom-serial")`
/// -- the repo's pre-2026-09 path, kept verbatim because the value is
/// frozen.
const SERIAL_NAMESPACE: Uuid = Uuid::from_bytes([
    0x18, 0x68, 0x7b, 0x2b, 0xa4, 0x9c, 0x56, 0x07, 0xb1, 0xe7, 0xf7, 0x40, 0xda, 0xee, 0x83, 0x2f,
]);

/// The BOM's `serialNumber`, derived from the two inputs' SHA-256s so an
/// identical input pair always yields an identically identified BOM --
/// rather than a fresh random v4 UUID per run, which made every BOM
/// (and, with it, `conflated.parquet`) non-reproducible.
fn deterministic_serial_number(atp: &AtpMetadata, osm: &OsmMetadata) -> Result<String> {
    let atp_sha = atp.sha256.as_deref().context(
        "AllThePlaces metadata has no sha256 (workdir from an older osm-diffs version?)",
    )?;
    let osm_sha = osm.sha256.as_deref().context(
        "OpenStreetMap metadata has no sha256 (workdir from an older osm-diffs version?)",
    )?;
    Ok(
        Uuid::new_v5(&SERIAL_NAMESPACE, format!("{atp_sha}:{osm_sha}").as_bytes())
            .urn()
            .to_string(),
    )
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
        "purl": format!("pkg:github/brawer/osmdiffs@{version}"),
        "externalReferences": [
            {"type": "vcs", "url": REPO_URL},
            {"type": "release-notes", "url": format!("{REPO_URL}/releases/tag/{version}")},
        ],
    })
}

/// `conflated.parquet` itself -- the data file this BOM is embedded
/// into, described as data (not as the `osm-diffs` tool that built it,
/// which is `tool_component()` instead).
///
/// `output` (`Some` only for the standalone sidecar BOM) adds the
/// finished file's SHA-256/512 and its published-at URL -- facts the
/// embedded copy can't carry.
fn output_component(anchor: &str, output: Option<&OutputFileRef>) -> Value {
    let mut external_references = vec![
        json!({"type": "documentation", "url": format!("{REPO_URL}/blob/main/docs/outputs/CONFLATED_PARQUET.md")}),
        license_external_reference(ODBL_URL),
    ];
    let mut component = json!({
        "bom-ref": "conflated.parquet",
        "type": "data",
        "name": "conflated.parquet",
        "mime-type": "application/vnd.apache.parquet",
        "version": anchor,
        "description": "Conflated AllThePlaces/OpenStreetMap dataset.",
        // ODbL, not CC0: ODbL's share-alike clause propagates to any
        // produced/derivative work incorporating OpenStreetMap data,
        // regardless of what else (here, CC0-licensed AllThePlaces
        // data) was combined with it. Same reasoning applies to the
        // attribution notice below.
        "licenses": license("ODbL-1.0", ODBL_URL),
        "copyright": OSM_COPYRIGHT,
        "data": [{"type": "dataset"}],
    });
    if let Some(o) = output {
        component["hashes"] = json!([
            {"alg": "SHA-256", "content": o.sha256},
            {"alg": "SHA-512", "content": o.sha512},
        ]);
        external_references.push(json!({"type": "distribution", "url": o.distribution_url}));
    }
    component["externalReferences"] = Value::Array(external_references);
    component
}

fn atp_component(atp: &AtpMetadata) -> Result<Value> {
    let start_time = format_rfc3339(atp.start_time);
    // Always Some by the time this runs: import_atp (which computes it,
    // see AtpMetadata::sha256's doc comment) is a prerequisite pipeline
    // step that always runs before conflate. Erroring rather than
    // falling back to a placeholder if it's ever missing -- e.g. a
    // workdir left over from before this field existed -- matches how
    // AtpMetadata's other fields are already treated: don't build a BOM
    // from data we don't actually trust.
    let sha256 = atp.sha256.as_deref().context(
        "AllThePlaces metadata has no sha256 (workdir from an older osm-diffs version?)",
    )?;
    Ok(json!({
        "bom-ref": "alltheplaces.zip",
        "type": "data",
        "name": "alltheplaces.zip",
        "version": &start_time,
        "supplier": supplier(),
        "licenses": license("CC0-1.0", CC0_URL),
        "hashes": [{"alg": "SHA-256", "content": sha256}],
        "purl": format!(
            "pkg:generic/alltheplaces.zip@{start_time}?download_url={}&checksum=sha256:{sha256}",
            atp.output_url
        ),
        "data": [{"type": "dataset", "contents": {"url": atp.output_url}}],
        "externalReferences": [
            {"type": "distribution", "url": atp.output_url},
            license_external_reference(CC0_URL),
            // Substantiates that this CC0-licensed input is actually
            // clear to use here, despite the pipeline's own output
            // being ODbL-licensed -- see the constant's own doc comment.
            {"type": "evidence", "url": ATP_IN_OSM_LICENSING_DISCUSSION_URL},
        ],
    }))
}

fn osm_component(osm: &OsmMetadata) -> Result<Value> {
    let replication_timestamp = format_rfc3339(osm.replication_timestamp);
    // Always Some by the time this runs: import_osm (which computes it,
    // see OsmMetadata::sha256's doc comment) is a prerequisite pipeline
    // step that always runs before conflate. Erroring rather than
    // falling back to a placeholder if it's ever missing -- e.g. a
    // workdir left over from before this field existed -- matches how
    // atp_component treats AtpMetadata::sha256.
    let sha256 = osm.sha256.as_deref().context(
        "OpenStreetMap metadata has no sha256 (workdir from an older osm-diffs version?)",
    )?;
    Ok(json!({
        // Reference PLANET_PBF_FILENAME rather than a literal, so this
        // can't drift from the file's actual local name (see #648 for a
        // proposal to rename it to match upstream's own convention).
        "bom-ref": pipeline::PLANET_PBF_FILENAME,
        "type": "data",
        "name": pipeline::PLANET_PBF_FILENAME,
        "version": &replication_timestamp,
        "supplier": osm_supplier(),
        "licenses": license("ODbL-1.0", ODBL_URL),
        "copyright": OSM_COPYRIGHT,
        "hashes": [{"alg": "SHA-256", "content": sha256}],
        "purl": format!(
            "pkg:generic/openstreetmap/planet@{replication_timestamp}?checksum=sha256:{sha256}"
        ),
        "data": [{"type": "dataset"}],
        "externalReferences": [
            license_external_reference(ODBL_URL),
        ],
    }))
}

/// Ties `tool_component()`, the two input `*_component()`s, and
/// `output_component()` together into one workflow: which tool consumed
/// which inputs to produce which output.
fn formulation(pipeline_run_id: &str, time_start: &str, time_end: &str) -> Value {
    json!({
        "bom-ref": "formula-osm-diffs-build",
        "workflows": [{
            "bom-ref": "workflow-osm-diffs-build",
            "uid": pipeline_run_id,
            "name": "osm-diffs conflation",
            "taskTypes": ["build"],
            // This pipeline runs as a weekly batch job (see
            // docs/SUPPLY_CHAIN_SECURITY.md), not on manual/ad-hoc
            // invocation -- reuse pipeline_run_id as the trigger's own
            // "uid" too: one trigger firing is one pipeline run here,
            // so there's no separate identifier worth inventing.
            "trigger": {
                "bom-ref": "trigger-osm-diffs-schedule",
                "uid": pipeline_run_id,
                "type": "scheduled",
            },
            "timeStart": time_start,
            "timeEnd": time_end,
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

    fn write_fixtures(workdir: &Path) -> Result<()> {
        fs::write(
            workdir.join("alltheplaces.meta.json"),
            r#"{
                "run_id": "2026-03-04-15-16-17",
                "output_url": "https://example.org/output.zip",
                "history_url": "https://data.alltheplaces.xyz/runs/history.json",
                "start_time": "2026-03-04T15:16:17Z",
                "end_time": "2026-03-04T18:42:03Z",
                "spiders": 3512,
                "total_lines": 3812044,
                "size_bytes": 812345678,
                "sha256": "3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7"
            }"#,
        )?;
        let mut pbf_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        pbf_path.push("tests/test_data/zugerland.osm.pbf");
        std::os::unix::fs::symlink(&pbf_path, workdir.join(pipeline::PLANET_PBF_FILENAME))?;
        // read_cached_metadata reads this sidecar rather than re-hashing
        // zugerland.osm.pbf on every test run; sha256 is that file's
        // real SHA-256 (`shasum -a 256 tests/test_data/zugerland.osm.pbf`),
        // the other fields match test_blob_reader's known values for it.
        fs::write(
            workdir.join(format!("{}.meta.json", pipeline::PLANET_PBF_FILENAME)),
            r#"{
                "replication_timestamp": "2026-01-27T08:11:02Z",
                "source": null,
                "writing_program": "osmx",
                "sha256": "56e12b62871018c7a969c9924bcfb1bdce15676bee706156260f101db809b9e1"
            }"#,
        )?;
        Ok(())
    }

    #[test]
    fn test_build() -> Result<()> {
        let workdir = TempDir::new()?;
        write_fixtures(workdir.path())?;

        let bom = build_bom_for_conflated_parquet(workdir.path(), "k8s-job-42", None)?;

        assert_eq!(bom["bomFormat"], "CycloneDX");
        assert_eq!(bom["specVersion"], "1.7");
        assert_eq!(bom["version"], 1);
        let serial_number = bom["serialNumber"].as_str().expect("serialNumber");
        assert!(serial_number.starts_with("urn:uuid:"));
        // Anchored to the most-recently-updated input (here the ATP run,
        // 2026-03-04, which is later than the OSM snapshot, 2026-01-27),
        // not the wall clock -- so the BOM is reproducible.
        assert_eq!(bom["metadata"]["timestamp"], "2026-03-04T15:16:17Z");
        assert_eq!(bom["metadata"]["supplier"]["name"], "All The Places");

        let tool = &bom["metadata"]["tools"]["components"][0];
        assert_eq!(tool["bom-ref"], "tool-osm-diffs");
        assert_eq!(tool["type"], "application");
        assert_eq!(tool["name"], "osm-diffs");
        assert_eq!(tool["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            tool["purl"],
            format!("pkg:github/brawer/osmdiffs@{}", env!("CARGO_PKG_VERSION"))
        );

        let output = &bom["metadata"]["component"];
        assert_eq!(output["bom-ref"], "conflated.parquet");
        assert_eq!(output["type"], "data");
        assert_eq!(output["name"], "conflated.parquet");
        assert_eq!(output["mime-type"], "application/vnd.apache.parquet");
        assert_eq!(output["version"], "2026-03-04T15:16:17Z");
        assert_eq!(output["licenses"][0]["license"]["id"], "ODbL-1.0");
        assert_eq!(output["licenses"][0]["license"]["url"], ODBL_URL);
        assert_eq!(output["copyright"], OSM_COPYRIGHT);

        let components = bom["components"].as_array().expect("components array");
        assert_eq!(components.len(), 2);

        let atp = &components[0];
        assert_eq!(atp["bom-ref"], "alltheplaces.zip");
        assert_eq!(atp["type"], "data");
        assert_eq!(atp["name"], "alltheplaces.zip");
        assert_eq!(atp["version"], "2026-03-04T15:16:17Z");
        assert_eq!(atp["supplier"]["name"], "All The Places");
        assert_eq!(atp["licenses"][0]["license"]["id"], "CC0-1.0");
        assert_eq!(atp["licenses"][0]["license"]["url"], CC0_URL);
        assert!(atp["copyright"].is_null());
        assert_eq!(atp["hashes"][0]["alg"], "SHA-256");
        assert_eq!(
            atp["hashes"][0]["content"],
            "3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7"
        );
        assert_eq!(
            atp["purl"],
            "pkg:generic/alltheplaces.zip@2026-03-04T15:16:17Z\
             ?download_url=https://example.org/output.zip&checksum=sha256:\
             3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7"
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
        assert_eq!(osm["supplier"]["name"], "OpenStreetMap Foundation");
        assert_eq!(osm["licenses"][0]["license"]["id"], "ODbL-1.0");
        assert_eq!(osm["licenses"][0]["license"]["url"], ODBL_URL);
        assert_eq!(osm["copyright"], OSM_COPYRIGHT);
        assert_eq!(osm["hashes"][0]["alg"], "SHA-256");
        assert_eq!(
            osm["hashes"][0]["content"],
            "56e12b62871018c7a969c9924bcfb1bdce15676bee706156260f101db809b9e1"
        );
        assert_eq!(
            osm["purl"],
            "pkg:generic/openstreetmap/planet@2026-01-27T08:11:02Z?checksum=sha256:\
             56e12b62871018c7a969c9924bcfb1bdce15676bee706156260f101db809b9e1"
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
        let workflow = &bom["formulation"][0]["workflows"][0];
        assert_eq!(workflow["uid"], "k8s-job-42");
        assert_eq!(workflow["trigger"]["type"], "scheduled");
        assert_eq!(workflow["trigger"]["uid"], "k8s-job-42");
        // timeStart/timeEnd are the input anchor too, not a wall-clock
        // span -- the workflow's own `uid` is the only run-specific field.
        assert_eq!(workflow["timeStart"], "2026-03-04T15:16:17Z");
        assert_eq!(workflow["timeEnd"], bom["metadata"]["timestamp"]);
        assert_eq!(workflow["resourceReferences"][0]["ref"], "tool-osm-diffs");
        for input in workflow["inputs"].as_array().expect("inputs") {
            let r = input["resource"]["ref"].as_str().expect("ref");
            assert!(known_refs.contains(&r), "unknown bom-ref {r}");
        }
        assert_eq!(
            workflow["outputs"][0]["resource"]["ref"],
            "conflated.parquet"
        );

        Ok(())
    }

    #[test]
    fn test_build_defaults_workflow_uid_to_empty_string() -> Result<()> {
        // No --run_id given (e.g. a local/interactive run): uid is the
        // empty string, not omitted or fabricated.
        let workdir = TempDir::new()?;
        write_fixtures(workdir.path())?;

        let bom = build_bom_for_conflated_parquet(workdir.path(), "", None)?;
        assert_eq!(bom["formulation"][0]["workflows"][0]["uid"], "");
        assert_eq!(bom["formulation"][0]["workflows"][0]["trigger"]["uid"], "");
        Ok(())
    }

    #[test]
    fn test_build_missing_atp_metadata() {
        let workdir = TempDir::new().expect("tempdir");
        assert!(build_bom_for_conflated_parquet(workdir.path(), "", None).is_err());
    }

    #[test]
    fn test_build_is_reproducible() -> Result<()> {
        // Same inputs -> byte-identical BOM (serialNumber included), so a
        // Kubernetes retry that re-runs `conflate` produces the same
        // `conflated.parquet`. Only `--run_id` legitimately varies.
        let workdir = TempDir::new()?;
        write_fixtures(workdir.path())?;

        let first = build_bom_for_conflated_parquet(workdir.path(), "job-1", None)?;
        let second = build_bom_for_conflated_parquet(workdir.path(), "job-1", None)?;
        assert_eq!(first, second);
        assert!(
            first["serialNumber"]
                .as_str()
                .expect("serialNumber")
                .starts_with("urn:uuid:")
        );

        // A different --run_id changes only the workflow uid, nothing else.
        let other = build_bom_for_conflated_parquet(workdir.path(), "job-2", None)?;
        assert_eq!(other["serialNumber"], first["serialNumber"]);
        assert_eq!(
            other["metadata"]["timestamp"],
            first["metadata"]["timestamp"]
        );
        assert_ne!(
            other["formulation"][0]["workflows"][0]["uid"],
            first["formulation"][0]["workflows"][0]["uid"]
        );
        Ok(())
    }

    #[test]
    fn test_build_with_output_ref_adds_hashes_and_distribution() -> Result<()> {
        let workdir = TempDir::new()?;
        write_fixtures(workdir.path())?;

        let embedded = build_bom_for_conflated_parquet(workdir.path(), "job-1", None)?;
        let (sha256, sha512) = ("aa".repeat(32), "bb".repeat(64));
        let url = "https://osmdiffs.dandelis.ch/data/conflated-20260304-aabbccdd.parquet";
        let standalone = build_bom_for_conflated_parquet(
            workdir.path(),
            "job-1",
            Some(OutputFileRef {
                sha256: &sha256,
                sha512: &sha512,
                distribution_url: url,
            }),
        )?;

        // The embedded copy can't hash the file it lives in.
        assert!(embedded["metadata"]["component"].get("hashes").is_none());

        let component = &standalone["metadata"]["component"];
        assert_eq!(component["hashes"][0]["alg"], "SHA-256");
        assert_eq!(component["hashes"][0]["content"], sha256);
        assert_eq!(component["hashes"][1]["alg"], "SHA-512");
        let distribution = component["externalReferences"]
            .as_array()
            .expect("externalReferences")
            .iter()
            .find(|r| r["type"] == "distribution")
            .expect("a distribution external reference");
        assert_eq!(
            distribution["url"],
            "https://osmdiffs.dandelis.ch/data/conflated-20260304-aabbccdd.parquet"
        );

        // Everything else is unchanged from the embedded BOM.
        assert_eq!(standalone["serialNumber"], embedded["serialNumber"]);
        assert_eq!(
            standalone["metadata"]["timestamp"],
            embedded["metadata"]["timestamp"]
        );
        Ok(())
    }
}
