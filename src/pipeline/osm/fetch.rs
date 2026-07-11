use crate::make_download_bar;
use anyhow::{Ok, Result};
use indicatif::MultiProgress;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

const OSM_TORRENT_URL: &str = "https://planet.openstreetmap.org/pbf/planet-latest.osm.pbf.torrent";

pub fn fetch_planet(progress: &MultiProgress, workdir: &Path) -> Result<PathBuf> {
    let pbf_path: PathBuf = workdir.join("osm-planet.pbf");
    if pbf_path.exists() {
        return Ok(pbf_path);
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(download_osm_planet(progress, workdir, &pbf_path))?;

    Ok(pbf_path)
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
