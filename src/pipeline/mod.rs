use anyhow::{Ok, Result};
use std::path::Path;

mod edits;
mod tiles;
mod upload;

pub fn run_pipeline(http_client: &reqwest::Client, workdir: &Path) -> Result<()> {
    if !workdir.exists() {
        std::fs::create_dir(workdir)?;
    }

    let progress = indicatif::MultiProgress::new();
    let atp = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(crate::atp::import_atp(http_client, &progress, workdir))?;
    let coverage = crate::coverage::build_coverage(&atp, &progress, workdir)?;
    let osm = crate::osm::import_osm(&coverage, &progress, workdir)?;
    let edits = edits::suggest_edits(&coverage, &atp, &osm, &progress, workdir)?;
    let tiles = tiles::render_tiles(&edits, &progress, workdir)?;
    upload::upload_tiles(&tiles, &progress)?;

    Ok(())
}
