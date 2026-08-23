use crate::make_download_bar;
use anyhow::{Context, Ok, Result, anyhow};
use futures_util::StreamExt;
use indicatif::MultiProgress;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use super::OsmMetadata;

/// Stable "latest" alias: `planet.openstreetmap.org` 302-redirects this
/// to the actual dated filename, currently hosted on a real AWS S3
/// bucket (`osm-planet-eu-central-1.s3.dualstack.eu-central-1.amazonaws.com`,
/// confirmed by hand with `curl -sIL`) -- `reqwest` follows the
/// redirect transparently.
const OSM_PLANET_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf";

/// How long a single chunk read is allowed to sit with no data arriving
/// before this gives up and lets the caller retry (via `fetch_planet`'s
/// resume support) -- not a deadline on the whole, multi-hour transfer,
/// which is why this isn't just `RequestBuilder::timeout()` (that caps
/// the entire request/response cycle, header-to-last-byte, and would
/// guarantee failure for a file this size).
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

pub fn fetch_planet(
    http_client: &reqwest::Client,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<(PathBuf, OsmMetadata)> {
    let pbf_path: PathBuf = workdir.join(super::PLANET_PBF_FILENAME);
    if pbf_path.exists() {
        if let Result::Ok(metadata) = super::read_cached_metadata(workdir) {
            return Ok((pbf_path, metadata));
        }
        // The .pbf exists, but its .meta.json sidecar is missing or
        // unreadable -- e.g. a regional extract dropped in for cloud
        // testing (scripts/test-on-hetzner), rather than a file this
        // function downloaded itself. Compute the metadata now, the
        // same way a fresh download does below, instead of failing or
        // re-downloading a file that's already there.
        let metadata = super::compute_and_persist_metadata(&pbf_path, workdir, progress)?;
        return Ok((pbf_path, metadata));
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(download_osm_planet(http_client, progress, &pbf_path))?;

    // Not async: a plain buffered read/hash of the (potentially tens of
    // GB) downloaded file, outside the tokio runtime used for the
    // download above.
    let metadata = super::compute_and_persist_metadata(&pbf_path, workdir, progress)?;

    Ok((pbf_path, metadata))
}

/// Downloads the planet file over plain HTTPS instead of BitTorrent.
/// BitTorrent made real sense when OSM's planet was mirrored across a
/// loosely-provisioned swarm of volunteer peers; today's distribution
/// is a redirect straight to a well-provisioned cloud object store (see
/// `OSM_PLANET_URL`'s doc comment) that a single HTTP connection can
/// already saturate -- the swarm's resilience bought less than it used
/// to when it ultimately bottoms out at a handful of HTTPS mirrors
/// anyway. Uses `http_client` (the same properly-configured client
/// `import_atp` already uses -- see `main.rs::build_client()`) rather
/// than standing up a separate BitTorrent client with its own,
/// differently-configured HTTP stack for tracker/webseed access.
///
/// Resumable: downloads into `pbf_path` with a `.part` suffix, and
/// picks up from wherever that file left off via an HTTP `Range`
/// request if one already exists on disk (e.g. after a restarted
/// pipeline run) -- S3 advertises `Accept-Ranges: bytes` for this file.
async fn download_osm_planet(
    http_client: &reqwest::Client,
    progress: &MultiProgress,
    pbf_path: &Path,
) -> Result<()> {
    let part_path = pbf_path.with_extension("pbf.part");
    let resume_from = tokio::fs::metadata(&part_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let mut request = http_client.get(OSM_PLANET_URL);
    if resume_from > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("Failed to GET {OSM_PLANET_URL}"))?
        .error_for_status()
        .with_context(|| format!("Server returned error for {OSM_PLANET_URL}"))?;

    // A server that doesn't support (or ignores) the Range request
    // sends the whole file back from byte 0 instead of a 206 -- the
    // status code is the one reliable way to tell "here's the rest"
    // from "here's everything, again" apart.
    let (mut file, already_have) =
        if resume_from > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&part_path)
                .await
                .with_context(|| format!("Failed to open {}", part_path.display()))?;
            (file, resume_from)
        } else {
            let file = tokio::fs::File::create(&part_path)
                .await
                .with_context(|| format!("Failed to create file {}", part_path.display()))?;
            (file, 0)
        };

    let total = response.content_length().map(|len| len + already_have);
    let bar = make_download_bar(progress, "osm.fetch     ", total);
    bar.set_position(already_have);

    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::time::timeout(STALL_TIMEOUT, stream.next())
            .await
            .map_err(|_| anyhow!("no data received for {STALL_TIMEOUT:?}, giving up"))?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.context("Error reading chunk from response stream")?;
        file.write_all(&chunk)
            .await
            .context("Failed to write chunk to disk")?;
        bar.inc(chunk.len() as u64);
    }
    file.flush()
        .await
        .with_context(|| format!("Failed to flush {}", part_path.display()))?;
    bar.finish();

    tokio::fs::rename(&part_path, pbf_path)
        .await
        .with_context(|| {
            format!(
                "Failed to rename {} to {}",
                part_path.display(),
                pbf_path.display()
            )
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::osm::{PLANET_PBF_FILENAME, read_cached_metadata};

    fn test_data_path(filename: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests");
        path.push("test_data");
        path.push(filename);
        path
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::new()
    }

    #[test]
    fn test_fetch_planet_computes_metadata_for_preexisting_pbf_without_sidecar() -> Result<()> {
        // A .pbf dropped into the workdir without its .meta.json sidecar
        // -- e.g. a regional extract fetched for cloud testing, rather
        // than a file this function downloaded itself -- should have its
        // metadata computed on the spot, not trigger a fresh download or
        // a hard error.
        let workdir = tempfile::tempdir()?;
        let pbf_path = workdir.path().join(PLANET_PBF_FILENAME);
        std::fs::copy(test_data_path("zugerland.osm.pbf"), &pbf_path)?;
        assert!(read_cached_metadata(workdir.path()).is_err());

        let progress = MultiProgress::new();
        let client = test_client();
        let (returned_path, metadata) = fetch_planet(&client, &progress, workdir.path())?;

        assert_eq!(returned_path, pbf_path);
        // Must agree with what mod.rs's own test_read_header reports for
        // the same fixture.
        assert_eq!(
            metadata.replication_timestamp,
            time::UtcDateTime::from_unix_timestamp(1769501462)? // 2026-01-27T08:11:02Z
        );
        assert_eq!(metadata.source, None);
        assert_eq!(metadata.writing_program, Some("osmx".to_owned()));
        assert!(metadata.sha256.is_some());

        // The sidecar should now exist on disk, matching what was returned.
        assert_eq!(read_cached_metadata(workdir.path())?, metadata);

        Ok(())
    }
}
