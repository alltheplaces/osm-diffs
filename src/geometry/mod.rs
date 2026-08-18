//! Construction, repair, and assembly of `geo` geometries from raw
//! coordinates and OSM-style relation members.
//!
//! * [`build_points`] — a set of points, returned as a `Point`
//!   (single coordinate) or `MultiPoint`
//! * [`build_line`] — a path, returned as a `Point` (single coordinate),
//!   `LineString`, or `MultiLineString`
//! * [`build_ring`] — a closed boundary, returned as a `Polygon` or
//!   `MultiPolygon`
//! * [`PolygonAssembler`] — assembles an arbitrarily-ordered bag of rings
//!   (as from a multipolygon relation's members) into a `Polygon` or
//!   `MultiPolygon`, letting geometric nesting (not declared role)
//!   determine what's a shell and what's a hole, with a bounded coordinate
//!   budget for very large inputs (e.g. a relation with thousands of
//!   member ways)
//! * [`LineStitcher`] — stitches arbitrarily-ordered, arbitrarily-oriented
//!   `LineString`s into longer paths wherever their endpoints touch, with
//!   a bounded coordinate budget for very large inputs (e.g. OSM's
//!   Russia-border relation, 5000+ member ways)
//!
//! `build_line`/`build_ring` are fast in the common case (no
//! self-intersections: a single O(n log n) sweep confirms that, then the
//! geometry is returned directly) and fall back to repairing the input
//! when it self-intersects, rather than rejecting it outright. All four
//! also guard against a segment that crosses the antimeridian (±180°
//! longitude) being misread as an enormous segment spanning most of the
//! globe — see [`unwrap_antimeridian`] and [`align_to_reference_x`].

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use geo::MakeValid;
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::algorithm::sweep::{Cross, Intersections};
use geo::algorithm::validation::Validation;
use geo::orient::{Direction, Orient};
use geo::{
    Coord, Geometry, Line, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

mod geometry_builder;
mod geometry_tally;
#[cfg(test)]
mod grid_tests;
mod line_stitcher;
mod polygon_assembler;
mod polygon_union;
mod wkb_type;
mod wkb_writer;

pub use geometry_builder::{GeometryBuilder, PolygonFill};
pub use geometry_tally::GeometryTally;
pub use line_stitcher::LineStitcher;
pub use polygon_assembler::PolygonAssembler;
pub use polygon_union::PolygonUnion;
pub use wkb_type::{WkbGeometryType, wkb_geometry_type};
pub use wkb_writer::encode_wkb;

// =======================================================================
// build_points
// =======================================================================

/// Build a valid OGC Simple Features geometry for a set of points.
pub fn build_points(coords: Vec<Coord<f64>>) -> Option<Geometry<f64>> {
    let mut coords = coords;

    // Retain only coordinates where both x and y are finite.
    coords.retain(|c| c.x.is_finite() && c.y.is_finite());

    // Fast path: 0 or 1 points.
    match coords.len() {
        0 => return None,
        1 => return Some(Geometry::from(Point::new(coords[0].x, coords[0].y))),
        _ => {}
    }

    // De-duplicate (which needs sorted input).
    coords.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.y.partial_cmp(&b.y).unwrap_or(Ordering::Equal))
    });
    coords.dedup();

    if coords.len() == 1 {
        Some(Geometry::from(Point::new(coords[0].x, coords[0].y)))
    } else {
        let mp: MultiPoint<f64> = coords.into_iter().map(Point::from).collect();
        Some(Geometry::from(mp))
    }
}

// =======================================================================
// build_line / build_ring
// =======================================================================

