//! Logic for proposing edits to OpenStreetMap features given a match.

use crate::Match;
use anyhow::{Ok, Result};
use std::path::{Path, PathBuf};

mod maproulette;
use maproulette::MapRouletteWriter;

pub struct EditWriter {
    // TODO: Also generate work for on-the-ground mappers, integrate with StreetComplete.
    maproulette_writer: MapRouletteWriter,
}

impl EditWriter {
    pub fn output_paths(workdir: &Path) -> Vec<PathBuf> {
        MapRouletteWriter::output_paths(workdir)
    }

    pub fn try_new(workdir: &Path) -> Result<EditWriter> {
        assert!(workdir.exists());
        Ok(EditWriter {
            maproulette_writer: MapRouletteWriter::try_new(workdir)?,
        })
    }

    pub fn close(self) -> Result<()> {
        self.maproulette_writer.close()
    }

    pub fn generate_edits(&mut self, m: &Match) -> Result<()> {
        self.maproulette_writer.generate_edits(m)
    }
}
