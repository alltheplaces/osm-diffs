use crate::make_download_bar;
use anyhow::{Context, Ok, Result};
use indicatif::MultiProgress;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::OsmMetadata;

const OSM_TORRENT_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf.torrent";

pub fn fetch_planet(progress: &MultiProgress, workdir: &Path) -> Result<(PathBuf, OsmMetadata)> {
    let pbf_path: PathBuf = workdir.join(super::PLANET_PBF_FILENAME);
    if pbf_path.exists() {
        let metadata = super::read_cached_metadata(workdir).with_context(|| {
            format!(
                "{} exists, but its metadata could not be read",
                pbf_path.display()
            )
        })?;
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
            disable_dht: true, // no distributed hash table
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
