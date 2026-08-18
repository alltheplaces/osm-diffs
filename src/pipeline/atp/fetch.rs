use crate::make_download_bar;
use crate::utils::to_hex;
use anyhow::{Context, Ok, Result, anyhow};
use aws_lc_rs::digest::{Context as DigestContext, SHA256};
use futures_util::StreamExt;
use indicatif::MultiProgress;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::{SignedDuration, UtcDateTime, format_description::well_known::Rfc3339};
use tokio::{fs::File, io::AsyncWriteExt};

pub const ATP_RUN_HISTORY_URL: &str = "https://data.alltheplaces.xyz/runs/history.json";

/// Filename, within `workdir`, of the [`AtpMetadata`] persisted alongside
/// `alltheplaces.zip`. Shared between [`fetch_atp`] (which writes it) and
/// [`read_cached_metadata`] (which reads it back independently, e.g. when
/// assembling this pipeline's provenance BOM).
const META_JSON_FILENAME: &str = "alltheplaces.meta.json";

/// Minimum plausible stats for an AllThePlaces run (see [`AtpMetadata`]).
/// Below these, we treat the run as broken or incomplete and fall back to
/// an older one instead of trusting it. Deliberately loose: real runs are
/// far above these numbers (~3,500 spiders / ~3.8M lines / ~800MB as of
/// mid-2026) -- this only needs to catch a catastrophically broken run,
/// not flag routine week-to-week fluctuation.
const MIN_SPIDERS: u64 = 1_000;
const MIN_TOTAL_LINES: u64 = 500_000;
const MIN_SIZE_BYTES: u64 = 50_000_000;

/// How far back we're willing to fall back -- skipping incomplete or
/// suspiciously small runs -- before giving up and refusing to run the
/// pipeline on stale input.
const MAX_STALENESS: SignedDuration = SignedDuration::weeks(6);

/// Provenance metadata about a single AllThePlaces run, read from
/// `history.json` (see [`ATP_RUN_HISTORY_URL`]). Lets us embed the
/// provenance of our input data into our output files, the same way
/// `OsmMetadata` (see `src/pipeline/osm/mod.rs`) does for OpenStreetMap
/// planet dumps.
///
/// All fields are required: `history.json` is only supposed to list
/// finished runs, and we treat an entry that's missing any of these -- or
/// whose stats look broken, see [`MIN_SPIDERS`] and friends -- the same
/// as an absent entry, falling back to an older run rather than working
/// from partial or suspect data. See [`fetch_latest_run`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AtpMetadata {
    /// AllThePlaces' own identifier for this run, e.g. "2026-03-04-15-16-17".
    pub run_id: String,

    /// URL of the `output.zip` we downloaded for this run.
    pub output_url: String,

    /// URL of the run-history endpoint we queried to discover the run.
    pub history_url: String,

    /// When AllThePlaces started running the spiders for this run.
    #[serde(with = "crate::utils::rfc3339")]
    pub start_time: UtcDateTime,

    /// When AllThePlaces finished running the spiders for this run.
    #[serde(with = "crate::utils::rfc3339")]
    pub end_time: UtcDateTime,

    /// Number of spiders (individual data sources) included in this run.
    pub spiders: u64,

    /// Number of GeoJSON feature lines across all spiders in this run,
    /// as reported by AllThePlaces itself.
    pub total_lines: u64,

    /// Size, in bytes, of `output.zip` as reported by AllThePlaces.
    pub size_bytes: u64,

    /// SHA-256 of the downloaded `output.zip`, as lowercase hex --
    /// computed by us, not reported by AllThePlaces. Unlike every other
    /// field here, this is `None` for a candidate run fresh out of
    /// `history.json` (nothing downloaded yet to hash) and only ever
    /// `Some` once [`fetch_atp`] has actually downloaded and hashed the
    /// file, right before persisting it. Absence isn't a sign of
    /// distrust the way it would be for the other fields (see the
    /// struct doc) -- it's just a different lifecycle stage.
    pub sha256: Option<String>,
}

