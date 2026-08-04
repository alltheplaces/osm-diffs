use super::{BlobReader, Prunings};
use crate::{
    make_progress_bar,
    matchers::MatchMask,
    pipeline::osm::{
        geometry::{GeometryBuilder, build_line, build_ring},
        id_tagging_schema::is_area,
    },
    tables::{
        BlobTable, CoordTable, Feature, FeatureToIndex, GeometryTable, RecordReader, RecordWriter,
        RelationMember, StringCounts, StringPool,
    },
};
use anyhow::{Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::LimitedBufferBuilder};
use geo::{Centroid, Geometry, InterpolateLine, algorithm::line_measures::Haversine};
use indicatif::MultiProgress;
use osm_pbf_iter::{Blob, Primitive, PrimitiveBlock, RelationMemberType};
use prost::Message;
use rayon::prelude::*;
use s2::{cellid::CellID, cellunion::CellUnion};
use std::{
    fs::File,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::sync_channel,
    },
    thread,
};
use wkb::writer::{write_geometry, write_point};

#[allow(unused)]
pub struct Assembly<'a> {
    pub strings: StringPool<'a>,
    pub nodes: RecordReader,
    pub ways: RecordReader,
    pub leaf_relations: RecordReader,
}

pub fn assemble<'a>(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<Assembly<'a>> {
    let strings = assemble_strings(&prunings.strings, progress, workdir)?;
    let nodes = assemble_nodes(osm, prunings, &strings, progress, workdir)?;
    let ways = assemble_ways(osm, prunings, &strings, progress, workdir)?;
    let leaf_relations =
        assemble_leaf_relations(osm, prunings, &strings, &ways, progress, workdir)?;
    Ok(Assembly {
        strings,
        nodes,
        ways: ways.ways,
        leaf_relations: leaf_relations.leaf_relations,
    })
}

