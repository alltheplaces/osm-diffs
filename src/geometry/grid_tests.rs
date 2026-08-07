//! Regression tests driven by the vendored osm-testdata `grid` fixtures at
//! `tests/test_data/osm-testdata-grid/data` (see that directory's
//! `VENDORED.md` and <https://github.com/osmcode/osm-testdata/tree/master/grid>).
//!
//! Each fixture directory (`data/<category>/<id>/`) holds a small OSM-XML
//! file (`data.osm`) plus a `test.json` recording, for one or more of its
//! ways/relations, the `MULTIPOLYGON` WKT a correct multipolygon assembler
//! should produce — or the sentinel `"INVALID"` if the input has no valid
//! interpretation under strict OGC rules. Grid categories `1xx`/`3xx` (node/
//! way/attribute validity) carry no expected geometry and are not exercised
//! here; only `7xx`/`9xx` (multipolygon geometry, and roles/tags) do.
//!
//! For each such fixture, this test parses `data.osm` itself (nodes, ways,
//! relation members — a hand-rolled parser, since the fixtures are a fixed,
//! single-element-per-line format and the crate has no other need for an
//! OSM-XML reader), runs every member way through [`build_line`]/
//! [`build_ring`], and assembles the relation (or, for `from_type: "way"`
//! entries, the standalone closed way) through [`GeometryBuilder`] — the
//! same path production code uses, and one that, by design, ignores
//! declared member roles in favor of geometric nesting (see
//! [`PolygonAssembler`]'s docs). The result is compared to the expected
//! WKT by area of symmetric difference, not by string equality, since
//! ring start point/winding direction legitimately differ.
//!
//! # `default` vs. `fix`/`location`, and known mismatches
//! A fixture's `default` interpretation is the strict OGC reading; `fix`
//! and `location` (checked, in the vendored data, only ever alongside an
//! `INVALID` default) are alternate, deliberately lenient readings — e.g.
//! treating nodes at the same location but with different IDs as the same
//! point. `GeometryBuilder` is itself lenient (it repairs self-
//! intersections and mismatched endpoints rather than rejecting them), so
//! it's compared against whichever of `default`/`fix`/`location` is
//! actually a valid WKT (see [`expected_geometry`]).
//!
//! One further caveat, tracked as a known mismatch in [`KNOWN_MISMATCHES`]
//! rather than a hard test failure:
//! * **A segment duplicated across two ways isn't deduped, and can leave a
//!   ring in pieces.** Two ways that both contain the same segment (`711`
//!   -- unlike a "spike", `742`/`743`'s exact-reversal case, which
//!   `LineStitcher` does now handle) stitch into one chain, get cut at the
//!   incidental point-touches the duplicate creates, and end up as two
//!   duplicate 2-point pieces plus the real path -- after which every
//!   node the duplicate touches looks like a genuine degree-3 junction
//!   (one edge from each duplicate copy, one from the real path), which
//!   correctly blocks further merging in the general case but is wrong
//!   here, where the "3" is an artifact of the duplicate rather than a
//!   real fork. See <https://github.com/alltheplaces/osm-diffs/issues/541>.
//!
//! A fixture with no valid `default`/`fix`/`location` at all has no oracle
//! to check against and is likewise recorded in [`KNOWN_MISMATCHES`].
//! None of this is something this test suite should fail CI over today —
//! it's the follow-up work the grid run surfaced.

use std::{collections::HashMap, fs, path::PathBuf};

use geo::Area;
use geo::algorithm::bool_ops::BooleanOps;
use serde::Deserialize;
use wkt::TryFromWkt;

use super::*;

// =======================================================================
// Minimal OSM-XML parsing, tailored to the grid fixtures' fixed format
// (one element per line, double-quoted attributes, no self-closing
// <way>/<relation> tags even when childless) rather than general XML.
// =======================================================================

struct GridWay {
    node_refs: Vec<i64>,
}

struct GridMember {
    member_type: String,
    member_ref: i64,
}

struct GridRelation {
    members: Vec<GridMember>,
}