pub async fn fetch_atp(
    url: &str,
    client: &Client,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<(PathBuf, AtpMetadata)> {
    let out_path: PathBuf = workdir.join("alltheplaces.zip");
    let meta_json_path = workdir.join(META_JSON_FILENAME);
    if out_path.exists() {
        let metadata = read_meta_json(&meta_json_path).await.with_context(|| {
            format!(
                "{} exists, but its metadata could not be read from {}",
                out_path.display(),
                meta_json_path.display()
            )
        })?;
        return Ok((out_path, metadata));
    }

    let tmp_path = workdir.join("alltheplaces.zip.tmp");
    let metadata = download_atp(url, client, progress, &tmp_path, &meta_json_path).await?;
    std::fs::rename(&tmp_path, &out_path)?; // atomic file system operation
    Ok((out_path, metadata))
}

async fn download_atp(
    url: &str,
    client: &Client,
    progress: &MultiProgress,
    dest: &Path,
    meta_json_dest: &Path,
) -> Result<AtpMetadata> {
    let mut file = File::create(dest)
        .await
        .with_context(|| format!("Failed to create file {}", dest.display()))?;

    let mut metadata = fetch_latest_run(client, url).await?;
    let response = client
        .get(&metadata.output_url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("Failed to GET {}", metadata.output_url))?
        .error_for_status()
        .with_context(|| format!("Server returned error for {}", metadata.output_url))?;
    let content_length = response.content_length();
    let mut stream = response.bytes_stream();
    let bar = make_download_bar(progress, "atp.fetch     ", content_length);
    // SHA-256 of the downloaded bytes, via aws-lc-rs -- the same crypto
    // library this crate already uses for TLS (see build_client() in
    // main.rs) -- rather than a second, separate hashing implementation.
    // Note: this build links aws-lc-sys (the general-purpose backend),
    // not aws-lc-fips-sys, so this isn't running through AWS-LC's
    // FIPS 140-3-validated module (see scripts/sbom/pipeline.jq's CBOM
    // entry for that module's own certification) -- just the same
    // non-FIPS-mode library, for one less dependency, not for FIPS
    // compliance.
    let mut hasher = DigestContext::new(&SHA256);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading chunk from response stream")?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("Failed to write chunk to disk")?;
        bar.inc(chunk.len() as u64);
    }
    file.flush()
        .await
        .with_context(|| format!("Failed to flush {}", dest.display()))?;
    metadata.sha256 = Some(to_hex(hasher.finish().as_ref()));
    write_meta_json(&metadata, meta_json_dest).await?;
    bar.finish();
    Ok(metadata)
}

