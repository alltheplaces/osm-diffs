use super::Place;
use anyhow::{Ok, Result};
use arrow::{
    array::{ArrayRef, MapArray, StringArray, StructArray, UInt8Array, UInt16Array, UInt64Array},
    buffer::OffsetBuffer,
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    file::properties::{EnabledStatistics, WriterProperties},
};
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub struct ParquetWriter {
    osm: bool,
    schema: Arc<Schema>,
    writer: ArrowWriter<File>,
    places: Vec<Place>,
    num_tags: usize,
}

impl ParquetWriter {
    pub fn try_new(
        batch_size: usize,
        page_size: usize, // 1 MiB is a good value for production data
        osm: bool,
        out: &Path,
    ) -> Result<ParquetWriter> {
        assert!(batch_size > 0);
        let file = File::create(out)?;
        let schema = Arc::new(make_schema(osm));
        let props = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::try_new(15)?))
            .set_statistics_enabled(EnabledStatistics::Page)
            .set_data_page_size_limit(page_size)
            .build();
        let writer = ArrowWriter::try_new(file, schema.clone(), Some(props))?;
        Ok(ParquetWriter {
            osm,
            schema,
            writer,
            places: Vec::with_capacity(batch_size),
            num_tags: 0,
        })
    }

    pub fn write(&mut self, place: Place) -> Result<()> {
        if self.places.len() == self.places.capacity() {
            self.flush()?;
        }
        self.num_tags += place.tags.len();
        self.places.push(place);
        Ok(())
    }

    pub fn close(mut self) -> Result<()> {
        self.flush()?;
        self.writer.close()?;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let mut values = Vec::<Arc<dyn arrow::array::Array>>::with_capacity(6);
        values.push(Arc::new(UInt64Array::from_iter(
            self.places.iter().map(|p| p.s2_cell_id),
        )));
        values.push(Arc::new(UInt16Array::from_iter(
            self.places.iter().map(|p| p.mask.0),
        )));
        if self.osm {
            values.push(Arc::new(StringArray::from_iter_values(
                self.places.iter().map(|_| "osm"),
            )));
            values.push(Arc::new(UInt8Array::from_iter(self.places.iter().map(
                |p| match p.source.as_ref() {
                    "n" => 0_u8,
                    "w" => 1_u8,
                    "r" => 2_u8,
                    _ => panic!("source must be \"n\", \"w\" or \"r\""),
                },
            ))));
            values.push(Arc::new(UInt64Array::from_iter(
                self.places.iter().map(|p| p.osm_id),
            )));
        } else {
            values.push(Arc::new(StringArray::from_iter_values(
                self.places.iter().map(|p| p.source.as_str()),
            )));
        }
        values.push(make_tags(&self.places, self.num_tags));

        let batch = RecordBatch::try_new(self.schema.clone(), values)?;
        self.writer.write(&batch)?;
        self.places.clear();
        self.num_tags = 0;
        Ok(())
    }
}

fn make_tags(places: &[Place], num_tags: usize) -> Arc<MapArray> {
    let mut keys = Vec::with_capacity(num_tags);
    let mut values = Vec::with_capacity(num_tags);
    let mut offsets = Vec::with_capacity(num_tags + 1);
    for place in places.iter() {
        offsets.push(keys.len() as i32);
        for (key, value) in place.tags.iter() {
            keys.push(key.as_ref());
            values.push(value.as_ref());
        }
    }
    offsets.push(keys.len() as i32);

    let keys = StringArray::from(keys);
    let values = StringArray::from(values);
    let offsets = OffsetBuffer::new(offsets.into());

    let struct_array = StructArray::from(vec![
        (
            Arc::new(Field::new("key", DataType::Utf8, false)),
            Arc::new(keys) as ArrayRef,
        ),
        (
            Arc::new(Field::new("value", DataType::Utf8, false)),
            Arc::new(values) as ArrayRef,
        ),
    ]);

    let map_array = MapArray::new(
        Arc::new(Field::new(
            "key_value",
            DataType::Struct(
                vec![
                    Field::new("key", DataType::Utf8, false),
                    Field::new("value", DataType::Utf8, false),
                ]
                .into(),
            ),
            false, // non-nullable
        )),
        offsets,
        struct_array,
        None, // no nulls
        true, // sorted
    );
    Arc::new(map_array)
}

fn make_schema(osm: bool) -> Schema {
    let mut fields = vec![
        Field::new("s2_cell_id", DataType::UInt64, /* nullable */ false),
        Field::new("mask", DataType::UInt16, /* nullable */ false),
        Field::new("source", DataType::Utf8, /* nullable */ false),
    ];
    if osm {
        let mut osm_type_metadata = HashMap::new();
        osm_type_metadata.insert("ENUM_VALUES".to_string(), "node,way,relation".to_string());
        osm_type_metadata.insert("ENUM_TYPE".to_string(), "u8".to_string());
        fields.push(
            Field::new("osm_type", DataType::UInt8, /* nullable */ false)
                .with_metadata(osm_type_metadata),
        );
        fields.push(Field::new(
            "osm_id",
            DataType::UInt64,
            /* nullable */ false,
        ));
    }
    fields.push(Field::new(
        "tags",
        DataType::Map(
            Arc::new(Field::new(
                "key_value",
                DataType::Struct(
                    vec![
                        Field::new("key", DataType::Utf8, /* nullable */ false),
                        Field::new("value", DataType::Utf8, /* nullable */ false),
                    ]
                    .into(),
                ),
                /* nullable */ false,
            )),
            /* sorted */ true,
        ),
        false, // non-nullable
    ));

    Schema::new(fields)
}
