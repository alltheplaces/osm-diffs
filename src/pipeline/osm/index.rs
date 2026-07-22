use super::{BlobReader, Prunings};
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    tables::{StringCounts, StringPool},
};
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock};
use rayon::prelude::*;
use std::{fs::File, path::Path, sync::mpsc::sync_channel, thread};

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
        index_nodes(osm, prunings, &strings, progress, workdir)?;
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
    _workdir: &Path,
) -> Result<()> {
    let progress_bar = make_progress_bar(
        progress,
        "osm.index.nodes  ",
        osm.count_node_blobs() as u64,
        "nodes",
    );
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let producer = s.spawn(|| osm.send_node_blobs(blob_tx));

        let keep_nodes = &prunings.keep_nodes;
        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Node(node) = primitive
                        && keep_nodes.contains(node.id)
                    {
                        // Handle geometry.
                        let s2_lat_lon = s2::latlng::LatLng::from_degrees(node.lat, node.lon);
                        let _s2_cell_id = s2::cellid::CellID::from(s2_lat_lon);

                        // Handle tags.
                        let mut mask = MatchMask::default();
                        for (key, value) in node.tags.iter() {
                            mask.add_tag(key, value);
                            let _key_id = strings.lookup(key).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap node/{} tag key not in StringPool: \"{}\"",
                                    node.id, key
                                )
                            });
                            let _value_id = strings.lookup(value).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap node/{} tag value not in StringPool: \"{}\"",
                                    node.id, value
                                )
                            });
                        }

                        // TODO: Encode as proto message. Sort by s2_cell_id.
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;
        Ok(())
    })?;
    progress_bar.finish();
    Ok(())
}
