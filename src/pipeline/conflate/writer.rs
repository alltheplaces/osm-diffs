use anyhow::{Context, Ok, Result};
use arrow::array::{
    ArrayRef, RecordBatch, StructArray,
    builder::{
        BinaryBuilder, ListBuilder, MapBuilder, MapFieldNames, StringBuilder, StructBuilder,
        TimestampMillisecondBuilder, UInt32Builder, UInt64Builder,
    },
};
use arrow_buffer::builder::NullBufferBuilder;
use arrow_schema::{DataType, Field, SchemaRef};
use deepsize::DeepSizeOf;
use geo::Centroid;
use parquet::{
    arrow::{ArrowWriter, arrow_writer::ArrowWriterOptions},
    basic::{Compression, ZstdLevel},
    file::{metadata::KeyValue, properties::WriterProperties},
    schema::types::ColumnPath,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    num::{NonZeroU32, NonZeroU64},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::ConflatedFeature;
use crate::{matchers::OsmCandidate, tables::StringPool, utils::UtcTimestamp};

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
    // atp.fetched -- who fetched this feature and when, mirroring
    // osm.modified's shape.
    atp_fetched_timestamps: TimestampMillisecondBuilder,
    atp_fetched_spiders: StringBuilder,
    atp_geometries: BinaryBuilder,

    osm_present: NullBufferBuilder,
    osm_types: StringBuilder,
    osm_ids: UInt64Builder,
    osm_tags: MapBuilder<StringBuilder, StringBuilder>,
    osm_modified_timestamps: TimestampMillisecondBuilder,
    osm_modified_changesets: UInt64Builder,
    osm_modified_versions: UInt32Builder,
    osm_way_members: ListBuilder<UInt64Builder>,
    osm_relation_members: ListBuilder<StructBuilder>,
    osm_geometries: BinaryBuilder,
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

    pub osm_id: Option<NonZeroU64>,
    osm_modified_timestamp: Option<UtcTimestamp>,
    osm_modified_changeset: Option<NonZeroU64>,
    osm_modified_version: Option<NonZeroU32>,
    osm_tags: Vec<(String, String)>,
    /// `Some` only when `osm_id` is a way -- a way's node references, in
    /// OSM's declared order. `None` for a node/relation, or when there's
    /// no OSM match at all.
    osm_way_members: Option<Vec<u64>>,
    /// `Some` only when `osm_id` is a relation -- `(member_type, member_id,
    /// role)` triples, in OSM's declared order. `None` for a node/way, or
    /// when there's no OSM match at all. `member_type` is `String`, not
    /// `&'static str`, even though `decode_member_id` only ever returns
    /// one of three static strings -- `ParquetRow` derives `Deserialize`
    /// for its external-sort round trip, and a `'static` reference can't
    /// satisfy that derive's implied `'de: 'static` bound.
    osm_relation_members: Option<Vec<(String, u64, String)>>,
    osm_shape_wkb: Vec<u8>,

    atp_spider: Option<String>,
    atp_fetched: Option<UtcTimestamp>,
    atp_tags: Vec<(String, String)>,
    atp_shape_wkb: Vec<u8>,
}

/// Key under which the CycloneDX provenance BOM (see `crate::provenance`)
/// is stored in this file's Parquet key-value metadata.
///
/// Neither CycloneDX nor Parquet defines a convention for this: CycloneDX
/// is transport-agnostic (a BOM is normally a sibling file, an OCI
/// artifact, or an in-toto/SLSA attestation, none of which map to a
/// Parquet key), and Parquet's `key_value_metadata` is just a flat list
/// of arbitrary strings that tools namespace themselves (e.g. GeoParquet
/// uses `geo`, GDAL `gdal:schema`, Spark
/// `org.apache.spark.sql.parquet.row.metadata`, Arrow `ARROW:schema`).
/// `org.cyclonedx.bom` follows that same reverse-DNS-style namespacing.
const PROVENANCE_KEY: &str = "org.cyclonedx.bom";