/// Build a valid OGC Simple Features geometry from an **open** path of
/// coordinates.
///
/// # Degenerate-length input
/// * Zero coordinates: returns `None`.
/// * Exactly one coordinate: returns a [`Geometry::Point`] rather than
///   `None` — a single coordinate is a perfectly valid (if trivial) OGC
///   geometry, just not a line.
/// * Any non-finite (`NaN`/`inf`) coordinate, at any position: returns
///   `None`.
///
/// # Antimeridian handling
/// Before anything else, longitudes are unwrapped with
/// [`unwrap_antimeridian`] so a segment that actually crosses the ±180°
/// meridian (e.g. 179.9° to -179.9°, a short hop) isn't misread as a huge
/// segment spanning nearly the whole globe. See that function's docs for
/// the assumptions this makes and what the output coordinates look like.
///
/// # Fast path
/// If the (unwrapped) path has no self-intersections (the common case), a
/// single O(n log n) sweep-line pass (Bentley–Ottmann) confirms that, and
/// the function returns a [`Geometry::LineString`] directly.
///
/// # Repair path
/// If the path *does* self-intersect, every self-intersection point is
/// inserted as a vertex and the path is cut into simple pieces at each of
/// those points. Cutting at every self-intersection point guarantees each
/// resulting piece is simple: an interior self-crossing within a piece
/// would itself have been detected by the sweep and turned into a cut
/// point, which is a contradiction. The pieces are returned as a
/// [`Geometry::MultiLineString`].
///
/// Exact segment overlaps (two parts of the path running along the same
/// line) aren't split by this repair; they're rare in practice and would
/// need a merge step rather than a point cut.
pub fn build_line(coords: Vec<Coord<f64>>) -> Option<Geometry<f64>> {
    if coords.is_empty() || coords.iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return None;
    }
    if coords.len() == 1 {
        return Some(Geometry::from(Point::new(coords[0].x, coords[0].y)));
    }

    let mut coords = coords;
    unwrap_antimeridian(&mut coords);

    let ls = LineString::new(coords);
    if !ls.is_valid() {
        return None;
    }

    let crossings = find_crossings(&ls);
    if crossings.is_empty() {
        return Some(Geometry::from(ls));
    }

    let pieces = cut_at_crossings(&ls, crossings);
    if pieces.is_empty() {
        None
    } else {
        Some(Geometry::from(MultiLineString::new(pieces)))
    }
}

/// Build a valid OGC Simple Features geometry from a **closed** ring of
/// coordinates (a polygon boundary with no holes).
///
/// The input does not need to already be closed — if the first and last
/// coordinates differ, the ring is closed automatically by repeating the
/// first coordinate.
///
/// # Fast path
/// If the ring's boundary is simple (the common case), the function
/// returns a [`Geometry::Polygon`] with that ring as its exterior and no
/// interior rings. A single validity pass (which, per OGC rules, includes
/// a self-intersection check for closed rings) confirms this.
///
/// # Repair path
/// If the ring self-intersects (e.g. a "bowtie"), it's repaired using
/// `geo`'s constrained-Delaunay-triangulation-based polygon repair
/// (`MakeValid`), which can split one self-intersecting ring into multiple
/// simple polygons. The result is returned as a [`Geometry::MultiPolygon`].
///
/// # Antimeridian handling
/// Before validation, longitudes are unwrapped with
/// [`unwrap_antimeridian`] so a ring that legitimately straddles the ±180°
/// meridian (e.g. a country like Fiji or Russia) isn't misread as
/// self-intersecting because of the coordinate discontinuity. This assumes
/// the ring only crosses the meridian locally rather than circumnavigating
/// the globe — see that function's docs for details.
///
/// # Ring orientation
/// OGC Simple Features validity doesn't constrain winding order — a
/// clockwise exterior ring is just as "valid" as a counterclockwise one —
/// but many consumers (notably GeoJSON, per RFC 7946) expect a canonical
/// orientation. Regardless of the input's winding, the returned exterior
/// ring(s) are always counterclockwise and interior rings clockwise
/// (`geo`'s `Direction::Default`). If you need the opposite convention
/// (e.g. traditional ESRI shapefiles), reorient the result yourself with
/// `.orient(Direction::Reversed)`.
///
/// # Degenerate input
/// Returns `None` if the coordinates can't form a valid geometry at all:
/// empty input, fewer than three distinct points, or any non-finite
/// coordinate.
pub fn build_ring(coords: Vec<Coord<f64>>) -> Option<Geometry<f64>> {
    let mut coords = coords;
    match (coords.first().copied(), coords.last().copied()) {
        (Some(first), Some(last)) if first != last => coords.push(first),
        (Some(_), Some(_)) => {} // already closed
        _ => return None,        // empty input
    }
    unwrap_antimeridian(&mut coords);

    let polygon = Polygon::new(LineString::new(coords), vec![]);

    if polygon.is_valid() {
        return Some(Geometry::from(polygon.orient(Direction::Default)));
    }

    if let Some(repaired) = polygon.make_valid().ok()
        && !repaired.0.is_empty()
    {
        let oriented: Vec<Polygon<f64>> = repaired
            .0
            .into_iter()
            .map(|p| p.orient(Direction::Default))
            .collect();
        Some(Geometry::from(MultiPolygon::new(oriented)))
    } else {
        None
    }
}