struct GridData {
    nodes: HashMap<i64, Coord<f64>>,
    ways: HashMap<i64, GridWay>,
    relations: HashMap<i64, GridRelation>,
}

/// Extracts the value of `name="..."` from an XML element line.
fn attr<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = line.find(needle.as_str())? + needle.len();
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}

enum Context {
    None,
    Way(i64),
    Relation(i64),
}

fn parse_osm(xml: &str) -> GridData {
    let mut nodes = HashMap::new();
    let mut ways: HashMap<i64, GridWay> = HashMap::new();
    let mut relations: HashMap<i64, GridRelation> = HashMap::new();
    let mut context = Context::None;

    for line in xml.lines() {
        let line = line.trim();
        if line.starts_with("<node ") {
            let id: i64 = attr(line, "id").expect("node needs id").parse().unwrap();
            let lon: f64 = attr(line, "lon").expect("node needs lon").parse().unwrap();
            let lat: f64 = attr(line, "lat").expect("node needs lat").parse().unwrap();
            nodes.insert(id, Coord { x: lon, y: lat });
        } else if line.starts_with("<way ") {
            let id: i64 = attr(line, "id").expect("way needs id").parse().unwrap();
            ways.insert(
                id,
                GridWay {
                    node_refs: Vec::new(),
                },
            );
            context = Context::Way(id);
        } else if line.starts_with("<nd ") {
            if let Context::Way(id) = context {
                let node_ref: i64 = attr(line, "ref").expect("nd needs ref").parse().unwrap();
                ways.get_mut(&id).unwrap().node_refs.push(node_ref);
            }
        } else if line.starts_with("<relation ") {
            let id: i64 = attr(line, "id")
                .expect("relation needs id")
                .parse()
                .unwrap();
            relations.insert(
                id,
                GridRelation {
                    members: Vec::new(),
                },
            );
            context = Context::Relation(id);
        } else if line.starts_with("<member ") {
            if let Context::Relation(id) = context {
                let member_type = attr(line, "type").expect("member needs type").to_string();
                let member_ref: i64 = attr(line, "ref")
                    .expect("member needs ref")
                    .parse()
                    .unwrap();
                relations.get_mut(&id).unwrap().members.push(GridMember {
                    member_type,
                    member_ref,
                });
            }
        } else if line == "</way>" || line == "</relation>" {
            context = Context::None;
        }
        // <tag> lines (and everything else) carry nothing this test needs:
        // roles come from <member>, and which relations to build is driven
        // by test.json's from_type/from_id, not by the OSM `type` tag.
    }

    GridData {
        nodes,
        ways,
        relations,
    }
}

// =======================================================================
// Building actual geometry, via the same functions production code uses
// =======================================================================

fn way_coords(data: &GridData, way_id: i64) -> Vec<Coord<f64>> {
    data.ways[&way_id]
        .node_refs
        .iter()
        .map(|node_id| data.nodes[node_id])
        .collect()
}

fn build_way_geometry(data: &GridData, way_id: i64) -> Option<Geometry<f64>> {
    let coords = way_coords(data, way_id);
    if coords.len() >= 2 && coords.first() == coords.last() {
        build_ring(coords)
    } else {
        build_line(coords)
    }
}

/// Builds the actual geometry for one `test.json` area entry
/// (`from_type`/`from_id`), the same way production code would: each
/// member way through `build_line`/`build_ring`, then all of them through
/// a `GeometryBuilder` (which ignores declared roles).
fn build_area(data: &GridData, from_type: &str, from_id: i64) -> Option<Geometry<f64>> {
    match from_type {
        "way" => build_way_geometry(data, from_id),
        "relation" => {
            let relation = data.relations.get(&from_id)?;
            let mut builder = GeometryBuilder::new(PolygonFill::Containment);
            for member in &relation.members {
                if member.member_type == "way"
                    && let Some(geometry) = build_way_geometry(data, member.member_ref)
                {
                    builder.add(&geometry);
                }
                // No `type="relation"` members appear in the vendored grid
                // fixtures, so recursive super-relations aren't exercised
                // here.
            }
            builder.finish()
        }
        other => panic!("unexpected from_type {other:?} in grid fixture"),
    }
}