impl ParquetWriter {
    /// `provenance_bom` is the CycloneDX document from `crate::provenance`,
    /// already serialized to a JSON string -- embedded verbatim as this
    /// file's `PROVENANCE_KEY` key-value metadata.
    pub fn create(
        path: &Path,
        max_rows_per_group: usize,
        provenance_bom: &str,
    ) -> Result<ParquetWriter> {
        let mut tmp_path = PathBuf::from(path);
        tmp_path.add_extension("tmp");
        let schema = SchemaRef::new(schema::build_schema());
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(22)?))
            .set_max_row_group_row_count(Some(max_rows_per_group))
            .set_key_value_metadata(Some(vec![KeyValue::new(
                PROVENANCE_KEY.to_string(),
                provenance_bom.to_string(),
            )]))
            .set_column_bloom_filter_enabled(Self::column_path("atp.fetched.spider"), true)
            .set_column_bloom_filter_enabled(Self::column_path("atp.tags.key_value.key"), true)
            .set_column_bloom_filter_enabled(Self::column_path("atp.tags.key_value.value"), true)
            .set_column_bloom_filter_enabled(Self::column_path("osm.modified.changeset"), true)
            .set_column_bloom_filter_enabled(Self::column_path("osm.id"), true)
            .set_column_bloom_filter_enabled(Self::column_path("osm.tags.key_value.key"), true)
            .set_column_bloom_filter_enabled(Self::column_path("osm.tags.key_value.value"), true)
            .set_column_bloom_filter_enabled(Self::column_path("osm.type"), true)
            .build();
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
            atp_fetched_timestamps: Self::new_timestamp_builder(max_rows_per_group),
            atp_fetched_spiders: StringBuilder::with_capacity(
                /* item_capacity */ max_rows_per_group,
                /* data_capacity */ 16 * 1024,
            ),

            // Most ATP geometries are points, which need 21 bytes in WKB encoding.
            atp_geometries: BinaryBuilder::with_capacity(
                /* item_capacity */ max_rows_per_group,
                /* data_capacity */ max_rows_per_group * 21,
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
            osm_modified_timestamps: Self::new_timestamp_builder(max_rows_per_group),
            osm_modified_changesets: UInt64Builder::with_capacity(max_rows_per_group),
            osm_modified_versions: UInt32Builder::with_capacity(max_rows_per_group),
            osm_way_members: ListBuilder::new(UInt64Builder::new()).with_field(Field::new(
                "item",
                DataType::UInt64,
                /* nullable */ false,
            )),
            osm_relation_members: ListBuilder::new(Self::new_relation_member_struct_builder())
                .with_field(Field::new(
                    "item",
                    DataType::Struct(schema::relation_member_fields()),
                    /* nullable */ false,
                )),

            // Many OSM geometries are points, which need 21 bytes in WKB encoding.
            osm_geometries: BinaryBuilder::with_capacity(
                /* item_capacity */ max_rows_per_group,
                /* data_capacity */ max_rows_per_group * 21,
            ),
        })
    }

    fn column_path(name: &str) -> ColumnPath {
        let parts: Vec<String> = name.split('.').map(String::from).collect();
        ColumnPath::from(parts)
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
        // MapBuilder defaults to a nullable "value" field; nothing here
        // ever actually appends a null value (see `write()` below), so
        // this matches that in the schema too, matching alltheplaces.parquet's
        // "tags" column (see places/writer.rs), which was already
        // non-nullable there.
        .with_values_field(Field::new(
            "value",
            DataType::Utf8,
            /* nullable */ false,
        ))
    }

    fn new_timestamp_builder(capacity: usize) -> TimestampMillisecondBuilder {
        TimestampMillisecondBuilder::with_capacity(capacity).with_timezone("UTC")
    }

    fn new_relation_member_struct_builder() -> StructBuilder {
        StructBuilder::new(
            schema::relation_member_fields(),
            vec![
                Box::new(StringBuilder::new()), // type
                Box::new(UInt64Builder::new()), // id
                Box::new(StringBuilder::new()), // role
            ],
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
            self.atp_fetched_timestamps
                .append_value(to_millis(row.atp_fetched.expect("atp_fetched")));
            self.atp_fetched_spiders.append_value(atp_spider);
            self.atp_geometries.append_value(&row.atp_shape_wkb);
        } else {
            self.atp_present.append_null();
            self.atp_tags.append(false)?;
            self.atp_fetched_timestamps.append_value(0);
            self.atp_fetched_spiders.append_value("");
            self.atp_geometries.append_null();
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

            self.osm_modified_timestamps.append_value(to_millis(
                row.osm_modified_timestamp.expect("osm_modified_timestamp"),
            ));
            self.osm_modified_changesets.append_value(
                row.osm_modified_changeset
                    .expect("osm_modified_changeset")
                    .get(),
            );
            self.osm_modified_versions.append_value(
                row.osm_modified_version
                    .expect("osm_modified_version")
                    .get(),
            );

            match row.osm_way_members {
                Some(way_members) => {
                    for node_id in way_members {
                        self.osm_way_members.values().append_value(node_id);
                    }
                    self.osm_way_members.append(true);
                }
                None => self.osm_way_members.append(false),
            }

            match row.osm_relation_members {
                Some(relation_members) => {
                    for (member_type, member_id, role) in relation_members {
                        let member_builder = self.osm_relation_members.values();
                        member_builder
                            .field_builder::<StringBuilder>(0)
                            .expect("type field builder")
                            .append_value(member_type);
                        member_builder
                            .field_builder::<UInt64Builder>(1)
                            .expect("id field builder")
                            .append_value(member_id);
                        member_builder
                            .field_builder::<StringBuilder>(2)
                            .expect("role field builder")
                            .append_value(role);
                        member_builder.append(true);
                    }
                    self.osm_relation_members.append(true);
                }
                None => self.osm_relation_members.append(false),
            }

            self.osm_geometries.append_value(&row.osm_shape_wkb);
        } else {
            self.osm_present.append_null();
            self.osm_types.append_value("");
            self.osm_ids.append_value(0);
            self.osm_tags.append(false)?;
            self.osm_modified_timestamps.append_value(0);
            self.osm_modified_changesets.append_value(0);
            self.osm_modified_versions.append_value(0);
            self.osm_way_members.append(false);
            self.osm_relation_members.append(false);
            self.osm_geometries.append_null();
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

        let atp_fetched_fields = match atp_fields
            .iter()
            .find(|f| f.name() == "fetched")
            .expect("atp.fetched field")
            .data_type()
        {
            DataType::Struct(fields) => fields.clone(),
            _ => panic!("field \"atp.fetched\" must be DataType::Struct"),
        };
        let atp_fetched_struct = StructArray::new(
            atp_fetched_fields,
            vec![
                Arc::new(self.atp_fetched_timestamps.finish()) as ArrayRef,
                Arc::new(self.atp_fetched_spiders.finish()) as ArrayRef,
            ],
            None, // atp.fetched is never null when atp itself is present
        );

        let atp_struct = StructArray::try_new(
            atp_fields.clone(),
            vec![
                Arc::new(self.atp_tags.finish()) as ArrayRef,
                Arc::new(atp_fetched_struct) as ArrayRef,
                Arc::new(self.atp_geometries.finish()) as ArrayRef,
            ],
            self.atp_present.finish(),
        )?;

        let osm_fields = match self.schema.field_with_name("osm")?.data_type() {
            DataType::Struct(fields) => fields,
            _ => panic!("field \"osm\" must be DataType::Struct"),
        };

        let modified_fields = match osm_fields
            .iter()
            .find(|f| f.name() == "modified")
            .expect("osm.modified field")
            .data_type()
        {
            DataType::Struct(fields) => fields.clone(),
            _ => panic!("field \"osm.modified\" must be DataType::Struct"),
        };
        let modified_struct = StructArray::new(
            modified_fields,
            vec![
                Arc::new(self.osm_modified_timestamps.finish()) as ArrayRef,
                Arc::new(self.osm_modified_changesets.finish()) as ArrayRef,
                Arc::new(self.osm_modified_versions.finish()) as ArrayRef,
            ],
            None, // osm.modified is never null when osm itself is present
        );

        let osm_struct = StructArray::try_new(
            osm_fields.clone(),
            vec![
                Arc::new(self.osm_types.finish()) as ArrayRef,
                Arc::new(self.osm_ids.finish()) as ArrayRef,
                Arc::new(self.osm_tags.finish()) as ArrayRef,
                Arc::new(modified_struct) as ArrayRef,
                Arc::new(self.osm_way_members.finish()) as ArrayRef,
                Arc::new(self.osm_relation_members.finish()) as ArrayRef,
                Arc::new(self.osm_geometries.finish()) as ArrayRef,
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

/// Decodes a `Feature.id`-encoded value (`osm_id * 10 + {1,2,3}`) into
/// `(type, osm_id)`. Same encoding `feature_to_osm_id` in
/// `pipeline::osm::assemble` decodes -- not shared with it (that
/// function is private to a different module, and this crate hasn't
/// consolidated the encode/decode helpers yet, see
/// alltheplaces/osm-diffs#662's discussion) -- but mirrors the `% 10`
/// dispatch already used a few lines below for `osm.type` itself.
fn decode_member_id(id: u64) -> (&'static str, u64) {
    match id % 10 {
        1 => ("node", id / 10),
        2 => ("way", id / 10),
        3 => ("relation", id / 10),
        _ => panic!("relation member id {id} with unexpected last digit"),
    }
}

impl ParquetRow {
    /// Builds a `ParquetRow` from a matched (or unmatched) ATP/OSM pair.
    /// An inherent function rather than a `TryFrom` impl so it can take
    /// `osm_strings`, needed to resolve an OSM `Feature`'s tag ids into
    /// actual strings.
    pub(super) fn from_conflated(
        cf: ConflatedFeature,
        osm_strings: &StringPool,
    ) -> Result<ParquetRow> {
        let atp = cf.atp;
        let osm = cf.osm;

        // Internal sort key. Almost always available for free from `atp`
        // -- `produce_rows` only ever calls this with `atp: Some(..)`,
        // since it iterates ATP places outward, so the `osm`-only branch
        // below is unreached today, kept only so this function stays
        // correct if a caller ever does construct an OSM-only row (e.g.
        // a future "suggest creating this as a new OSM feature" flow).
        // Feature carries no precomputed position the way Place does
        // (see https://github.com/alltheplaces/osm-diffs/issues/662), so
        // that branch has to decode geometry and compute a centroid --
        // fine for a today-unreached path, not fine to do unconditionally.
        let s2_cell_id = if let Some(ref atp) = atp {
            atp.s2_cell_id
        } else if let Some(ref feature) = osm {
            let candidate = OsmCandidate {
                feature,
                strings: osm_strings,
            };
            let centroid = candidate
                .geometry()?
                .centroid()
                .with_context(|| format!("OSM feature {} geometry has no centroid", feature.id))?;
            let ll = s2::latlng::LatLng::from_degrees(centroid.y(), centroid.x());
            s2::cellid::CellID::from(ll).0
        } else {
            anyhow::bail!("ConflatedRow must not have atp and osm both None")
        };
        let Some(s2_cell_id) = NonZeroU64::new(s2_cell_id) else {
            anyhow::bail!("s2_cell_id must not be zero");
        };

        let atp_spider;
        let atp_fetched;
        let atp_shape_wkb;
        let atp_tags;
        if let Some(atp) = atp {
            atp_shape_wkb = wkb(&atp.shape());
            atp_spider = Some(atp.spider);
            atp_fetched = Some(atp.fetched);
            atp_tags = atp.tags;
        } else {
            atp_shape_wkb = Vec::with_capacity(0);
            atp_spider = None;
            atp_fetched = None;
            atp_tags = Vec::with_capacity(0);
        };

        let osm_id;
        let osm_modified_timestamp;
        let osm_modified_changeset;
        let osm_modified_version;
        let osm_way_members;
        let osm_relation_members;
        let osm_shape_wkb;
        let osm_tags;
        if let Some(feature) = osm {
            osm_id = NonZeroU64::new(feature.id);
            osm_modified_timestamp = Some(UtcTimestamp(
                time::UtcDateTime::from_unix_timestamp(i64::try_from(feature.timestamp)?)
                    .with_context(|| {
                        format!(
                            "OSM feature {} has an out-of-range timestamp {}",
                            feature.id, feature.timestamp
                        )
                    })?,
            ));
            osm_modified_changeset = NonZeroU64::new(feature.changeset);
            osm_modified_version = NonZeroU32::new(feature.version);
            osm_tags = feature
                .tags
                .chunks_exact(2)
                .map(|kv| {
                    (
                        osm_strings.get(kv[0] as usize).to_owned(),
                        osm_strings.get(kv[1] as usize).to_owned(),
                    )
                })
                .collect();

            osm_way_members = (feature.id % 10 == 2).then(|| feature.way_members.clone());
            osm_relation_members = (feature.id % 10 == 3).then(|| {
                feature
                    .relation_members
                    .iter()
                    .map(|m| {
                        let (member_type, member_id) = decode_member_id(m.id);
                        let role = osm_strings.get(m.role as usize).to_owned();
                        (member_type.to_owned(), member_id, role)
                    })
                    .collect()
            });

            // Already valid OGC Simple Features WKB -- no reconstruction
            // from a synthetic point needed, unlike the old Place path.
            osm_shape_wkb = feature.geometry_wkb;
        } else {
            osm_id = None;
            osm_modified_timestamp = None;
            osm_modified_changeset = None;
            osm_modified_version = None;
            osm_way_members = None;
            osm_relation_members = None;
            osm_shape_wkb = Vec::with_capacity(0);
            osm_tags = Vec::with_capacity(0);
        };

        Ok(ParquetRow {
            s2_cell_id,
            atp_spider,
            atp_fetched,
            atp_tags,
            atp_shape_wkb,
            osm_id,
            osm_modified_timestamp,
            osm_modified_changeset,
            osm_modified_version,
            osm_tags,
            osm_way_members,
            osm_relation_members,
            osm_shape_wkb,
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

/// Converts to milliseconds since the Unix epoch -- Parquet's TIMESTAMP
/// logical type has no representation for a "seconds" unit (see
/// `arrow_to_parquet_type` in the `parquet` crate: `DataType::Timestamp
/// (TimeUnit::Second, _)` silently produces a bare, unannotated `INT64`
/// column, not a real logical-typed timestamp), so every timestamp
/// column here uses `TimeUnit::Millisecond` and this conversion, even
/// though the underlying data (OSM/ATP timestamps) never has
/// sub-second precision.
fn to_millis(t: UtcTimestamp) -> i64 {
    t.unix_timestamp() * 1000
}

fn wkb(shape: &geo::Geometry<f64>) -> Vec<u8> {
    // Most of our features have point geometry, which uses 21 bytes in WKB encoding.
    let mut buf = Vec::<u8>::with_capacity(21);
    let opts = wkb::writer::WriteOptions {
        endianness: wkb::Endianness::LittleEndian,
    };
    wkb::writer::write_geometry(&mut buf, shape, &opts).expect("wkb encoding failed");
    buf
}

mod schema {
    use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
    use parquet_geospatial::{WkbEdges, WkbMetadata, WkbType};

    const NOT_NULLABLE: bool = false;
    const NULLABLE: bool = true;
    const UNSORTED: bool = false;

    pub fn build_schema() -> Schema {
        let fetched = Field::new_struct(
            "fetched",
            vec![
                new_timestamp_field("timestamp", NOT_NULLABLE),
                Field::new("spider", DataType::Utf8, NOT_NULLABLE),
            ],
            NOT_NULLABLE,
        );

        let atp = Field::new_struct(
            "atp",
            vec![
                new_key_value_field("tags"),
                fetched,
                new_geo_field("geometry", NOT_NULLABLE),
            ],
            NULLABLE,
        );

        let modified = Field::new_struct(
            "modified",
            vec![
                new_timestamp_field("timestamp", NOT_NULLABLE),
                Field::new("changeset", DataType::UInt64, NOT_NULLABLE),
                Field::new("version", DataType::UInt32, NOT_NULLABLE),
            ],
            NOT_NULLABLE,
        );

        let osm = Field::new_struct(
            "osm",
            vec![
                Field::new("type", DataType::Utf8, NOT_NULLABLE),
                Field::new("id", DataType::UInt64, NOT_NULLABLE),
                new_key_value_field("tags"),
                modified,
                Field::new_list(
                    "way_members",
                    Field::new("item", DataType::UInt64, NOT_NULLABLE),
                    NULLABLE,
                ),
                Field::new_list(
                    "relation_members",
                    Field::new(
                        "item",
                        DataType::Struct(relation_member_fields()),
                        NOT_NULLABLE,
                    ),
                    NULLABLE,
                ),
                new_geo_field("geometry", NOT_NULLABLE),
            ],
            NULLABLE,
        );

        Schema::new(vec![atp, osm])
    }

    /// Fields of one `osm.relation_members` list entry: the member's own
    /// `type`/`id` (same encoding as the top-level `osm.type`/`osm.id`,
    /// but describing the *member*, not the relation itself) and its
    /// `role` within the relation (e.g. "outer", "inner", or "" if the
    /// member has no role).
    pub fn relation_member_fields() -> Fields {
        vec![
            Field::new("type", DataType::Utf8, NOT_NULLABLE),
            Field::new("id", DataType::UInt64, NOT_NULLABLE),
            Field::new("role", DataType::Utf8, NOT_NULLABLE),
        ]
        .into()
    }

    fn new_key_value_field(name: &str) -> Field {
        Field::new_map(
            name,
            "key_value",
            Field::new("key", DataType::Utf8, NOT_NULLABLE),
            Field::new("value", DataType::Utf8, NOT_NULLABLE),
            UNSORTED,
            NOT_NULLABLE,
        )
    }

    fn new_geo_field(name: &str, nullable: bool) -> Field {
        let metadata = WkbMetadata::new(Some("EPSG:4326"), Some(WkbEdges::Spherical));
        Field::new(name, DataType::Binary, nullable)
            .with_extension_type(WkbType::new(Some(metadata)))
    }

    fn new_timestamp_field(name: &str, nullable: bool) -> Field {
        Field::new(
            name,
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            nullable,
        )
    }
}
