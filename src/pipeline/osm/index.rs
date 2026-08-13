//! Builds an [OsmFeatureIndex] from an [Assembly]'s four `RecordReader`s.
//!
//! Not yet consumed anywhere -- `import_osm` in `mod.rs` calls this and
//! discards the result, the same way it already discards `Assembly`
//! itself, until `conflate`/`suggest_edits` are ready to use it. This
//! exists so the index-building step gets exercised against real PBF
//! data -- and can be watched with actual production-scale metrics --
//! before anything downstream depends on it.

use super::assemble::Assembly;
use crate::make_progress_bar;
use crate::tables::{FeatureToIndex, OsmFeatureIndex, RecordReader};
use anyhow::{Context, Result};
use indicatif::MultiProgress;
use prost::Message;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::thread;

/// Merges `assembly`'s four `RecordReader`s (nodes, ways, leaf_relations,
/// super_relations) and builds the `OsmFeatureIndex` at `out`.
///
/// Four producer threads -- one per `RecordReader` -- each sequentially
/// decode their own stream and send decoded `FeatureToIndex` records down
/// a shared multi-producer channel; `OsmFeatureIndex::create` drains it.
///
/// Deliberately not `rayon::par_bridge()`: the expensive part (LZ4
/// decompression) happens *inside* each `RecordReader::iter()` call while
/// pulling the next record, which is inherently sequential per stream --
/// bridging a single reader's iterator would just serialize on the
/// decompressor anyway, adding rayon dispatch overhead for no gain. Four
/// independent streams decoding concurrently is the actual parallelism
/// win here. Not optimizing further than that for now -- worth checking
/// whether this is an actual bottleneck first. If it is, sharding
/// `assemble_nodes`/`assemble_ways`'s own `RecordWriter` output into more
/// files would raise the ceiling above four; `leaf_relations`/
/// `super_relations` likely wouldn't benefit the same way, since
/// OpenStreetMap has comparatively few relations.
pub fn build_index<'a>(
    assembly: &Assembly,
    progress: &MultiProgress,
    workdir: &Path,
    out: &Path,
) -> Result<OsmFeatureIndex<'a>> {
    if OsmFeatureIndex::exists(out) {
        return OsmFeatureIndex::open(out);
    }

    let sources: [(&str, &RecordReader); 4] = [
        ("nodes", &assembly.nodes),
        ("ways", &assembly.ways),
        ("leaf_relations", &assembly.leaf_relations),
        ("super_relations", &assembly.super_relations),
    ];
    let total: u64 = sources.iter().map(|(_, r)| r.len() as u64).sum();
    let progress_bar = make_progress_bar(
        progress,
        "osm.index                   ",
        total,
        "features → index",
    );

    let mut create_result: Option<Result<OsmFeatureIndex<'a>>> = None;
    thread::scope(|s| -> Result<()> {
        let (tx, rx) = sync_channel::<FeatureToIndex>(1024);
        let progress_bar = &progress_bar;

        let producers: Vec<_> = sources
            .into_iter()
            .map(|(name, reader)| {
                let tx = tx.clone();
                s.spawn(move || -> Result<()> {
                    for record in reader.iter()? {
                        let bytes = record?;
                        let fti = FeatureToIndex::decode(bytes.as_slice()).with_context(|| {
                            format!("failed to decode a FeatureToIndex record from {name}")
                        })?;
                        tx.send(fti)?;
                        progress_bar.inc(1);
                    }
                    Ok(())
                })
            })
            .collect();
        drop(tx); // so rx.into_iter() ends once every producer's clone is dropped

        create_result = Some(OsmFeatureIndex::create(rx.into_iter(), workdir, out));

        for producer in producers {
            producer.join().expect("panic in producer")?;
        }
        Ok(())
    })?;

    progress_bar.finish();
    create_result.expect("thread::scope runs its closure exactly once")
}
