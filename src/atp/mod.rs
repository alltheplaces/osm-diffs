use anyhow::{Context, Ok, Result};
use ext_sort::{ExternalSorter, ExternalSorterBuilder, buffer::mem::MemoryLimitedBufferBuilder};
use geo::algorithm::line_measures::Haversine;
use geo::{InteriorPoint, InterpolateLine, Point};
use geojson::GeoJson;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use memmap2::Mmap;
use piz::ZipArchive;
use rayon::prelude::*;
use reqwest::Client;
use std::fs::{File, rename};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use crate::places::{ParquetWriter, Place};
use crate::{PROGRESS_BAR_STYLE, matchers::MatchMask};

mod fetch;

pub async fn import_atp(
    client: &Client,
    progress: &MultiProgress,
    workdir: &Path,
) -> Result<PathBuf> {
    let out = workdir.join("alltheplaces.parquet");
    if out.exists() {
        return Ok(out);
    }

    let input_zip = fetch::fetch_atp(fetch::ATP_RUN_HISTORY_URL, client, progress, workdir).await?;

    // To avoid deadlock, we must not use Rayon threads here.
    // https://dev.to/sgchris/scoped-threads-with-stdthreadscope-in-rust-163-48f9
    let (tx, rx) = sync_channel(50_000);
    let (ra, rb) = std::thread::scope(|s| {
        let r1 = s.spawn(|| process_places(rx, progress, workdir, &out));
        let r2 = s.spawn(|| process_zip(&input_zip, progress, tx));
        (r1.join().unwrap(), r2.join().unwrap())
    });
    ra?;
    rb?;

    Ok(out)
}

fn process_places(
    places: Receiver<Place>,
    progress: &MultiProgress,
    workdir: &Path,
    out: &Path,
) -> Result<()> {
    let mut tmp = PathBuf::from(out);
    tmp.add_extension("tmp");
    let mut writer = ParquetWriter::try_new(
        /* batch size, in records */ 4 * 1024,
        /* osm */ false,
        &tmp,
    )?;
    let sorter: ExternalSorter<Place, std::io::Error, MemoryLimitedBufferBuilder> =
        ExternalSorterBuilder::new()
            .with_tmp_dir(workdir)
            .with_buffer(MemoryLimitedBufferBuilder::new(150_000_000))
            .build()?;

    let num_features = AtomicU64::new(0);
    let sorted = sorter.sort(places.iter().map(|x| {
        num_features.fetch_add(1, Ordering::SeqCst);
        std::io::Result::Ok(x)
    }))?;
    let num_features = num_features.load(Ordering::SeqCst);

    let bar = progress.add(ProgressBar::new(num_features));
    bar.set_style(ProgressStyle::with_template(PROGRESS_BAR_STYLE)?);
    bar.set_prefix("atp.write     ");
    bar.set_message("features");

    for place in sorted {
        writer.write(place?)?;
        bar.inc(1);
    }
    writer.close()?;
    rename(&tmp, out)?;
    bar.finish_with_message(format!("{} features", num_features));

    log::info!(
        "import_atp finished, built {:?} with {:?} features",
        out,
        num_features
    );
    Ok(())
}

fn process_zip(path: &Path, progress: &MultiProgress, channel: SyncSender<Place>) -> Result<()> {
    let file = File::open(path).with_context(|| format!("could not open file `{:?}`", path))?;
    let mapping = unsafe { Mmap::map(&file).context("Couldn't mmap zip file")? };
    let archive = ZipArchive::with_prepended_data(&mapping)
        .context("Couldn't load archive")?
        .0;
    let bar = progress.add(ProgressBar::new(archive.entries().len() as u64));
    bar.set_prefix("atp.read      ");
    bar.set_style(ProgressStyle::with_template(PROGRESS_BAR_STYLE)?);
    bar.set_message("feature collections");

    archive.entries().par_iter().try_for_each(|entry| {
        if entry.size > 0 {
            let reader = archive.read(entry)?;
            process_geojson(reader, Some(channel.clone()))?;
        }
        bar.inc(1);
        Ok(())
    })?;
    bar.finish();
    Ok(())
}