/// Queries `history_url` (`history.json`) and returns the metadata for
/// whichever entry has the latest `start_time` among those that are both
/// complete and pass a basic sanity check (see [`MIN_SPIDERS`] and
/// friends), as long as it isn't older than [`MAX_STALENESS`]. Examines
/// every entry rather than assuming `history.json` lists them in any
/// particular order.
async fn fetch_latest_run(client: &Client, history_url: &str) -> Result<AtpMetadata> {
    let response = client
        .get(history_url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .with_context(|| format!("Failed to GET {history_url}"))?
        .error_for_status()
        .with_context(|| format!("Server returned error for {history_url}"))?;
    let json: serde_json::Value = response.json().await?;
    let entries = json
        .as_array()
        .ok_or_else(|| anyhow!("{} did not return a JSON array", history_url))?;

    let mut rejected: Vec<String> = Vec::new();
    let mut newest: Option<AtpMetadata> = None;
    for entry in entries {
        match parse_run(entry, history_url) {
            std::result::Result::Ok(metadata) => {
                let is_newer = match &newest {
                    None => true,
                    Some(current) => metadata.start_time > current.start_time,
                };
                if is_newer {
                    newest = Some(metadata);
                }
            }
            Err(reason) => {
                log::warn!("Skipping AllThePlaces run entry: {reason}");
                rejected.push(reason.to_string());
            }
        }
    }

    let Some(metadata) = newest else {
        return Err(anyhow!(
            "No usable AllThePlaces run found in {}: {}",
            history_url,
            if rejected.is_empty() {
                "history.json has no entries".to_string()
            } else {
                rejected.join("; ")
            }
        ));
    };

    let age: SignedDuration = UtcDateTime::now() - metadata.end_time;
    if age <= MAX_STALENESS {
        return Ok(metadata);
    }
    Err(anyhow!(
        "newest usable AllThePlaces run ({}) ended {} days ago, past the {}-week staleness \
         limit; other rejected entries: {}",
        metadata.run_id,
        age.whole_days(),
        MAX_STALENESS.whole_weeks(),
        if rejected.is_empty() {
            "(none)".to_string()
        } else {
            rejected.join("; ")
        }
    ))
}

/// Parses and sanity-checks a single `history.json` entry. Returns an
/// explanatory error on any field that's missing, malformed, or
/// suspiciously small; callers collect these across several rejected
/// entries into one combined error message.
fn parse_run(entry: &serde_json::Value, history_url: &str) -> Result<AtpMetadata> {
    let str_field = |key: &str| -> Result<String> {
        entry
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow!("missing '{key}'"))
    };
    let u64_field = |key: &str| -> Result<u64> {
        entry
            .get(key)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("missing '{key}'"))
    };
    let timestamp_field = |key: &str| -> Result<UtcDateTime> {
        let s = str_field(key)?;
        UtcDateTime::parse(&s, &Rfc3339).map_err(|e| anyhow!("invalid '{key}' {s:?}: {e}"))
    };

    let run_id = str_field("run_id")?;
    let output_url = str_field("output_url")?;
    let start_time = timestamp_field("start_time")?;
    let end_time = timestamp_field("end_time")?;
    let spiders = u64_field("spiders")?;
    let total_lines = u64_field("total_lines")?;
    let size_bytes = u64_field("size_bytes")?;

    if spiders < MIN_SPIDERS || total_lines < MIN_TOTAL_LINES || size_bytes < MIN_SIZE_BYTES {
        return Err(anyhow!(
            "run {run_id} looks broken (spiders={spiders}, total_lines={total_lines}, \
             size_bytes={size_bytes})"
        ));
    }

    Ok(AtpMetadata {
        run_id,
        output_url,
        history_url: history_url.to_string(),
        start_time,
        end_time,
        spiders,
        total_lines,
        size_bytes,
        sha256: None,
    })
}

async fn write_meta_json(metadata: &AtpMetadata, dest: &Path) -> Result<()> {
    let mut file = File::create(dest)
        .await
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    let data = serde_json::to_string(metadata)?;
    file.write_all(data.as_bytes()).await?;
    file.flush()
        .await
        .with_context(|| format!("Failed to flush {}", dest.display()))?;
    Ok(())
}