// =======================================================================
// Antimeridian handling, shared by every builder in this module
// =======================================================================

/// Unwrap longitudes in place so that no consecutive pair of coordinates
/// has an apparent jump greater than 180 degrees in `x`.
///
/// This is the standard fix for paths/rings crossing the antimeridian
/// (±180°): without it, a segment that actually crosses from, say, 179.9°
/// to -179.9° (a short hop across the dateline) looks like an enormous
/// ~360° segment spanning nearly the whole globe — which then falsely
/// appears to self-intersect with the rest of the path, triggering
/// spurious (and wrong) repairs.
///
/// It works like phase unwrapping: it walks the coordinates keeping a
/// running multiple-of-360 offset, and bumps that offset by ∓360 whenever
/// the next point would otherwise require a jump of more than 180° from
/// the previous *unwrapped* point.
///
/// # Assumptions and caveats
/// * `x` is assumed to be longitude in the conventional `[-180, 180]`
///   range. For planar (non-geographic) data this is a no-op unless your
///   coordinates happen to jump by more than 180 units between consecutive
///   points, which would be unusual.
/// * This assumes the path/ring crosses the meridian locally rather than
///   circumnavigating the globe. A ring that nets a full 360° revolution
///   in longitude will end up with its closing coordinate no longer
///   bit-equal to its opening one; that's a genuinely unusual/degenerate
///   input this function doesn't attempt to handle.
/// * The output is *not* re-wrapped back into `[-180, 180]`: OGC Simple
///   Features geometries have no degree-range constraint, so a continuous
///   ("unwrapped") sequence like `[170, 175, 185, 190]` is just as valid a
///   geometry as one that stays within `[-180, 180]` — it only needs
///   re-wrapping by the caller if a canonical range is required at
///   render/serialization time (e.g. for GeoJSON output).
fn unwrap_antimeridian(coords: &mut [Coord<f64>]) {
    let mut offset = 0.0_f64;
    for i in 1..coords.len() {
        let prev_x = coords[i - 1].x; // already unwrapped by a prior iteration
        let raw_delta = (coords[i].x + offset) - prev_x;
        if raw_delta > 180.0 {
            offset -= 360.0;
        } else if raw_delta < -180.0 {
            offset += 360.0;
        }
        coords[i].x += offset;
    }
}

/// Shift `ls`'s x-coordinates by whichever multiple of 360° brings its
/// centroid closest to `reference_x`, so geometries that were each
/// independently antimeridian-unwrapped (via [`unwrap_antimeridian`], each
/// relative to its own starting point) end up in one consistent coordinate
/// frame before being combined by [`PolygonAssembler`] or [`LineStitcher`].
///
/// For example: an exterior ring unwrapped to sit at longitude 170–190,
/// and a hole that was independently unwrapped (or never crossed the
/// meridian at all) and sits natively around -175, need to be brought into
/// the same frame — shifting the hole by +360 to ~185 — before any planar
/// operation (containment, intersection, ...) can see them correctly.
fn align_to_reference_x(ls: &mut LineString<f64>, reference_x: f64) {
    let cx = centroid_x(ls);
    let mut best_shift = 0.0_f64;
    let mut best_dist = (cx - reference_x).abs();
    for shift in [360.0, -360.0] {
        let dist = (cx + shift - reference_x).abs();
        if dist < best_dist {
            best_dist = dist;
            best_shift = shift;
        }
    }
    if best_shift != 0.0 {
        for c in ls.0.iter_mut() {
            c.x += best_shift;
        }
    }
}