// =======================================================================
// WKT parsing, via the `wkt` crate, for test.json's `MULTIPOLYGON(...)`
// expected-geometry values
// =======================================================================

/// Parses a `MULTIPOLYGON(...)` string, the only WKT shape that appears in
/// the grid fixtures' `test.json` files.
fn parse_multipolygon_wkt(wkt: &str) -> MultiPolygon<f64> {
    MultiPolygon::<f64>::try_from_wkt_str(wkt)
        .unwrap_or_else(|e| panic!("failed to parse grid fixture wkt {wkt:?}: {e}"))
}

// =======================================================================
// test.json parsing
// =======================================================================

#[derive(Deserialize)]
struct TestJson {
    #[allow(dead_code)]
    test_id: u32,
    #[allow(dead_code)]
    description: String,
    areas: Option<Areas>,
}

#[derive(Deserialize)]
struct Areas {
    default: Option<Vec<AreaEntry>>,
    fix: Option<Vec<AreaEntry>>,
    location: Option<Vec<AreaEntry>>,
}

#[derive(Deserialize)]
struct AreaEntry {
    from_id: i64,
    from_type: String,
    wkt: String,
}

/// Picks the variant to check `GeometryBuilder`'s (lenient) output
/// against: `default` if it's fully valid, else `fix`, else `location` —
/// matching the fact that, in this fixture set, `fix`/`location` are only
/// ever present to give an alternative when `default` is `"INVALID"` (see
/// module docs).
fn expected_geometry(areas: &Areas) -> Option<&Vec<AreaEntry>> {
    for variant in [&areas.default, &areas.fix, &areas.location] {
        if let Some(entries) = variant
            && entries.iter().all(|e| e.wkt != "INVALID")
        {
            return Some(entries);
        }
    }
    None
}

// =======================================================================
// Geometric comparison (by area of symmetric difference, not WKT string
// equality: start point, winding direction, and collinear-point splitting
// legitimately differ between two equally-valid representations)
// =======================================================================

fn as_multipolygon(geometry: &Geometry<f64>) -> Option<MultiPolygon<f64>> {
    match geometry {
        Geometry::Polygon(p) => Some(MultiPolygon::new(vec![p.clone()])),
        Geometry::MultiPolygon(mp) => Some(mp.clone()),
        _ => None,
    }
}

fn areas_match(actual: &Geometry<f64>, expected: &MultiPolygon<f64>) -> bool {
    let Some(actual) = as_multipolygon(actual) else {
        return false;
    };
    let expected_area = expected.unsigned_area();
    let actual_area = actual.unsigned_area();
    let scale = expected_area.max(actual_area).max(1e-9);
    let symmetric_difference_area = actual.xor(expected).unsigned_area();
    symmetric_difference_area / scale < 1e-6
}

// =======================================================================
// The test itself
// =======================================================================

/// Grid test IDs whose `GeometryBuilder` output is known not to match its
/// oracle yet — either because there's no valid `default`/`fix`/`location`
/// WKT to check against at all, or because of the
/// multipolygon-vs-general-relation gap noted in the module docs. Tracked
/// here (rather than skipped silently) so a fix shows up as a now-
/// unexpectedly-passing test, prompting its removal from this list.
const KNOWN_MISMATCHES: &[u32] = &[
    // A segment duplicated across two ways isn't deduped (see module
    // docs' "duplicated across two ways" caveat).
    // https://github.com/alltheplaces/osm-diffs/issues/541
    711,
    // No default/fix/location entry parses to a real polygon at all --
    // nothing to check `GeometryBuilder`'s output against yet. Not a filed
    // bug: these fixtures are meant to have no lenient interpretation.
    710, 714, 715, 740, 741, 744, 745, 746, 768, 771, 773,
];

