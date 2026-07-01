//! Generate MapRoulette challenges to fix OpenStreetMap.

use crate::{Match, Place};
use anyhow::{Ok, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// Generates MapRoulette challenges to fix OpenStreetMap.
pub struct MapRouletteWriter {
    add_missing_websites: ChallengeWriter,
    update_websites: ChallengeWriter,
}

impl MapRouletteWriter {
    pub fn output_paths(workdir: &Path) -> Vec<PathBuf> {
        let dir = workdir.join("edits").join("maproulette");
        vec![
            dir.join(Self::ADD_MISSING_WEBSITES_FILENAME),
            dir.join(Self::UPDATE_WEBSITES_FILENAME),
        ]
    }

    pub fn try_new(workdir: &Path) -> Result<MapRouletteWriter> {
        let dir = workdir.join("edits").join("maproulette");
        std::fs::create_dir_all(&dir)?;
        Ok(MapRouletteWriter {
            add_missing_websites: ChallengeWriter::try_new(
                dir.join(Self::ADD_MISSING_WEBSITES_FILENAME),
            )?,
            update_websites: ChallengeWriter::try_new(dir.join(Self::UPDATE_WEBSITES_FILENAME))?,
        })
    }

    pub fn close(self) -> Result<()> {
        self.add_missing_websites.close()?;
        self.update_websites.close()?;
        Ok(())
    }

    pub fn generate_edits(&mut self, m: &Match) -> Result<()> {
        let mut osm_website: Option<&str> = None;
        let mut osm_contact_website: Option<&str> = None;
        if let Some(ref osm) = m.osm {
            for (key, value) in osm.tags.iter() {
                match key.as_str() {
                    "website" => osm_website = Some(value.as_str()),
                    "contact:website" => osm_contact_website = Some(value.as_str()),
                    _ => {}
                }
            }
        }

        let mut atp_website: Option<&str> = None;
        for atp in m.atp_matches.iter() {
            for (key, value) in atp.tags.iter() {
                match key.as_str() {
                    "website" => atp_website = Some(value.as_str()),
                    "contact:website" => atp_website = Some(value.as_str()),
                    _ => {}
                }
            }
        }

	// Generate edits for the "website" tag.
        if let Some(atp_website) = atp_website
            && let Some(ref osm) = m.osm
        {
            if Some(atp_website) == osm_website || Some(atp_website) == osm_contact_website {
                // Website is the same in OSM and ATP. Nothing to edit.
	    } else if osm_website != None {
		self.update_website(osm, "website", atp_website)?;
	    } else if osm_contact_website != None {
		self.update_website(osm, "contact:website", atp_website)?;
            } else if osm_website == None && osm_contact_website == None {
                self.add_missing_website(osm, atp_website)?;
	    }
        }

        Ok(())
    }

    fn add_missing_website(&mut self, _osm: &Place, _website: &str) -> Result<()> {
	// TODO: Implement.
	Ok(())
    }

    fn update_website(&mut self, _osm: &Place, _tag: &str, _website: &str) -> Result<()> {
	// TODO: Implement.
	Ok(())
    }
    
    const ADD_MISSING_WEBSITES_FILENAME: &str = "add-missing-websites.jsonl";
    const UPDATE_WEBSITES_FILENAME: &str = "update-websites.jsonl";
}

/// Writes tasks for a single MapRoulette challenge.
struct ChallengeWriter {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl ChallengeWriter {
    pub fn try_new(path: PathBuf) -> Result<ChallengeWriter> {
        let file = File::create(&Self::tmp_path(&path))?;
        let writer = BufWriter::with_capacity(32768, file);
        Ok(ChallengeWriter { path, writer })
    }

    pub fn close(mut self) -> Result<()> {
        self.writer.flush()?;
        let file = self.writer.into_inner()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&Self::tmp_path(&self.path), self.path)?;
        Ok(())
    }

    fn tmp_path(path: &Path) -> PathBuf {
        let mut p = PathBuf::from(path);
        p.add_extension("tmp");
        p
    }
}
