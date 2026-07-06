use anyhow::{Ok, Result};
use deepsize::DeepSizeOf;
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
};

use super::ConflatedFeature;

pub struct ParquetWriter {
    #[allow(unused)]
    path: PathBuf,
    last_s2_cell_id: u64,
}

/// A single row in the conflated parquet file.
#[derive(Debug, DeepSizeOf, Deserialize, Serialize)]
pub struct ParquetRow {
    /// Internal sort key. Intentionally not written to our output
    /// parquet file because we don’t want to expose S2 cells to
    /// external clients. For point geometries, this would not be a
    /// big issue, but the algorithm to compute a single S2 cell for
    /// polylines and polygons may change in the future. (At the
    /// moment, we take the centroid, but we should rather leave this
    /// to the S2 library; but the Rust version of S2 does not
    /// implement this yet). We still sort the output by S2 because
    /// spatial sorting gives better compression and higher query
    /// performance with geographic Parquet files.
    s2_cell_id: NonZeroU64,

    osm_id: Option<NonZeroU64>,
    osm_changeset: Option<NonZeroU64>,
    osm_version: Option<NonZeroU32>,
    osm_tags: Vec<(String, String)>,
    atp_spider: Option<String>,
    atp_tags: Vec<(String, String)>,
}

impl ParquetWriter {
    pub fn create(path: &Path) -> Result<ParquetWriter> {
        Ok(ParquetWriter {
            path: PathBuf::from(path),
            last_s2_cell_id: 0,
        })
    }

    pub fn write(&mut self, row: ParquetRow) -> Result<()> {
        let row_s2_cell_id = row.s2_cell_id.get();
        assert!(row_s2_cell_id >= self.last_s2_cell_id);
        self.last_s2_cell_id = row_s2_cell_id;
        Ok(())
    }

    pub fn close(self) -> Result<()> {
        Ok(())
    }
}

impl TryFrom<ConflatedFeature> for ParquetRow {
    type Error = anyhow::Error;
    fn try_from(cf: ConflatedFeature) -> Result<Self, Self::Error> {
        let atp = cf.atp;
        let osm = cf.osm;
        let s2_cell_id = if let Some(ref osm) = osm {
            osm.s2_cell_id
        } else if let Some(ref atp) = atp {
            atp.s2_cell_id
        } else {
            anyhow::bail!("ConflatedRow must not have atp and osm both None")
        };
        let Some(s2_cell_id) = NonZeroU64::new(s2_cell_id) else {
            anyhow::bail!("s2_cell_id must not be zero");
        };

        let atp_spider;
        let atp_tags;
        if let Some(atp) = atp {
            atp_spider = Some(atp.source);
            atp_tags = atp.tags;
        } else {
            atp_spider = None;
            atp_tags = Vec::with_capacity(0);
        };

        let osm_id;
        let osm_changeset;
        let osm_version;
        let osm_tags;
        if let Some(osm) = osm {
            osm_id = osm.osm_id;
            osm_changeset = osm.osm_changeset;
            osm_version = osm.osm_version;
            osm_tags = osm.tags;
        } else {
            osm_id = None;
            osm_changeset = None;
            osm_version = None;
            osm_tags = Vec::with_capacity(0);
        };

        Ok(ParquetRow {
            s2_cell_id,
            atp_spider,
            atp_tags,
            osm_id,
            osm_changeset,
            osm_version,
            osm_tags,
        })
    }
}

impl Ord for ParquetRow {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.s2_cell_id
            .cmp(&other.s2_cell_id)
            // We do not need to look at other OSM properties since OSM IDs are unique.
            .then(self.osm_id.cmp(&other.osm_id))
            .then(self.atp_spider.cmp(&other.atp_spider))
            .then(self.atp_tags.cmp(&other.atp_tags))
    }
}

impl PartialOrd for ParquetRow {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for ParquetRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for ParquetRow {}