fn process_geojson<T: Read>(reader: T, channel: Option<SyncSender<Place>>) -> Result<()> {
    let buffer = BufReader::new(reader);
    for line in buffer.lines() {
        let line = line?;

        // Handle start of FeatureCollection, first line in file.
        if line.starts_with("{\"type\":\"FeatureCollection\"") {
            let mut json = line;
            json.push_str("]}");
            let parsed = json.parse::<geojson::FeatureCollection>()?;
            if is_usable_for_osm(&parsed) {
                continue;
            } else {
                // Ignore FeatureCollections that can't be conflated with OSM
                // according to the AllThePlaces metadata.
                return Ok(());
            }
        }

        // Handle end of FeatureCollection, last line in file.
        if line == "]}" {
            continue;
        }

        // Remove any trailing commas.
        let trimmed = if line.ends_with(',') {
            &line[..line.len() - 1]
        } else {
            &line
        };

        // Handle individual features.
        let Some(place) = make_place(trimmed) else {
            continue;
        };
        if let Some(ref channel) = channel {
            channel.send(place)?;
        };
    }
    Ok(())
}

fn is_usable_for_osm(fc: &geojson::FeatureCollection) -> bool {
    let Some(members) = &fc.foreign_members else {
        return false;
    };
    let Some(attrs) = members.get("dataset_attributes") else {
        return false;
    };

    // If AllThePlaces explicitly marks a dataset as OK or Not-OK for OSM,
    // that takes precedence. This is used when a data source has explicitly
    // signed a waiver for OSM, or if negotiations were unsuccessful.
    let use_osm = attrs
        .get("use:openstreetmap")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match use_osm {
        "yes" => return true,
        "no" => return false,
        _ => {}
    }

    // Otherwise, look at the license of the AllThePlaces dataset.
    // https://osmfoundation.org/wiki/Licence/Licence_Compatibility
    let license_wikidata = attrs
        .get("license:wikidata")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match license_wikidata {
        "Q6938433" => return true,    // CC0
        "Q14947546" => return false,  // CC-BY-3.0
        "Q18199165" => return false,  // CC-BY-SA-4.0
        "Q20007257" => return false,  // CC-BY-4.0
        "Q21659044" => return true,   // Unlicense
        "Q26805818" => return true,   // Italian Open Data License 2.0
        "Q52555753" => return false,  // CC-BY-3.0-AU
        "Q80939351" => return true,   // etalab-2.0
        "Q99891702" => return true,   // OGL-UK-3.0
        "Q106835855" => return false, // NLOD-2.0
        "Q115716001" => return false, // opendata.swiss BY-ASK
        "Q133462062" => return true,  // opendata.swiss OPEN
        "Q133802534" => return false, // opendata.swiss BY
        _ => {}
    }

    // https://github.com/alltheplaces/alltheplaces/blob/master/docs/SPIDER_LINEAGE.md
    let lineage = attrs
        .get("spider:lineage")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match lineage {
        "S_ATP_BRANDS" => true, // first-party data
        _ => false,
    }
}

