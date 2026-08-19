use crate::make_download_bar;
use anyhow::{Ok, Result};
use indicatif::MultiProgress;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::OsmMetadata;

const OSM_TORRENT_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf.torrent";

pub fn fetch_planet(progress: &MultiProgress, workdir: &Path) -> Result<(PathBuf, OsmMetadata)> {
    let pbf_path: PathBuf = workdir.join(super::PLANET_PBF_FILENAME);
    if pbf_path.exists() {
        if let Result::Ok(metadata) = super::read_cached_metadata(workdir) {
            return Ok((pbf_path, metadata));
        }
        // The .pbf exists, but its .meta.json sidecar is missing or
        // unreadable -- e.g. a regional extract dropped in for cloud
        // testing (scripts/test-branch-on-hetzner), rather than a file
        // this function downloaded itself. Compute the metadata now, the
        // same way a fresh download does below, instead of failing or
        // re-downloading a file that's already there.
        let metadata = super::compute_and_persist_metadata(&pbf_path, workdir, progress)?;
        return Ok((pbf_path, metadata));
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(download_osm_planet(progress, workdir, &pbf_path))?;

    // Not async: a plain buffered read/hash of the (potentially tens of
    // GB) downloaded file, outside the tokio runtime used for the
    // torrent download above.
    let metadata = super::compute_and_persist_metadata(&pbf_path, workdir, progress)?;

    Ok((pbf_path, metadata))
}

async fn download_osm_planet(
    progress: &MultiProgress,
    workdir: &Path,
    pbf_path: &Path,
) -> Result<()> {
    let session = Session::new_with_opts(
        PathBuf::from(workdir),
        SessionOptions {
            dht: None, // no distributed hash table
            ..Default::default()
        },
    )
    .await?;

    let handle = session
        .add_torrent(
            AddTorrent::from_url(OSM_TORRENT_URL),
            Some(AddTorrentOptions {
                output_folder: Some(workdir.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            }),
        )
        .await?
        .into_handle()
        .ok_or_else(|| anyhow::anyhow!("torrent was already managed"))?;

    // Wait for metadata so we know the final filename and size.
    let (torrent_filename, torrent_size) = loop {
        let size = handle.stats().total_bytes;
        if let Some(name) = handle.name()
            && size > 0
        {
            break (name, size);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let bar = make_download_bar(progress, "osm.fetch     ", Some(torrent_size));
    let progress_task = tokio::spawn({
        let handle = handle.clone();
        let bar = bar.clone();
        async move {
            loop {
                let stats = handle.stats();
                if bar.length() != Some(stats.total_bytes) {
                    bar.set_length(stats.total_bytes);
                }
                bar.set_position(stats.progress_bytes);
                if stats.finished {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    handle.wait_until_completed().await?;
    bar.finish();
    progress_task.abort();
    session.stop().await;

    let downloaded = workdir.join(&torrent_filename);
    if downloaded != pbf_path {
        std::fs::rename(&downloaded, pbf_path)?;
    }

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
        let (returned_path, metadata) = fetch_planet(&progress, workdir.path())?;

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
