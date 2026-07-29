use super::{BlobReader, Prunings};
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    tables::{Feature, FeatureToIndex, RecordReader, RecordWriter, StringCounts, StringPool},
};
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock};
use prost::Message;
use rayon::prelude::*;
use std::{fs::File, path::Path, sync::mpsc::sync_channel, thread};
use wkb::writer::write_point;

#[allow(unused)]
pub struct Index<'a> {
    pub strings: StringPool<'a>,
}

impl<'a> Index<'a> {
    pub fn create(
        osm: &mut BlobReader<File>,
        prunings: &Prunings,
        progress: &MultiProgress,
        workdir: &Path,
    ) -> Result<Index<'a>> {
        let strings = index_strings(&prunings.strings, progress, workdir)?;
        let _nodes = index_nodes(osm, prunings, &strings, progress, workdir)?;
        Ok(Index { strings })
    }
}

fn index_strings<'a>(
    strings: &StringCounts,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<StringPool<'a>> {
    let string_pool_path = workdir.join("osm-index.strings");
    if string_pool_path.exists() {
        let input_modified = strings.modified()?;
        let output_modified = std::fs::metadata(&string_pool_path)?.modified()?;
        if input_modified <= output_modified {
            return StringPool::open(&string_pool_path);
        }
    }

    let read_progress = make_progress_bar(
        progress,
        "osm.index.strings",
        strings.len() as u64,
        "strings",
    );
    let sorter: ExternalSorter<(u64, String), std::io::Error, LimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(LimitedBufferBuilder::new(
                4 * 1024 * 1024,
                /* preallocate */ true,
            ))
            .build()?;
    let sorted = sorter.sort_by(
        strings.iter().map(|(text, count)| {
            read_progress.inc(1);
            std::io::Result::Ok((count, String::from(text)))
        }),
        |a, b| b.0.cmp(&a.0),
    )?;
    let write_progress = make_progress_bar(
        progress,
        "– write          ",
        strings.len() as u64,
        "strings",
    );

    let mut iter_result: Result<()> = Ok(());
    let pool = StringPool::create(
        sorted.map_while(|item| {
            if let std::result::Result::Ok((_count, text)) = item {
                write_progress.inc(1);
                Some(text)
            } else {
                iter_result = Err(anyhow::Error::new(item.unwrap_err()));
                None
            }
        }),
        workdir,
        &string_pool_path,
    )?;
    iter_result?;
    read_progress.finish();
    Ok(pool)
}

fn index_nodes(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<RecordReader> {
    let out_path = workdir.join("osm-index.nodes");
    if out_path.exists() {
        return RecordReader::open(&out_path);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.index.nodes  ",
        osm.count_node_blobs() as u64,
        "blobs → features",
    );
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (feature_tx, feature_rx) = sync_channel::<Vec<u8>>(1024);
        let producer = s.spawn(|| osm.send_node_blobs(blob_tx));

        let keep_nodes = &prunings.keep_nodes;
        let consumer = s.spawn(move || {
            let wkb_options = wkb::writer::WriteOptions {
                endianness: wkb::Endianness::LittleEndian,
            };
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Node(node) = primitive
                        && keep_nodes.contains(node.id)
                        && let Some(info) = node.info
                        && let Some(version) = info.version
                        && let Some(changeset) = info.changeset
                    {
                        let mut fti = FeatureToIndex::default();
                        let feature = fti.feature.get_or_insert_with(Feature::default);
                        feature.id = 10 * node.id + 1;
                        feature.version = version;
                        feature.changeset = changeset;

                        // Handle geometry.
                        let point = geo::Point::new(node.lon, node.lat); // x = longitude, y = latitude
                        write_point(&mut feature.geometry_wkb, &point, &wkb_options)?;
                        let s2_lat_lon = s2::latlng::LatLng::from_degrees(node.lat, node.lon);
                        fti.s2_cell_id.reserve(1);
                        fti.s2_cell_id.push(s2::cellid::CellID::from(s2_lat_lon).0);

                        // Handle tags.
                        let mut mask = MatchMask::default();
                        feature.tags.reserve(node.tags.len() * 2);
                        for (key, value) in node.tags.iter() {
                            mask.add_tag(key, value);
                            let key_id = strings.lookup(key).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap node/{} tag key not in StringPool: \"{}\"",
                                    node.id, key
                                )
                            });
                            feature.tags.push(key_id as u32);
                            let value_id = strings.lookup(value).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap node/{} tag value not in StringPool: \"{}\"",
                                    node.id, value
                                )
                            });
                            feature.tags.push(value_id as u32);
                        }

                        if !mask.is_empty() {
                            fti.match_mask = mask.0 as u32;
                            feature_tx.send(fti.encode_to_vec())?;
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        let writer = s.spawn(|| {
            let mut tmp_path = out_path.clone();
            tmp_path.add_extension("tmp");
            let mut out = RecordWriter::create(&tmp_path)?;
            for f in feature_rx {
                out.write(&f)?;
            }
            out.close()?;
            std::fs::rename(&tmp_path, &out_path)?;
            Ok(())
        });

        writer.join().expect("panic in writer")?;
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;
        Ok(())
    })?;
    progress_bar.finish();
    RecordReader::open(&out_path)
}