async fn read_meta_json(path: &Path) -> Result<AtpMetadata> {
    let data = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Reads back the [`AtpMetadata`] persisted for a prior `fetch_atp` call
/// in `workdir`. Synchronous, unlike `fetch_atp`/`read_meta_json`
/// themselves: by the time this is useful -- assembling this pipeline's
/// provenance BOM once AllThePlaces has already been fetched, e.g. from
/// `conflate()` -- there's no tokio runtime running, and no need to
/// start one just to read a small JSON file that's already on disk.
pub fn read_cached_metadata(workdir: &Path) -> Result<AtpMetadata> {
    let path = workdir.join(META_JSON_FILENAME);
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("Failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::ProgressDrawTarget;
    use mockito::Server;
    use serde_json::json;
    use tempfile::TempDir;

    /// A run entry, `hours_ago` hours old, that passes every check.
    fn fresh_entry(run_id: &str, output_url: &str, hours_ago: i64) -> serde_json::Value {
        let end_time = UtcDateTime::now() - SignedDuration::hours(hours_ago);
        let start_time = end_time - SignedDuration::hours(3);
        json!({
            "run_id": run_id,
            "output_url": output_url,
            "start_time": start_time.format(&Rfc3339).unwrap(),
            "end_time": end_time.format(&Rfc3339).unwrap(),
            "spiders": MIN_SPIDERS + 1,
            "total_lines": MIN_TOTAL_LINES + 1,
            "size_bytes": MIN_SIZE_BYTES + 1,
        })
    }

    #[tokio::test]
    async fn test_fetch_atp() -> Result<()> {
        let mut server = Server::new_async().await;
        let server_url = server.url();
        let output_url = format!("{}/atp_data", server_url);
        let mock_history_payload = json!([fresh_entry("2026-03-04-15-16-17", &output_url, 2)]);
        let mock_history_url = format!("{}/history", server_url);
        let mock_history = server
            .mock("GET", "/history")
            .with_status(200)
            .with_header("Content-Type", "application/json")
            .with_body(mock_history_payload.to_string().as_bytes())
            .create_async()
            .await;
        let mock_atp_data = server
            .mock("GET", "/atp_data")
            .with_status(200)
            .with_header("Content-Type", "application/zip")
            .with_body(b"data")
            .create_async()
            .await;
        let client = test_client(&server);
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let workdir = TempDir::new()?;
        let (path, metadata) =
            fetch_atp(&mock_history_url, &client, &progress, workdir.path()).await?;
        mock_history.assert_async().await;
        mock_atp_data.assert_async().await;

        assert!(path.exists());
        assert_eq!(tokio::fs::read(&path).await?, b"data");
        // Independently-known SHA-256 of the literal bytes b"data" --
        // verifies the actual digest, not just that some string ended
        // up in the field.
        assert_eq!(
            metadata.sha256.as_deref(),
            Some("3a6eb0790f39ac87c94f3856b2dd2c5d110e6811602261a9a923d3bb23adc8b7")
        );
        assert_eq!(metadata.run_id, "2026-03-04-15-16-17");
        assert_eq!(metadata.output_url, output_url);
        assert_eq!(metadata.history_url, mock_history_url);
        assert_eq!(metadata.spiders, MIN_SPIDERS + 1);
        assert_eq!(metadata.total_lines, MIN_TOTAL_LINES + 1);
        assert_eq!(metadata.size_bytes, MIN_SIZE_BYTES + 1);

        // The metadata must also have been persisted to disk, so it can
        // be recovered on a later run without re-hitting the network
        // (see the `out_path.exists()` branch of `fetch_atp`).
        let meta_json_path = workdir.path().join("alltheplaces.meta.json");
        let meta_json_str = tokio::fs::read_to_string(meta_json_path).await?;
        let persisted: AtpMetadata = serde_json::from_str(&meta_json_str)?;
        assert_eq!(persisted, metadata);

        // Fetching again must not hit the network a second time, and
        // must return the metadata read back from disk.
        let (path2, metadata2) =
            fetch_atp(&mock_history_url, &client, &progress, workdir.path()).await?;
        assert_eq!(path2, path);
        assert_eq!(metadata2, metadata);

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_latest_run() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!([
                    fresh_entry("2017-12-14-02-01-04", "https://example.org/first.zip", 240),
                    fresh_entry("2026-03-18-13-32-34", "https://example.org/last.zip", 2),
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let history_url = server.url();
        let result = fetch_latest_run(&client, &history_url).await.unwrap();
        assert_eq!(result.run_id, "2026-03-18-13-32-34");
        assert_eq!(result.output_url, "https://example.org/last.zip");
        assert_eq!(result.history_url, history_url);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn test_fetch_latest_run_picks_max_start_time_regardless_of_array_order() {
        // Don't just trust history.json to list entries oldest-first:
        // put the more recent one first and make sure it still wins.
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!([
                    fresh_entry("2026-03-18-13-32-34", "https://example.org/newer.zip", 2),
                    fresh_entry("2017-12-14-02-01-04", "https://example.org/older.zip", 240),
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let result = fetch_latest_run(&client, &server.url()).await.unwrap();
        assert_eq!(result.run_id, "2026-03-18-13-32-34");
        assert_eq!(result.output_url, "https://example.org/newer.zip");
    }

    #[tokio::test]
    async fn test_fetch_latest_run_falls_back_on_missing_fields() {
        // The newest entry is missing `end_time` (e.g. a run still in
        // progress, or a future change to history.json); we must fall
        // back to the next-older entry rather than error out or use
        // partial data.
        let mut server = Server::new_async().await;
        let mut incomplete = fresh_entry("2026-03-19-00-00-00", "https://example.org/wip.zip", 1);
        incomplete.as_object_mut().unwrap().remove("end_time");
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!([
                    fresh_entry("2026-03-18-13-32-34", "https://example.org/last.zip", 5),
                    incomplete,
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let result = fetch_latest_run(&client, &server.url()).await.unwrap();
        assert_eq!(result.run_id, "2026-03-18-13-32-34");
    }

    #[tokio::test]
    async fn test_fetch_latest_run_falls_back_on_broken_stats() {
        // The newest entry claims only a single spider ran -- far below
        // MIN_SPIDERS -- so it looks broken; fall back to the older run.
        let mut server = Server::new_async().await;
        let mut broken = fresh_entry("2026-03-19-00-00-00", "https://example.org/broken.zip", 1);
        broken["spiders"] = json!(1);
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!([
                    fresh_entry("2026-03-18-13-32-34", "https://example.org/last.zip", 5),
                    broken,
                ])
                .to_string(),
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let result = fetch_latest_run(&client, &server.url()).await.unwrap();
        assert_eq!(result.run_id, "2026-03-18-13-32-34");
    }

    #[tokio::test]
    async fn test_fetch_latest_run_rejects_stale_run() {
        // The only entry is complete and sane, but ended well past
        // MAX_STALENESS -- refuse rather than silently using stale data.
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!([fresh_entry(
                    "2026-01-01-00-00-00",
                    "https://example.org/old.zip",
                    24 * 70, // 10 weeks ago
                )])
                .to_string(),
            )
            .create_async()
            .await;
        let client = test_client(&server);
        let err = fetch_latest_run(&client, &server.url()).await.unwrap_err();
        assert!(err.to_string().contains("2026-01-01-00-00-00"));
        assert!(err.to_string().contains("staleness limit"));
    }

    #[tokio::test]
    async fn test_fetch_latest_run_missing_output_url() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"other_field": "value"}]"#)
            .create_async()
            .await;

        let client = test_client(&server);
        let err = fetch_latest_run(&client, &server.url()).await.unwrap_err();
        assert!(err.to_string().contains("missing 'run_id'"));
    }

    #[tokio::test]
    async fn test_fetch_latest_run_empty_history() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;

        let client = test_client(&server);
        let err = fetch_latest_run(&client, &server.url()).await.unwrap_err();
        assert!(err.to_string().contains("no entries"));
    }

    fn test_client(_server: &Server) -> Client {
        Client::builder()
            // Mockito runs on 127.0.0.1; no proxy needed.
            .no_proxy()
            .build()
            .expect("failed to build test client")
    }

    #[test]
    fn test_sha256_matches_nist_test_vector() {
        // Exercises the exact same aws_lc_rs::digest call path
        // download_atp() uses (DigestContext::new(&SHA256), .update(),
        // .finish(), to_hex()), against SHA-256's standard "abc" test
        // vector.
        let mut hasher = DigestContext::new(&SHA256);
        hasher.update(b"abc");
        // Expected SHA-256("abc"), as per
        // https://csrc.nist.gov/CSRC/media/Projects/Cryptographic-Standards-and-Guidelines/documents/examples/SHA256.pdf
        assert_eq!(
            to_hex(hasher.finish().as_ref()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