fn centroid_x(ls: &LineString<f64>) -> f64 {
    let n = ls.0.len().max(1) as f64;
    ls.0.iter().map(|c| c.x).sum::<f64>() / n
}

// =======================================================================
// Self-intersection repair for open paths, shared by build_line and
// LineStitcher
// =======================================================================

#[derive(Clone, Copy)]
struct IndexedLine {
    idx: usize,
    line: Line<f64>,
}

impl Cross for IndexedLine {
    type Scalar = f64;
    fn line(&self) -> Line<f64> {
        self.line
    }
}

struct Crossing {
    /// Index of the segment being cut, and the parametric position (0..1)
    /// along it where the cut occurs. `param` of `0.0` or `1.0` means the
    /// crossing lands exactly on an existing vertex.
    segment_idx: usize,
    param: f64,
}

/// Run a single Bentley–Ottmann sweep over `ls`'s segments and collect
/// every self-intersection, excluding the expected touch between
/// consecutive segments at their shared vertex.
fn find_crossings(ls: &LineString<f64>) -> Vec<Crossing> {
    let n = ls.0.len().saturating_sub(1);
    if n < 2 {
        return Vec::new();
    }
    let closed = ls.is_closed();

    let segments: Vec<IndexedLine> = ls
        .lines()
        .enumerate()
        .map(|(idx, line)| IndexedLine { idx, line })
        .collect();

    let mut crossings = Vec::new();
    for (a, b, _intersection) in Intersections::from_iter(segments.iter().copied()) {
        let adjacent = (a.idx as i64 - b.idx as i64).abs() == 1
            || (closed && ((a.idx == 0 && b.idx == n - 1) || (b.idx == 0 && a.idx == n - 1)));
        if adjacent {
            continue;
        }
        if let Some(LineIntersection::SinglePoint {
            intersection: pt, ..
        }) = line_intersection(a.line, b.line)
        {
            crossings.push(Crossing {
                segment_idx: a.idx,
                param: param_along(a.line, pt),
            });
            crossings.push(Crossing {
                segment_idx: b.idx,
                param: param_along(b.line, pt),
            });
        }
        // Collinear/overlapping-segment crossings aren't split here; see
        // build_line's doc comment for why that's an accepted limitation.
    }
    crossings
}

/// Cut `ls` into simple pieces at every crossing found by [`find_crossings`].
fn cut_at_crossings(ls: &LineString<f64>, crossings: Vec<Crossing>) -> Vec<LineString<f64>> {
    let coords = &ls.0;
    let n_segments = coords.len() - 1;

    let mut inserts: HashMap<usize, Vec<f64>> = HashMap::new();
    let mut break_vertices: HashSet<usize> = HashSet::new();

    for c in crossings {
        if c.param <= 1e-9 {
            break_vertices.insert(c.segment_idx);
        } else if c.param >= 1.0 - 1e-9 {
            break_vertices.insert(c.segment_idx + 1);
        } else {
            inserts.entry(c.segment_idx).or_default().push(c.param);
        }
    }

    let mut noded: Vec<(Coord<f64>, bool)> = Vec::new();
    for (i, &coord) in coords.iter().enumerate() {
        noded.push((coord, break_vertices.contains(&i)));
        if i < n_segments
            && let Some(ts) = inserts.get(&i)
        {
            let mut ts = ts.clone();
            ts.sort_by(|x, y| x.partial_cmp(y).unwrap());
            let seg = Line::new(coords[i], coords[i + 1]);
            for t in ts {
                let pt = Coord {
                    x: seg.start.x + t * (seg.end.x - seg.start.x),
                    y: seg.start.y + t * (seg.end.y - seg.start.y),
                };
                noded.push((pt, true));
            }
        }
    }

    let mut pieces = Vec::new();
    let mut current: Vec<Coord<f64>> = Vec::new();
    for (coord, is_break) in noded {
        current.push(coord);
        if is_break && current.len() >= 2 {
            pieces.push(LineString::new(std::mem::take(&mut current)));
            current.push(coord); // next piece continues from this point
        }
    }
    if current.len() >= 2 {
        pieces.push(LineString::new(current));
    }

    pieces.retain(|p| p.0.len() >= 2 && !(p.0.len() == 2 && p.0[0] == p.0[1]));
    pieces
}