fn make_place(geojson: &str) -> Option<Place> {
    let parsed = geojson.parse::<GeoJson>().ok()?;
    let coord = find_point(&parsed)?.0;
    let GeoJson::Feature(feature) = parsed else {
        return None;
    };
    let properties = feature.properties?;

    let mut source: Option<String> = None;

    // We strip three properties ("nsi_id", "@spider", "@source_uri") from tags,
    // so we do not need to reserve space for them.
    let num_tags = properties.len();
    let mut tags =
        Vec::<(String, String)>::with_capacity(if num_tags > 3 { num_tags - 3 } else { num_tags });
    let mut mask = MatchMask::default();

    for (key, val) in properties {
        // All The Places inserts an "nsi_id" for any features
        // that are matches for the OSM Name Suggestion Index.
        // Since this is purely internal to debugging All The Places,
        // we strip off this property here.
        if key == "nsi_id" || key.is_empty() {
            continue;
        }

        let value = match val {
            serde_json::Value::String(v) => Some(v),
            serde_json::Value::Bool(v) => Some(v.to_string()),
            serde_json::Value::Number(v) => Some(v.to_string()),
            _ => None,
        };
        let Some(mut value) = value else {
            continue;
        };

        if key == "@spider" {
            value.insert_str(0, "atp/");
            source = Some(value);
            continue;
        }

        if key.starts_with('@') || value.is_empty() {
            continue;
        }

        mask.add_tag(&key, &value);
        tags.push((key, value));
    }
    Place::new(&coord, source?, mask, tags)
}

