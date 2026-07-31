use super::{BlobReader, Prunings};
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    pipeline::osm::{
        geometry::{build_line, build_points, build_ring},
        id_tagging_schema::is_area,
    },
    tables::{Feature, FeatureToIndex, RecordReader, RecordWriter, StringCounts, StringPool},
};
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use geo::{Centroid, Geometry, InterpolateLine, algorithm::line_measures::Haversine};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use prost::Message;
use rayon::prelude::*;
use s2::{cellid::CellID, cellunion::CellUnion};
use std::{fs::File, path::Path, sync::mpsc::sync_channel, thread};
use wkb::writer::{write_geometry, write_point};

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
        let _ways = index_ways(osm, prunings, &strings, progress, workdir)?;
        let _relations = index_relations(osm, prunings, &strings, progress, workdir)?;
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
        "osm.index.strings  ",
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

const WKB_WRITE_OPTIONS: wkb::writer::WriteOptions = wkb::writer::WriteOptions {
    endianness: wkb::Endianness::LittleEndian,
};

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
        "osm.index.nodes    ",
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
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Node(node) = primitive
                        && keep_nodes.contains(node.id)
                        && let Some(ref info) = node.info
                        && let Some(version) = info.version
                        && let Some(changeset) = info.changeset
                    {
                        let mut fti = FeatureToIndex::default();
                        let feature = fti.feature.get_or_insert_with(Feature::default);
                        feature.id = 10 * node.id + 1;
                        feature.version = version;
                        feature.changeset = changeset;
                        if let Some(timestamp) = info.timestamp {
                            feature.timestamp = timestamp;
                        }

                        // Handle geometry.
                        let point = geo::Point::new(node.lon, node.lat); // x = longitude, y = latitude
                        write_point(&mut feature.geometry_wkb, &point, &WKB_WRITE_OPTIONS)?;
                        index_geometry(&Geometry::Point(point), &mut fti.s2_cell_id);

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

// TODO: On behalf of index_relations(), this function should
// additionally emit a table with just the geometry of the (relatively
// few, about 5.8 million) ways in prunings.relation_members. This
// table should be indexed by way ID, so that index_relations() can
// quickly retrieve the geometry of all member ways when it constructs
// OGC Simple Features geometry for an OpenStreetMap relation. Note
// that the same way can (at least in theory) simultaneously be a
// member of some multipolygon relation, so its coordinates have to be
// interpreted as a ring, and also be a member of a non-multipolygon
// relation whose member ways need to be stitched to form (say) a long
// line string. Perhaps it will be best to emit just the coordinates
// in sequence from here, so that the consumer (ie., index_relations)
// can figure out how to best construct geometry. We’ll need to think
// about this a little more.
fn index_ways(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<RecordReader> {
    let out_path = workdir.join("osm-index.ways");
    if out_path.exists() {
        return RecordReader::open(&out_path);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.index.ways     ",
        osm.count_way_blobs() as u64,
        "blobs → features",
    );
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (feature_tx, feature_rx) = sync_channel::<Vec<u8>>(1024);
        let producer = s.spawn(|| osm.send_way_blobs(blob_tx));

        let keep_ways = &prunings.keep_ways;
        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Way(way) = primitive
                        && keep_ways.contains(way.id)
                        && let Some(ref info) = way.info
                        && let Some(version) = info.version
                        && let Some(changeset) = info.changeset
                    {
                        let mut fti = FeatureToIndex::default();
                        let feature = fti.feature.get_or_insert_with(Feature::default);
                        feature.id = 10 * way.id + 2;
                        feature.version = version;
                        feature.changeset = changeset;
                        if let Some(timestamp) = info.timestamp {
                            feature.timestamp = timestamp;
                        }

                        // Handle way members, look up their coordinates.
                        let way_members_count = way.refs().count();
                        let way_members = &mut feature.way_members;
                        way_members.reserve(way_members_count);
                        let mut coords = Vec::<geo::Coord>::with_capacity(way_members_count);
                        for node_id in way.refs() {
                            if node_id > 0 {
                                let node_id: u64 = node_id as u64;
                                way_members.push(node_id);
                                if let Some(c) = prunings.coords.get(node_id) {
                                    coords.push(c);
                                }
                            }
                        }
                        let way_members_count = way_members.len();

                        // Construct the geometry, conforming to the OGC Simple Features model.
                        // We try to repair degenerate cases, so the resulting shape can be
                        // a Point, LineString, Polygon, MultiLineString, or MultiPolygon.
                        let is_closed = way_members_count >= 2
                            && way_members[0] == way_members[way_members_count - 1];
                        let geometry = if is_area(is_closed, way.tags()) {
                            build_ring(coords)
                        } else {
                            build_line(coords)
                        };
                        let Some(geometry) = geometry else {
                            continue;
                        };
                        write_geometry(&mut feature.geometry_wkb, &geometry, &WKB_WRITE_OPTIONS)?;
                        index_geometry(&geometry, &mut fti.s2_cell_id);

                        // Handle tags.
                        let mut mask = MatchMask::default();
                        feature.tags.reserve(way.tags().count() * 2);
                        for (key, value) in way.tags() {
                            mask.add_tag(key, value);
                            let key_id = strings.lookup(key).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap way/{} tag key not in StringPool: \"{}\"",
                                    way.id, key
                                )
                            });
                            feature.tags.push(key_id as u32);
                            let value_id = strings.lookup(value).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap way/{} tag value not in StringPool: \"{}\"",
                                    way.id, value
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

fn index_relations(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<RecordReader> {
    let out_path = workdir.join("osm-index.relations");
    if out_path.exists() {
        return RecordReader::open(&out_path);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.index.relations",
        osm.count_relation_blobs() as u64,
        "blobs → features",
    );
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (feature_tx, feature_rx) = sync_channel::<Vec<u8>>(1024);
        let producer = s.spawn(|| osm.send_relation_blobs(blob_tx));

        let keep_relations = &prunings.keep_relations;
        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(relation) = primitive
                        && keep_relations.contains(relation.id)
                        && let Some(ref info) = relation.info
                        && let Some(version) = info.version
                        && let Some(changeset) = info.changeset
                    {
                        let mut fti = FeatureToIndex::default();
                        let feature = fti.feature.get_or_insert_with(Feature::default);
                        feature.id = 10 * relation.id + 3;
                        feature.version = version;
                        feature.changeset = changeset;
                        if let Some(timestamp) = info.timestamp {
                            feature.timestamp = timestamp;
                        }

                        // Handle member nodes.
                        let mut node_coords = Vec::<geo::Coord>::new();
                        for (_role, id, member_type) in relation.members() {
                            if member_type == RelationMemberType::Node
                                && let Some(c) = prunings.coords.get(id)
                            {
                                node_coords.push(c);
                            }
                        }

                        // TODO: Handle member ways and relations.
                        let geometry = build_points(node_coords);
                        let Some(geometry) = geometry else {
                            continue;
                        };

                        write_geometry(&mut feature.geometry_wkb, &geometry, &WKB_WRITE_OPTIONS)?;
                        index_geometry(&geometry, &mut fti.s2_cell_id);

                        // Handle tags.
                        let mut mask = MatchMask::default();
                        feature.tags.reserve(relation.tags().count() * 2);
                        for (key, value) in relation.tags() {
                            mask.add_tag(key, value);
                            let key_id = strings.lookup(key).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap relation/{} tag key not in StringPool: \"{}\"",
                                    relation.id, key
                                )
                            });
                            feature.tags.push(key_id as u32);
                            let value_id = strings.lookup(value).unwrap_or_else(|| {
                                panic!(
                                    "OpenStreetMap relation/{} tag value not in StringPool: \"{}\"",
                                    relation.id, value
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

// TODO: Replace this by proper s2 cell coverage once polygons and polylines
// are supported in the Rust port of the S2 geometry library. However, note
// that AllThePlaces only cares about small features, so our current approach
// to approximate LineStrings and Polygons as Points is actually not as bad
// as it may seem; when looking for OpenStreetMap features near an AllThePlaces
// feature, we construct an S2 cap (search circle) for several hundred meters
// to a few kilometers depending on the tags in ATP.
fn index_geometry(g: &Geometry, s2_cell_ids: &mut Vec<u64>) {
    s2_cell_ids.clear();
    match g {
        Geometry::Point(p) => {
            s2_cell_ids.reserve(1);
            s2_cell_ids.push(s2_cell_id_for_point(p).0)
        }

        Geometry::MultiPoint(multipoint) => {
            s2_cell_ids.reserve(multipoint.len());
            for p in multipoint.iter() {
                s2_cell_ids.push(s2_cell_id_for_point(p).0);
            }
        }

        Geometry::LineString(line) => {
            s2_cell_ids.reserve(1);
            if let Some(p) = Haversine.point_at_ratio_from_start(line, 0.5) {
                s2_cell_ids.push(s2_cell_id_for_point(&p).0);
            }
        }

        Geometry::MultiLineString(mls) => {
            let mut cell_ids = Vec::<CellID>::with_capacity(mls.0.len());
            for line in mls.iter() {
                if let Some(p) = Haversine.point_at_ratio_from_start(line, 0.5) {
                    cell_ids.push(s2_cell_id_for_point(&p));
                }
            }
            let mut cu = CellUnion(cell_ids);
            cu.normalize();
            s2_cell_ids.reserve(cu.0.len());
            for cell_id in cu.0 {
                s2_cell_ids.push(cell_id.0);
            }
        }

        Geometry::MultiPolygon(mp) => {
            let mut cell_ids = Vec::<CellID>::with_capacity(mp.0.len());
            for poly in mp.iter() {
                if let Some(p) = poly.centroid() {
                    cell_ids.push(s2_cell_id_for_point(&p));
                }
            }
            let mut cu = CellUnion(cell_ids);
            cu.normalize();
            s2_cell_ids.reserve(cu.0.len());
            for cell_id in cu.0 {
                s2_cell_ids.push(cell_id.0);
            }
        }

        _ => {
            if let Some(centroid) = g.centroid() {
                s2_cell_ids.reserve(1);
                s2_cell_ids.push(s2_cell_id_for_point(&centroid).0);
            };
        }
    };
}

/// Helper for [index_geometry].
fn s2_cell_id_for_point(p: &geo::Point) -> CellID {
    let s2_lat_lng = s2::latlng::LatLng::from_degrees(p.y(), p.x());
    s2::cellid::CellID::from(s2_lat_lng)
}