/// Parametric position (0.0..=1.0) of `pt` along `line`, assuming `pt` lies on it.
fn param_along(line: Line<f64>, pt: Coord<f64>) -> f64 {
    let dx = line.end.x - line.start.x;
    let dy = line.end.y - line.start.y;
    let len_sq = dx * dx + dy * dy;
    if len_sq == 0.0 {
        return 0.0;
    }
    ((pt.x - line.start.x) * dx + (pt.y - line.start.y) * dy) / len_sq
}

// =======================================================================
// Tests
// =======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::coord;

    fn c(x: f64, y: f64) -> Coord<f64> {
        coord! { x: x, y: y }
    }

    #[test]
    fn build_points_no_points_returns_none() {
        assert!(build_points(vec![]).is_none());
    }

    #[test]
    fn build_points_single_point_returns_point() {
        let geom = build_points(vec![c(0.0, 0.0)]).expect("should build");
        assert!(matches!(geom, Geometry::Point(_)));
    }

    #[test]
    fn build_points_removes_duplicate() {
        let coords = vec![c(0.0, 0.0), c(1.0, 1.0), c(0.0, 0.0), c(1.0, 1.0)];
        if let Some(Geometry::MultiPoint(mp)) = build_points(coords) {
            assert_eq!(mp.len(), 2);
        } else {
            panic!("expected MultiPoint");
        }
    }

    #[test]
    fn build_line_simple_path_returns_line_string() {
        let coords = vec![c(0.0, 0.0), c(1.0, 0.0), c(1.0, 1.0)];
        let geom = build_line(coords).expect("should build");
        assert!(matches!(geom, Geometry::LineString(_)));
    }

    #[test]
    fn build_line_empty_returns_none() {
        assert!(build_line(vec![]).is_none());
    }

    #[test]
    fn build_line_single_coordinate_returns_point() {
        let geom = build_line(vec![c(1.5, -2.5)]).expect("should build");
        match geom {
            Geometry::Point(p) => {
                assert_eq!(p.x(), 1.5);
                assert_eq!(p.y(), -2.5);
            }
            other => panic!("expected Point, got {other:?}"),
        }
    }

    #[test]
    fn build_line_single_non_finite_coordinate_returns_none() {
        assert!(build_line(vec![c(f64::NAN, 0.0)]).is_none());
    }

    #[test]
    fn build_line_non_finite_returns_none() {
        let coords = vec![c(0.0, 0.0), c(f64::NAN, 1.0)];
        assert!(build_line(coords).is_none());
    }

    #[test]
    fn build_line_antimeridian_crossing_returns_single_line_string() {
        // A short hop across the dateline: 179deg -> -179deg is really a
        // 2-degree segment, not a 358-degree one.
        let coords = vec![c(179.0, 10.0), c(-179.0, 12.0), c(-177.0, 14.0)];
        let geom = build_line(coords).expect("should build");
        match geom {
            Geometry::LineString(ls) => {
                let xs: Vec<f64> = ls.0.iter().map(|c| c.x).collect();
                for w in xs.windows(2) {
                    assert!((w[1] - w[0]).abs() < 180.0);
                }
            }
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    fn build_line_self_intersecting_returns_multi_line_string() {
        let coords = vec![c(0.0, 0.0), c(2.0, 2.0), c(2.0, 0.0), c(0.0, 2.0)];
        let geom = build_line(coords).expect("should build");
        match geom {
            Geometry::MultiLineString(mls) => {
                assert!(mls.0.len() >= 2);
                for piece in &mls.0 {
                    assert!(piece.is_valid());
                }
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn build_line_repair_preserves_all_coordinates() {
        let coords = vec![c(0.0, 0.0), c(2.0, 2.0), c(2.0, 0.0), c(0.0, 2.0)];
        let geom = build_line(coords).unwrap();
        if let Geometry::MultiLineString(mls) = geom {
            let total_points: usize = mls.0.iter().map(|l| l.0.len()).sum();
            assert!(total_points >= 4);
        } else {
            panic!("expected MultiLineString");
        }
    }

    #[test]
    fn build_ring_simple_ring_returns_polygon() {
        let coords = vec![c(0.0, 0.0), c(4.0, 0.0), c(4.0, 4.0), c(0.0, 4.0)];
        let geom = build_ring(coords).expect("should build");
        assert!(matches!(geom, Geometry::Polygon(_)));
    }

    #[test]
    fn build_ring_auto_closes_open_input() {
        let coords = vec![c(0.0, 0.0), c(4.0, 0.0), c(4.0, 4.0), c(0.0, 4.0)];
        let geom = build_ring(coords).expect("should build");
        if let Geometry::Polygon(p) = geom {
            assert_eq!(p.exterior().0.first(), p.exterior().0.last());
        } else {
            panic!("expected Polygon");
        }
    }

    #[test]
    fn build_ring_bowtie_returns_multi_polygon() {
        let coords = vec![c(0.0, 0.0), c(0.0, 20.0), c(20.0, 0.0), c(20.0, 20.0)];
        let geom = build_ring(coords).expect("should build");
        match geom {
            Geometry::MultiPolygon(mp) => {
                assert!(mp.0.len() >= 2);
                for poly in &mp.0 {
                    assert!(poly.is_valid());
                }
            }
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn build_ring_too_few_points_returns_none() {
        assert!(build_ring(vec![c(0.0, 0.0), c(1.0, 1.0)]).is_none());
        assert!(build_ring(vec![]).is_none());
    }

    #[test]
    fn build_ring_non_finite_returns_none() {
        let coords = vec![c(0.0, 0.0), c(4.0, 0.0), c(f64::NAN, 4.0)];
        assert!(build_ring(coords).is_none());
    }

    #[test]
    fn build_ring_antimeridian_crossing_returns_single_polygon() {
        let coords = vec![
            c(179.0, 10.0),
            c(-179.0, 10.0),
            c(-179.0, 12.0),
            c(179.0, 12.0),
        ];
        let geom = build_ring(coords).expect("should build");
        match geom {
            Geometry::Polygon(p) => {
                let xs: Vec<f64> = p.exterior().0.iter().map(|c| c.x).collect();
                for w in xs.windows(2) {
                    assert!((w[1] - w[0]).abs() < 180.0);
                }
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    /// Shoelace-formula signed area: positive for CCW, negative for CW.
    fn signed_area(ring: &LineString<f64>) -> f64 {
        let pts = &ring.0;
        let mut sum = 0.0;
        for w in pts.windows(2) {
            sum += w[0].x * w[1].y - w[1].x * w[0].y;
        }
        sum / 2.0
    }

    #[test]
    fn build_ring_clockwise_input_is_reoriented_counterclockwise() {
        let coords = vec![c(0.0, 0.0), c(0.0, 4.0), c(4.0, 4.0), c(4.0, 0.0)];
        assert!(signed_area(&LineString::new(coords.clone())) < 0.0);

        let geom = build_ring(coords).expect("should build");
        if let Geometry::Polygon(p) = geom {
            assert!(signed_area(p.exterior()) > 0.0);
        } else {
            panic!("expected Polygon");
        }
    }

    #[test]
    fn build_ring_counterclockwise_input_stays_counterclockwise() {
        let coords = vec![c(0.0, 0.0), c(4.0, 0.0), c(4.0, 4.0), c(0.0, 4.0)];
        assert!(signed_area(&LineString::new(coords.clone())) > 0.0);

        let geom = build_ring(coords).expect("should build");
        if let Geometry::Polygon(p) = geom {
            assert!(signed_area(p.exterior()) > 0.0);
        } else {
            panic!("expected Polygon");
        }
    }

    #[test]
    fn build_ring_bowtie_repair_produces_counterclockwise_pieces() {
        let coords = vec![c(0.0, 0.0), c(0.0, 20.0), c(20.0, 0.0), c(20.0, 20.0)];
        let geom = build_ring(coords).expect("should build");
        if let Geometry::MultiPolygon(mp) = geom {
            for poly in &mp.0 {
                assert!(signed_area(poly.exterior()) > 0.0);
            }
        } else {
            panic!("expected MultiPolygon");
        }
    }
}
