use anyhow::{Context, Ok, Result};
use std::{path::Path, time::SystemTime};

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

/// Returns the highest (most recent) last modification time among all paths.
/// If any path does not exist or cannot be accessed, an error is returned.
#[allow(unused)] // TODO: Remove once this function is used in production code.
fn last_modified(paths: &[&Path]) -> Result<SystemTime> {
    if !paths.is_empty() {
        let mut last = modified(paths[0])?;
        for path in &paths[1..] {
            last = last.max(modified(path)?);
        }
        Ok(last)
    } else {
        anyhow::bail!("paths should not be empty")
    }
}

fn modified(path: &Path) -> Result<SystemTime> {
    std::fs::metadata(path)
        .with_context(|| format!("Failed to get metadata for path: {:?}", path))?
        .modified()
        .with_context(|| format!("Failed to get modification time for path: {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ops::Add, time::Duration};
    use tempfile::NamedTempFile;

    #[test]
    fn test_last_modified() -> Result<()> {
        let f0 = NamedTempFile::new()?;
        let f2 = NamedTempFile::new()?;
        let f7 = NamedTempFile::new()?;

        let t0 = SystemTime::now();
        let t2 = t0.add(Duration::new(2, 0));
        let t7 = t0.add(Duration::new(7, 0));

        f0.as_file().set_modified(t0)?;
        f2.as_file().set_modified(t2)?;
        f7.as_file().set_modified(t7)?;

        assert!(last_modified(&[]).is_err());
        assert!(last_modified(&[&Path::new("/no/such/file")]).is_err());

        assert_eq!(last_modified(&[f0.path()])?, t0);
        assert_eq!(last_modified(&[f0.path(), f2.path()])?, t2);
        assert_eq!(last_modified(&[f2.path(), f0.path()])?, t2);
        assert_eq!(last_modified(&[f0.path(), f2.path(), f7.path()])?, t7);
        assert_eq!(last_modified(&[f0.path(), f7.path(), f2.path()])?, t7);
        assert_eq!(last_modified(&[f7.path(), f2.path(), f0.path()])?, t7);

        Ok(())
    }
}