/// Finds a representative point for a GeoJson feature.
fn find_point(geojson: &GeoJson) -> Option<Point> {
    let GeoJson::Feature(f) = geojson else {
        return None;
    };
    let Some(geometry) = &f.geometry else {
        return None;
    };
    let geom = TryInto::<geo::Geometry<f64>>::try_into(geometry).ok()?;
    match geom {
        geo::Geometry::LineString(line_string) => {
            Haversine.point_at_ratio_from_start(&line_string, 0.5)
        }
        geo::Geometry::MultiLineString(mls) => {
            if !mls.0.is_empty() {
                Haversine.point_at_ratio_from_start(&mls.0[0], 0.5)
            } else {
                None
            }
        }
        _ => geom.interior_point(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::ProgressDrawTarget;
    use std::collections::HashMap;

    // A point feature for testing.
    const PLAYGROUND: &str = r#"{
       "type": "Feature",
       "properties": {
           "leisure": "playground",
           "addr:street": "Hermann-G\u00f6tz-Strasse",
           "addr:city": "Winterthur",
           "operator": "Stadtgr\u00fcn Winterthur",
           "operator:wikidata": "Q56825906",
           "@spider": "winterthur_ch"
       },
       "geometry": {
           "type": "Point",
           "coordinates": [8.7339982, 47.5039168]
        }
    }"#;

    const BICYCLE_ROAD: &str = r#"{
       "type": "Feature",
       "properties": {
           "@spider": "bern_ch",
           "highway": "residential",
           "bicycle_road": "yes",
           "@source_uri": "https://map.bern.ch/ogd/poi_velo/poi_velo_json.zip"
       },
       "geometry": {
           "type": "LineString",
           "coordinates": [
               [7.458535, 46.940702],
               [7.458746, 46.941164],
               [7.458778, 46.941229],
               [7.459291, 46.942315],
               [7.459298, 46.942329],
               [7.459647, 46.943080],
               [7.460838, 46.943692]
           ]
       }
    }"#;

    fn find_point(geojson: &str) -> Option<Point> {
        super::find_point(&geojson.parse::<GeoJson>().unwrap())
    }

    #[test]
    fn test_find_point_for_point() {
        let pt = find_point(PLAYGROUND).unwrap();
        assert!((pt.x() - 8.7339982).abs() < 1e-7);
        assert!((pt.y() - 47.5039168).abs() < 1e-7);
    }

    #[test]
    fn test_find_point_for_line_string() {
        let pt = find_point(&BICYCLE_ROAD).unwrap();
        assert!((pt.x() - 7.4593195).abs() < 1e-6);
        assert!((pt.y() - 46.9423753).abs() < 1e-6);
    }

    #[test]
    fn test_find_point_for_polygon() {
        let geojson = r#"{
           "type": "Feature",
           "geometry": {
               "type": "Polygon",
               "coordinates": [
                   [
                       [-80.190, 25.774],
                       [-66.118, 18.466],
                       [-64.757, 32.321]
                   ],
                   [
                       [-70.579, 28.745],
                       [-67.514, 29.570],
                       [-66.668, 27.339]
                   ]
               ]
           }
        }"#;
        let pt = find_point(&geojson).unwrap();
        assert!((pt.x() - -72.4474).abs() < 1e-3);
        assert!((pt.y() - 25.3935).abs() < 1e-3);
    }

    fn tags<'a>(place: &'a Place) -> Vec<(&'a str, &'a str)> {
        place
            .tags
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect()
    }

    #[test]
    fn test_make_place() {
        let place = super::make_place(PLAYGROUND).unwrap();
        assert_eq!(place.source, "atp/winterthur_ch");
        assert_eq!(
            tags(&place),
            [
                ("addr:city", "Winterthur"),
                ("addr:street", "Hermann-Götz-Strasse"),
                ("leisure", "playground"),
                ("operator", "Stadtgrün Winterthur"),
                ("operator:wikidata", "Q56825906"),
            ]
        );
    }

    #[test]
    fn test_make_place_for_road() {
        assert!(super::make_place(BICYCLE_ROAD).is_none());
    }

    #[test]
    fn test_process_zip() -> Result<()> {
        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("tests/test_data/alltheplaces.zip");
        let progress = MultiProgress::with_draw_target(ProgressDrawTarget::hidden());
        let (tx, rx) = sync_channel(1000);
        process_zip(&path, &progress, tx)?;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for place in rx {
            *counts.entry(place.source).or_insert(0) += 1;
        }
        assert_eq!(counts["atp/misenso_ch"], 3);
        assert_eq!(counts["atp/tchibo"], 1);
        assert_eq!(counts["atp/winterthur_ch"], 4);
        Ok(())
    }

    #[test]
    fn test_is_usable_for_osm() {
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[("use:openstreetmap", "yes")])),
            true
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[("use:openstreetmap", "no")])),
            false
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[
                ("license", "Creative Commons Zero"),
                ("license:wikidata", "Q6938433"),
                ("spider:lineage", "S_ATP_AGGREGATORS"),
            ])),
            true
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[
                ("license", "Creative Commons Zero"),
                ("license:wikidata", "Q6938433"),
                ("use:openstreetmap", "no"),
            ])),
            false
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[("spider:lineage", "S_ATP_BRANDS")])),
            true
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[
                ("license", "Creative Commons Attribution 4.0 International"),
                ("license:wikidata", "Q20007257"),
                ("spider:lineage", "S_ATP_BRANDS")
            ])),
            false
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[
                ("license", "Etalab Open License 2.0"),
                ("license:wikidata", "Q80939351"),
                ("spider:lineage", "S_ATP_GOVERNMENT")
            ])),
            true
        );
        assert_eq!(
            is_usable_for_osm(&make_dataset(&[("spider:lineage", "S_ATP_AGGREGATORS")])),
            false
        );
    }

    fn make_dataset(tags: &[(&str, &str)]) -> geojson::FeatureCollection {
        let dataset_attributes = tags
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect::<serde_json::Map<_, _>>();
        let mut foreign_members = serde_json::Map::new();
        foreign_members.insert(
            "dataset_attributes".to_string(),
            serde_json::Value::Object(dataset_attributes),
        );
        geojson::FeatureCollection {
            bbox: None,
            features: vec![],
            foreign_members: Some(foreign_members),
        }
    }
}

#[cfg(any(test, fuzzing))]
pub mod fuzz {
    use super::process_geojson;
    use std::io::Cursor;

    pub fn fuzz_process_geojson(data: &[u8]) {
        _ = process_geojson(Cursor::new(data), None);
    }

    #[test]
    fn test_fuzz_process_geojson() {
        // Make sure we don’t crash. The actual fuzzing is performed
        // by `cargo fuzz`, not when running unit tests.
        fuzz_process_geojson(b"foo");
    }
}