fn grid_fixture_dirs() -> Vec<PathBuf> {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.push("tests/test_data/osm-testdata-grid/data");

    let mut dirs = Vec::new();
    for category in ["7", "9"] {
        let category_dir = root.join(category);
        let mut entries: Vec<_> = fs::read_dir(&category_dir)
            .unwrap_or_else(|e| panic!("failed to read {category_dir:?}: {e}"))
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        entries.sort();
        dirs.extend(entries);
    }
    dirs
}

#[test]
fn grid_multipolygon_tests() {
    let mut mismatches = Vec::new();
    let mut checked = 0usize;

    for dir in grid_fixture_dirs() {
        let test_id: u32 = dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap_or_else(|_| panic!("non-numeric grid fixture dir {dir:?}"));

        let test_json: TestJson =
            serde_json::from_str(&fs::read_to_string(dir.join("test.json")).unwrap())
                .unwrap_or_else(|e| panic!("failed to parse {test_id}/test.json: {e}"));
        let Some(areas) = &test_json.areas else {
            continue; // not a multipolygon test (no expected geometry)
        };
        let Some(expected_entries) = expected_geometry(areas) else {
            if !KNOWN_MISMATCHES.contains(&test_id) {
                mismatches.push(format!("{test_id}: no valid default/fix/location oracle"));
            }
            continue;
        };

        let data = parse_osm(&fs::read_to_string(dir.join("data.osm")).unwrap());
        checked += 1;

        let mut case_ok = true;
        let mut details = Vec::new();
        for entry in expected_entries {
            let expected = parse_multipolygon_wkt(&entry.wkt);
            match build_area(&data, &entry.from_type, entry.from_id) {
                Some(actual) if areas_match(&actual, &expected) => {}
                Some(actual) => {
                    case_ok = false;
                    details.push(format!(
                        "{} {}: expected {}, got {actual:?}",
                        entry.from_type, entry.from_id, entry.wkt
                    ));
                }
                None => {
                    case_ok = false;
                    details.push(format!(
                        "{} {}: expected {}, got nothing",
                        entry.from_type, entry.from_id, entry.wkt
                    ));
                }
            }
        }

        if !case_ok && !KNOWN_MISMATCHES.contains(&test_id) {
            mismatches.push(format!("{test_id}: {}", details.join("; ")));
        }
    }

    assert!(
        checked > 0,
        "no grid fixtures with a checkable oracle were found"
    );
    assert!(
        mismatches.is_empty(),
        "{} unexpected grid mismatch(es):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

#[test]
fn known_mismatches_are_still_mismatches() {
    // Guards against KNOWN_MISMATCHES silently going stale: if a listed
    // test id starts passing (e.g. after a GeometryBuilder fix), it should
    // be removed from the list rather than staying there unnoticed.
    for &test_id in KNOWN_MISMATCHES {
        let dir = grid_fixture_dirs()
            .into_iter()
            .find(|d| d.file_name().unwrap().to_str().unwrap() == test_id.to_string())
            .unwrap_or_else(|| panic!("KNOWN_MISMATCHES has stale test id {test_id}"));

        let test_json: TestJson =
            serde_json::from_str(&fs::read_to_string(dir.join("test.json")).unwrap()).unwrap();
        let areas = test_json
            .areas
            .as_ref()
            .unwrap_or_else(|| panic!("{test_id} in KNOWN_MISMATCHES has no areas at all"));
        let data = parse_osm(&fs::read_to_string(dir.join("data.osm")).unwrap());

        let still_mismatched = match expected_geometry(areas) {
            None => true, // still no oracle to check against
            Some(entries) => entries.iter().any(|entry| {
                let expected = parse_multipolygon_wkt(&entry.wkt);
                match build_area(&data, &entry.from_type, entry.from_id) {
                    Some(actual) => !areas_match(&actual, &expected),
                    None => true,
                }
            }),
        };
        assert!(
            still_mismatched,
            "{test_id} is in KNOWN_MISMATCHES but now matches its oracle; remove it from the list"
        );
    }
}