fn assemble_strings<'a>(
    strings: &StringCounts,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<StringPool<'a>> {
    let string_pool_path = workdir.join("osm-assemble.strings");
    if string_pool_path.exists() {
        let input_modified = strings.modified()?;
        let output_modified = std::fs::metadata(&string_pool_path)?.modified()?;
        if input_modified <= output_modified {
            return StringPool::open(&string_pool_path);
        }
    }

    let read_progress = make_progress_bar(
        progress,
        "osm.assemble.strings  ",
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

fn assemble_nodes(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<RecordReader> {
    let out_path = workdir.join("osm-assemble.nodes");
    if out_path.exists() {
        return RecordReader::open(&out_path);
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.assemble.nodes",
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
                    {
                        let mut fti = assemble_feature(10 * node.id + 1, &node.info);
                        let point = geo::Point::new(node.lon, node.lat); // x = longitude, y = latitude
                        if let Some(feature) = &mut fti.feature {
                            write_point(&mut feature.geometry_wkb, &point, &WKB_WRITE_OPTIONS)?;
                        }
                        assemble_geometry(&Geometry::Point(point), &mut fti.s2_cell_id);
                        if assemble_tags(node.tags.iter().cloned(), strings, &mut fti) {
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

/// The result of [assemble_ways].
struct AssembledWays<'a> {
    ways: RecordReader,
    ways_in_relations: GeometryTable<'a>,
}

fn assemble_ways<'a>(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<AssembledWays<'a>> {
    let out_path = workdir.join("osm-assemble.ways");
    let ways_in_relations_path = workdir.join("osm-assemble.ways-in-relations");
    if out_path.exists() && ways_in_relations_path.exists() {
        return Ok(AssembledWays {
            ways: RecordReader::open(&out_path)?,
            ways_in_relations: GeometryTable::open(&ways_in_relations_path)?,
        });
    }

    let progress_bar = make_progress_bar(
        progress,
        "osm.assemble.ways",
        osm.count_way_blobs() as u64,
        "blobs → features, geometries",
    );
    let feature_count = AtomicU64::new(0);
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (feature_tx, feature_rx) = sync_channel::<Vec<u8>>(1024);
        let (geometry_tx, geometry_rx) = sync_channel::<(u64, Geometry)>(1024);
        let producer = s.spawn(|| osm.send_way_blobs(blob_tx));
        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Way(way) = primitive {
                        let feature_id = 10 * way.id + 2;
                        let is_interesting = prunings.keep_ways.contains(way.id);
                        let is_relation_member = prunings.relation_members.contains(feature_id);
                        if !is_interesting && !is_relation_member {
                            continue;
                        }

                        let mut fti = assemble_feature(feature_id, &way.info);
                        let Some(feature) = &mut fti.feature else {
                            panic!("missing fti.feature");
                        };

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

                        // Build a WKB (Well-Known Binary) representatino for the way’s
                        // geometry. If the way geometry is needed for a relation, send
                        // it to wkb_writer to build a lookup table from way_id -> WKB.
                        write_geometry(&mut feature.geometry_wkb, &geometry, &WKB_WRITE_OPTIONS)?;
                        if is_relation_member {
                            geometry_tx.send((way.id, geometry.clone()))?;
                        }

                        if is_interesting && assemble_tags(way.tags(), strings, &mut fti) {
                            assemble_geometry(&geometry, &mut fti.s2_cell_id);
                            feature_tx.send(fti.encode_to_vec())?;
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        let feature_writer = s.spawn(|| {
            let mut tmp_path = out_path.clone();
            tmp_path.add_extension("tmp");
            let mut out = RecordWriter::create(&tmp_path)?;
            for f in feature_rx {
                feature_count.fetch_add(1, Ordering::SeqCst);
                out.write(&f)?;
            }
            out.close()?;
            std::fs::rename(&tmp_path, &out_path)?;
            Ok(())
        });

        let geometry_writer = s.spawn(|| {
            GeometryTable::create(geometry_rx.into_iter(), workdir, &ways_in_relations_path)
        });

        feature_writer.join().expect("panic in feature_writer")?;
        geometry_writer.join().expect("panic in geometry_writer")?;
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;
        Ok(())
    })?;

    let result = AssembledWays {
        ways: RecordReader::open(&out_path)?,
        ways_in_relations: GeometryTable::open(&ways_in_relations_path)?,
    };
    progress_bar.finish_with_message(format!(
        "{} features, {} geometries",
        feature_count.load(Ordering::SeqCst),
        result.ways_in_relations.len()
    ));
    Ok(result)
}

/// The result of [assemble_leaf_relations].
struct AssembledLeafRelations<'a> {
    /// Records of FeatureToIndex protos for leaf relations, ready to index.
    leaf_relations: RecordReader,

    /// Geometry of leaf relations that are members in super relations.
    #[allow(unused)]
    leaf_relations_geometry: GeometryTable<'a>,

    /// A BlobTable of Feature protos for super relations, at this stage without geometry.
    #[allow(unused)]
    super_relations: BlobTable<'a>,
}

fn assemble_leaf_relations<'a>(
    osm: &mut BlobReader<File>,
    prunings: &Prunings,
    strings: &StringPool,
    ways: &AssembledWays,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<AssembledLeafRelations<'a>> {
    let leaf_relations_path = workdir.join("osm-assemble.leaf-rels.leaves");
    let leaf_relations_geometry_path = workdir.join("osm-assemble.leaf-rels.geometry");
    let super_relations_path = workdir.join("osm-assemble.leaf-rels.super-rels");
    if leaf_relations_path.exists()
        && leaf_relations_geometry_path.exists()
        && super_relations_path.exists()
    {
        return Ok(AssembledLeafRelations {
            leaf_relations: RecordReader::open(&leaf_relations_path)?,
            leaf_relations_geometry: GeometryTable::open(&leaf_relations_geometry_path)?,
            super_relations: BlobTable::open(&super_relations_path)?,
        });
    }

    let coords = &prunings.coords;
    let leaf_relations_count = AtomicU64::new(0);
    let progress_bar = make_progress_bar(
        progress,
        "osm.assemble.leaf-rels",
        osm.count_relation_blobs() as u64,
        "blobs → features, geometries, super-rels",
    );

    let mut leaf_geometry: Option<GeometryTable> = None;
    let mut super_relations: Option<BlobTable> = None;
    thread::scope(|s| {
        let progress_bar = &progress_bar;
        let num_workers = usize::from(thread::available_parallelism()?);
        let (blob_tx, blob_rx) = sync_channel::<Blob>(num_workers);
        let (feature_tx, feature_rx) = sync_channel::<Vec<u8>>(1024);
        let (geometry_tx, geometry_rx) = sync_channel::<(u64, Geometry)>(512);
        let (super_rel_tx, super_rel_rx) = sync_channel::<(u64, Vec<u8>)>(512);
        let producer = s.spawn(|| osm.send_relation_blobs(blob_tx));

        let consumer = s.spawn(move || {
            blob_rx.into_iter().par_bridge().try_for_each(|blob| {
                let data = blob.into_data(); // decompress
                let block = PrimitiveBlock::parse(&data);
                for primitive in block.primitives() {
                    if let Primitive::Relation(relation) = primitive {
                        let feature_id = 10 * relation.id + 3;
                        let is_interesting = prunings.keep_relations.contains(relation.id);
                        let is_relation_member = prunings.relation_members.contains(feature_id);
                        if !is_interesting && !is_relation_member {
                            continue;
                        }

                        let mut fti = assemble_feature(feature_id, &relation.info);
                        let got_tags = assemble_tags(relation.tags(), strings, &mut fti);
                        assemble_relation_members(&relation, strings, &mut fti);

                        if is_super_relation(&relation)
                            && let Some(feature) = &fti.feature
                        {
                            super_rel_tx.send((relation.id, feature.encode_to_vec()))?;
                            continue;
                        }

                        // Build geometry.
                        let mut geom_builder = GeometryBuilder::new();
                        for (_role, id, member_type) in relation.members() {
                            if let Some(g) = lookup_relation_member_geometry(
                                member_type,
                                id,
                                coords,
                                &ways.ways_in_relations,
                                /* leaf_relations_wkb */ None,
                            ) {
                                geom_builder.add(&g);
                            }
                        }
                        let Some(geometry) = geom_builder.finish() else {
                            continue;
                        };

                        if let Some(feature) = &mut fti.feature {
                            write_geometry(
                                &mut feature.geometry_wkb,
                                &geometry,
                                &WKB_WRITE_OPTIONS,
                            )?;
                            if is_relation_member {
                                geometry_tx.send((relation.id, geometry.clone()))?;
                            }
                        }

                        if is_interesting && got_tags {
                            assemble_geometry(&geometry, &mut fti.s2_cell_id);
                            feature_tx.send(fti.encode_to_vec())?;
                        }
                    }
                }
                progress_bar.inc(1);
                Ok(())
            })
        });

        let leaf_rel_writer = s.spawn(|| {
            let mut tmp_path = leaf_relations_path.clone();
            tmp_path.add_extension("tmp");
            let mut out = RecordWriter::create(&tmp_path)?;
            for f in feature_rx {
                leaf_relations_count.fetch_add(1, Ordering::SeqCst);
                out.write(&f)?;
            }
            out.close()?;
            std::fs::rename(&tmp_path, &leaf_relations_path)?;
            Ok(())
        });

        let super_rel_writer = s.spawn(|| {
            super_relations = Some(BlobTable::create(
                super_rel_rx.into_iter(),
                workdir,
                &super_relations_path,
            )?);
            Ok(())
        });

        let leaf_geometry_writer = s.spawn(|| {
            leaf_geometry = Some(GeometryTable::create(
                geometry_rx.into_iter(),
                workdir,
                &leaf_relations_geometry_path,
            )?);
            Ok(())
        });

        leaf_rel_writer.join().expect("panic in leaf_rel_writer")?;
        super_rel_writer
            .join()
            .expect("panic in super_rel_writer")?;
        leaf_geometry_writer
            .join()
            .expect("panic in leaf_geometry_writer")?;
        consumer.join().expect("panic in consumer")?;
        producer.join().expect("panic in producer")?;
        Ok(())
    })?;

    let leaf_relations = RecordReader::open(&leaf_relations_path)?;
    let leaf_relations_geometry = leaf_geometry.expect("leaf_relations_geometry");
    let super_relations = super_relations.expect("super_relations");

    progress_bar.finish_with_message(format!(
        "{} leaf-rels, {} leaf-rel geometries, {} super-rels",
        leaf_relations_count.load(Ordering::SeqCst),
        leaf_relations_geometry.len(),
        super_relations.len(),
    ));

    Ok(AssembledLeafRelations {
        leaf_relations,
        leaf_relations_geometry,
        super_relations,
    })
}

fn assemble_feature(id: u64, info: &Option<osm_pbf_iter::info::Info<'_>>) -> FeatureToIndex {
    let mut fti = FeatureToIndex::default();
    let feature = fti.feature.get_or_insert_with(Feature::default);
    feature.id = id;
    if let Some(info) = info {
        if let Some(version) = info.version {
            feature.version = version;
        }
        if let Some(changeset) = info.changeset {
            feature.changeset = changeset;
        }
        if let Some(timestamp) = info.timestamp {
            feature.timestamp = timestamp;
        }
    }
    fti
}

// TODO: Replace this by proper s2 cell coverage once polygons and polylines
// are supported in the Rust port of the S2 geometry library. However, note
// that AllThePlaces only cares about small features, so our current approach
// to approximate LineStrings and Polygons as Points is actually not as bad
// as it may seem; when looking for OpenStreetMap features near an AllThePlaces
// feature, we construct an S2 cap (search circle) for several hundred meters
// to a few kilometers depending on the tags in ATP.
fn assemble_geometry(g: &Geometry, s2_cell_ids: &mut Vec<u64>) {
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

/// Helper for [assemble_geometry].
fn s2_cell_id_for_point(p: &geo::Point) -> CellID {
    let s2_lat_lng = s2::latlng::LatLng::from_degrees(p.y(), p.x());
    s2::cellid::CellID::from(s2_lat_lng)
}

/// Helper for [assemble_tags].
fn debug_id_str(id: u64) -> String {
    match id % 10 {
        1 => format!("node/{}", id / 10),
        2 => format!("way/{}", id / 10),
        3 => format!("relation/{}", id / 10),
        _ => format!("unknown-feature-type/{}", id),
    }
}

fn assemble_tags<'a, I>(tags: I, strings: &StringPool, fti: &mut FeatureToIndex) -> bool
where
    I: Iterator<Item = (&'a str, &'a str)> + Clone,
{
    let Some(feature) = &mut fti.feature else {
        panic!("missing fti.feature");
    };
    let mut mask = MatchMask::default();
    feature.tags.reserve(tags.clone().count() * 2);
    for (key, value) in tags {
        mask.add_tag(key, value);
        let key_id = strings.lookup(key).unwrap_or_else(|| {
            panic!(
                "tag \"{}\" of {} not in StringPool",
                key,
                debug_id_str(feature.id),
            )
        });
        feature.tags.push(key_id as u32);

        let value_id = strings.lookup(value).unwrap_or_else(|| {
            panic!(
                "value \"{}\" for tag \"{}\" of {} not in StringPool",
                value,
                key,
                debug_id_str(feature.id),
            )
        });

        feature.tags.push(value_id as u32);
    }

    fti.match_mask = mask.0 as u32;
    !mask.is_empty()
}

fn is_super_relation(rel: &osm_pbf_iter::Relation<'_>) -> bool {
    for (_role, _id, member_type) in rel.members() {
        if member_type == RelationMemberType::Relation {
            return true;
        }
    }
    false
}

fn member_feature_id(member_type: RelationMemberType, id: u64) -> u64 {
    match member_type {
        RelationMemberType::Node => id * 10 + 1,
        RelationMemberType::Way => id * 10 + 2,
        RelationMemberType::Relation => id * 10 + 3,
    }
}

fn assemble_relation_members(
    rel: &osm_pbf_iter::Relation<'_>,
    strings: &StringPool,
    fti: &mut FeatureToIndex,
) {
    let Some(feature) = &mut fti.feature else {
        panic!("missing fti.feature");
    };
    feature.relation_members.reserve(rel.members().count());
    for (role, id, member_type) in rel.members() {
        let role_id = strings.lookup(role).unwrap_or_else(|| {
            panic!(
                "OpenStreetMap relation/{} has a member \
                 whose role \"{}\" is not in StringPool",
                rel.id, role
            );
        });
        let member = RelationMember {
            role: role_id as u32,
            id: member_feature_id(member_type, id),
        };
        feature.relation_members.push(member);
    }
}

fn lookup_relation_member_geometry<'a>(
    member_type: RelationMemberType,
    id: u64,
    coords: &CoordTable,
    ways: &GeometryTable<'a>,
    leaf_relations: Option<&GeometryTable<'a>>,
) -> Option<geo::Geometry> {
    match member_type {
        RelationMemberType::Node => {
            if let Some(c) = coords.get(id) {
                return Some(Geometry::Point(geo::Point::from(c)));
            }
        }

        RelationMemberType::Way => {
            if let Some(way_geometry) = ways.lookup(id) {
                return Some(way_geometry);
            }
        }

        RelationMemberType::Relation => {
            if let Some(leaf_relations) = leaf_relations
                && let Some(rel_geometry) = leaf_relations.lookup(id)
            {
                return Some(rel_geometry);
            }
        }
    }

    None
}
