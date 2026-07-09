use anyhow::{Ok, Result};
use arrow::array::{
    ArrayRef, RecordBatch, StructArray,
    builder::{MapBuilder, MapFieldNames, StringBuilder, UInt32Builder, UInt64Builder},
};
use arrow_buffer::builder::NullBufferBuilder;
use arrow_schema::{DataType, SchemaRef};
use deepsize::DeepSizeOf;
use parquet::{
    arrow::{ArrowWriter, arrow_writer::ArrowWriterOptions},
    file::properties::WriterProperties,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::ConflatedFeature;

pub struct ParquetWriter {
    path: PathBuf,
    tmp_path: PathBuf,
    schema: SchemaRef,
    writer: ArrowWriter<File>,
    last_s2_cell_id: u64,
    rows_in_group: usize,
    max_rows_per_group: usize,

    atp_present: NullBufferBuilder,
    atp_tags: MapBuilder<StringBuilder, StringBuilder>,
    atp_spiders: StringBuilder,

    osm_present: NullBufferBuilder,
    osm_types: StringBuilder,
    osm_ids: UInt64Builder,
    osm_tags: MapBuilder<StringBuilder, StringBuilder>,
    osm_changesets: UInt64Builder,
    osm_versions: UInt32Builder,
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
    pub fn create(path: &Path, max_rows_per_group: usize) -> Result<ParquetWriter> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let schema = SchemaRef::new(schema::build_schema());
        let properties = WriterProperties::builder()
            .set_max_row_group_row_count(Some(max_rows_per_group))
            .build(); // TODO: metadata
        let options = ArrowWriterOptions::new().with_properties(properties);
        let file = File::create(&tmp_path)?;
        let writer = ArrowWriter::try_new_with_options(file, schema.clone(), options)?;
        Ok(ParquetWriter {
            path: PathBuf::from(path),
            tmp_path,
            schema,
            writer,
            last_s2_cell_id: 0,
            rows_in_group: 0,
            max_rows_per_group,

            atp_present: NullBufferBuilder::new(max_rows_per_group),
            atp_tags: Self::new_key_value_map_builder(max_rows_per_group),
            atp_spiders: StringBuilder::with_capacity(
                /* item_capacity */ max_rows_per_group,
                /* data_capacity */ 16 * 1024,
            ),

            osm_present: NullBufferBuilder::new(max_rows_per_group),
            // TODO: Use dictionary instead of string for osm_types?
            osm_types: StringBuilder::with_capacity(
                /* item_capacity */
                max_rows_per_group,
                /* data_capacity */ 1024,
            ),
            osm_ids: UInt64Builder::with_capacity(max_rows_per_group),
            osm_tags: Self::new_key_value_map_builder(max_rows_per_group),
            osm_changesets: UInt64Builder::with_capacity(max_rows_per_group),
            osm_versions: UInt32Builder::with_capacity(max_rows_per_group),
        })
    }

    fn new_key_value_map_builder(capacity: usize) -> MapBuilder<StringBuilder, StringBuilder> {
        MapBuilder::with_capacity(
            Some(MapFieldNames {
                entry: String::from("key_value"),
                key: String::from("key"),
                value: String::from("value"),
            }),
            StringBuilder::with_capacity(
                /* item_capacity */ capacity, /* data_capacity */ capacity,
            ),
            StringBuilder::with_capacity(
                /* item_capacity */ capacity, /* data_capacity */ capacity,
            ),
            capacity,
        )
    }

    pub fn write(&mut self, row: ParquetRow) -> Result<()> {
        let row_s2_cell_id = row.s2_cell_id.get();
        assert!(row_s2_cell_id >= self.last_s2_cell_id);
        self.last_s2_cell_id = row_s2_cell_id;
        if self.rows_in_group >= self.max_rows_per_group {
            self.write_row_group()?;
        }

        if let Some(atp_spider) = row.atp_spider {
            self.atp_present.append_non_null();
            for (key, value) in row.atp_tags.iter() {
                self.atp_tags.keys().append_value(key);
                self.atp_tags.values().append_value(value);
            }
            self.atp_tags.append(true)?;
            self.atp_spiders.append_value(atp_spider);
        } else {
            self.atp_present.append_null();
            self.atp_tags.append(false)?;
            self.atp_spiders.append_value("");
        }

        if let Some(osm_id) = row.osm_id {
            self.osm_present.append_non_null();
            self.osm_types.append_value(match osm_id.get() % 10 {
                1 => "node",
                2 => "way",
                3 => "relation",
                _ => panic!("osm_id {} with unexpected last digit", osm_id.get()),
            });
            self.osm_ids.append_value(osm_id.get() / 10);
            for (key, value) in row.osm_tags.iter() {
                self.osm_tags.keys().append_value(key);
                self.osm_tags.values().append_value(value);
            }
            self.osm_tags.append(true)?;

            self.osm_changesets
                .append_value(row.osm_changeset.expect("osm_changeset").get());
            self.osm_versions
                .append_value(row.osm_version.expect("osm_version").get());
        } else {
            self.osm_present.append_null();
            self.osm_types.append_value("");
            self.osm_ids.append_value(0);
            self.osm_tags.append(false)?;
            self.osm_changesets.append_value(0);
            self.osm_versions.append_value(0);
        }
        self.rows_in_group += 1;
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        if self.rows_in_group > 0 {
            self.write_row_group()?;
        }
        self.writer.close()?;
        std::fs::rename(self.tmp_path, self.path)?;
        Ok(())
    }

    fn write_row_group(&mut self) -> Result<()> {
        let atp_fields = match self.schema.field_with_name("atp")?.data_type() {
            DataType::Struct(fields) => fields,
            _ => panic!("field \"atp\" must be DataType::Struct"),
        };

        let atp_struct = StructArray::try_new(
            atp_fields.clone(),
            vec![
                Arc::new(self.atp_tags.finish()) as ArrayRef,
                Arc::new(self.atp_spiders.finish()) as ArrayRef,
            ],
            self.atp_present.finish(),
        )?;

        let osm_fields = match self.schema.field_with_name("osm")?.data_type() {
            DataType::Struct(fields) => fields,
            _ => panic!("field \"osm\" must be DataType::Struct"),
        };

        let osm_struct = StructArray::try_new(
            osm_fields.clone(),
            vec![
                Arc::new(self.osm_types.finish()) as ArrayRef,
                Arc::new(self.osm_ids.finish()) as ArrayRef,
                Arc::new(self.osm_tags.finish()) as ArrayRef,
                Arc::new(self.osm_changesets.finish()) as ArrayRef,
                Arc::new(self.osm_versions.finish()) as ArrayRef,
            ],
            self.osm_present.finish(),
        )?;

        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(atp_struct) as ArrayRef,
                Arc::new(osm_struct) as ArrayRef,
            ],
        )?;

        self.writer.write(&batch)?;
        self.rows_in_group = 0;
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

mod schema {
    use arrow_schema::{DataType, Field, Schema};

    const NOT_NULLABLE: bool = false;
    const NULLABLE: bool = true;
    const UNSORTED: bool = false;

    pub fn build_schema() -> Schema {
        let atp = Field::new_struct(
            "atp",
            vec![
                new_key_value_field("tags"),
                Field::new("spider", DataType::Utf8, NOT_NULLABLE),
            ],
            NULLABLE,
        );
        let osm = Field::new_struct(
            "osm",
            vec![
                Field::new("type", DataType::Utf8, NOT_NULLABLE),
                Field::new("id", DataType::UInt64, NOT_NULLABLE),
                new_key_value_field("tags"),
                Field::new("changeset", DataType::UInt64, NOT_NULLABLE),
                Field::new("version", DataType::UInt32, NOT_NULLABLE),
            ],
            NULLABLE,
        );
        Schema::new(vec![atp, osm])
    }

    fn new_key_value_field(name: &str) -> Field {
        Field::new_map(
            name,
            "key_value",
            Field::new("key", DataType::Utf8, NOT_NULLABLE),
            Field::new("value", DataType::Utf8, NULLABLE),
            UNSORTED,
            NOT_NULLABLE,
        )
    }
}
