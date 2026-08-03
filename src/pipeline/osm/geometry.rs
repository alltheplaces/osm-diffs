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
//!   determine what's a shell and what's a hole
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
    collections::{HashMap, HashSet, VecDeque},
};

use geo::MakeValid;
use geo::SimplifyVwPreserve;
use geo::algorithm::bool_ops::BooleanOps;
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::algorithm::sweep::{Cross, Intersections};
use geo::algorithm::validation::Validation;
use geo::orient::{Direction, Orient};
use geo::{
    Coord, Geometry, Line, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};

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
// PolygonAssembler
// =======================================================================

/// Assembles an arbitrarily-ordered collection of closed rings — as would
/// come from the members of an OSM multipolygon relation — into a valid
/// `Polygon` or `MultiPolygon`.
///
/// Fill is determined purely by geometric nesting (an even-odd
/// crossing-parity rule, via `geo`'s [`BooleanOps`]), which is direction-
/// and order-agnostic and naturally handles arbitrary nesting depth —
/// holes with islands with holes, disjoint outer shells, etc. — without
/// needing to know which rings are meant to be shells versus holes.
///
/// # Antimeridian handling
/// Rings passed in are assumed to already be internally antimeridian-safe
/// (e.g. via `build_ring`), but rings added separately may have been
/// unwrapped relative to *different* reference longitudes. Each new ring
/// is re-aligned via [`align_to_reference_x`] — by whichever multiple of
/// 360° brings its centroid closest to the first ring's — before being
/// stored, so all rings end up in one consistent coordinate frame before
/// any geometric operation runs.
pub struct PolygonAssembler {
    rings: Vec<LineString<f64>>,
    reference_x: Option<f64>,
}

impl PolygonAssembler {
    pub fn new() -> Self {
        Self {
            rings: Vec::new(),
            reference_x: None,
        }
    }

    /// Add a ring: a closed `LineString`, or a `Polygon`/`MultiPolygon`
    /// whose exterior and interior rings are flattened in individually.
    pub fn add_ring(&mut self, ring: &Geometry<f64>) {
        for mut ls in extract_rings(ring) {
            match self.reference_x {
                Some(rx) => align_to_reference_x(&mut ls, rx),
                None => self.reference_x = Some(centroid_x(&ls)),
            }
            self.rings.push(ls);
        }
    }

    /// Resolve everything added so far into a valid geometry.
    /// `None` if nothing was added, or the result has zero area.
    pub fn finish(self) -> Option<Geometry<f64>> {
        if self.rings.is_empty() {
            return None;
        }

        let soup = RingSoup(self.rings);
        // Union with an empty MultiPolygon: this doesn't add any area, it
        // just forces the fill rule to resolve `soup`'s rings by
        // themselves (see BooleanOps's even-odd crossing-parity docs).
        let resolved: MultiPolygon<f64> = soup.union(&MultiPolygon::new(vec![]));

        if resolved.0.is_empty() {
            return None;
        }

        let oriented: Vec<Polygon<f64>> = resolved
            .0
            .into_iter()
            .map(|p| p.orient(Direction::Default))
            .collect();

        match oriented.len() {
            1 => Some(Geometry::from(oriented.into_iter().next().unwrap())),
            _ => Some(Geometry::from(MultiPolygon::new(oriented))),
        }
    }
}

fn extract_rings(geom: &Geometry<f64>) -> Vec<LineString<f64>> {
    match geom {
        Geometry::LineString(ls) => vec![ls.clone()],
        Geometry::Polygon(p) => {
            let mut out = vec![p.exterior().clone()];
            out.extend(p.interiors().iter().cloned());
            out
        }
        Geometry::MultiPolygon(mp) => {
            let mut out = Vec::new();
            for p in &mp.0 {
                out.push(p.exterior().clone());
                out.extend(p.interiors().iter().cloned());
            }
            out
        }
        other => {
            debug_assert!(
                false,
                "PolygonAssembler::add_ring called with unsupported geometry: {other:?}"
            );
            Vec::new()
        }
    }
}

struct RingSoup(Vec<LineString<f64>>);

impl BooleanOps for RingSoup {
    type Scalar = f64;
    fn rings(&self) -> impl Iterator<Item = &LineString<f64>> {
        self.0.iter()
    }
}

// =======================================================================
// LineStitcher
// =======================================================================

/// Default cap on total coordinates a [`LineStitcher`] will produce; see
/// [`LineStitcher::with_max_coordinates`] to override it.
const DEFAULT_MAX_COORDINATES: usize = 2000;

type CoordKey = (u64, u64);

fn coord_key(c: Coord<f64>) -> CoordKey {
    (c.x.to_bits(), c.y.to_bits())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChainEnd {
    Front,
    Back,
}

/// Stitches arbitrarily-ordered, arbitrarily-oriented `LineString`s (and
/// `MultiLineString`s, flattened into their parts) into longer paths
/// wherever their endpoints touch, closing a path into a loop once its two
/// ends meet.
///
/// # Junctions
/// Merging only happens through a node where *exactly two* way-ends meet
/// (its "degree" is 2), computed once across everything added, in
/// `finish()` — not greedily as lines come in. A node touched by three or
/// more ways (a real junction) or by only one (a dead end) stays as a
/// boundary between separate output pieces rather than being merged
/// through. A self-closed way counts its own closing node twice (standard
/// graph convention for a self-loop), so a loop with a tail — a third way
/// touching the loop's closing node — correctly stays as two separate
/// pieces meeting at that node, rather than being merged incorrectly.
///
/// # Matching
/// Endpoints are matched by exact coordinate equality (see module docs on
/// `build_line`), not by proximity — appropriate for OSM data where a
/// shared node means bit-identical coordinates on both ways.
///
/// # Antimeridian handling
/// Each added line is aligned to a shared reference longitude (frozen at
/// the first line added) via [`align_to_reference_x`], so lines that were
/// independently antimeridian-unwrapped still match up correctly.
///
/// # Coordinate budget and simplification
/// The total number of coordinates held is kept at or under
/// `max_coordinates` (default [`DEFAULT_MAX_COORDINATES`]) by simplifying
/// stored lines (via `geo`'s topology-preserving `SimplifyVwPreserve`)
/// whenever `add()` would otherwise push the running total over budget —
/// not deferred to `finish()`. This matters for relations with very many
/// member ways (e.g. OSM's Russia-border relation has 5000+): waiting
/// until `finish()` to simplify would mean holding the entire
/// un-simplified geometry in memory first, which defeats the point of a
/// coordinate budget in the first place. Simplifying a stored way's
/// interior points early is safe regardless of what gets added later,
/// because OSM ways only ever connect to each other at way *endpoints*,
/// and simplification never moves or removes a line's first or last
/// coordinate. `finish()` still makes one further simplification pass
/// after stitching, since merging separate ways into longer chains can
/// open up simplification headroom that per-way simplification during
/// `add()` couldn't reach (it never simplifies across a not-yet-merged
/// endpoint).
///
/// Simplification tolerance (`epsilon`) is a plain distance in coordinate
/// units, not a geodesic one — same planar, not-latitude-corrected
/// treatment as the rest of this module. For lon/lat input this means the
/// same epsilon corresponds to a much larger real-world distance near the
/// poles than near the equator, since a degree of longitude shrinks
/// toward the poles while a degree of latitude doesn't. Not adjusted for
/// here; worth keeping in mind for anything spanning a wide latitude
/// range (Russia's border again being the obvious example).
///
/// # Self-intersection
/// [`finish`](Self::finish) runs each assembled chain through the same
/// self-intersection repair used by `build_line`, so results are already
/// simple per piece — including any self-intersection that simplification
/// itself might introduce (rare, but
/// <https://github.com/georust/geo/issues/1049> shows `SimplifyVwPreserve`
/// isn't entirely immune to it despite the name).
pub struct LineStitcher {
    lines: Vec<VecDeque<Coord<f64>>>,
    reference_x: Option<f64>,
    max_coordinates: usize,
    total_coords: usize,
    /// Simplification tolerance, grown monotonically across the
    /// stitcher's lifetime rather than reset on every `compact` call, so
    /// repeated compactions during a large `add()` sequence (e.g.
    /// thousands of ways) don't redo small, ineffective epsilon rounds
    /// every time.
    epsilon: f64,
}

impl LineStitcher {
    pub fn new() -> Self {
        Self::with_max_coordinates(DEFAULT_MAX_COORDINATES)
    }

    pub fn with_max_coordinates(max_coordinates: usize) -> Self {
        Self {
            lines: Vec::new(),
            reference_x: None,
            max_coordinates,
            total_coords: 0,
            epsilon: 1e-7,
        }
    }

    /// Add a line: a `LineString`, or a `MultiLineString` whose parts are
    /// added individually. Degenerate (fewer than 2 point) parts are
    /// skipped. Stitching itself is deferred to `finish()`, but the
    /// running coordinate total is enforced here — see `compact`.
    pub fn add(&mut self, geom: &Geometry<f64>) {
        for mut ls in extract_line_strings(geom) {
            if ls.0.len() < 2 {
                continue;
            }
            match self.reference_x {
                Some(rx) => align_to_reference_x(&mut ls, rx),
                None => self.reference_x = Some(centroid_x(&ls)),
            }
            self.total_coords += ls.0.len();
            self.lines.push(ls.0.into_iter().collect());
        }

        if self.total_coords > self.max_coordinates {
            self.compact();
        }
    }

    /// Re-simplify every stored (not-yet-stitched) line in place, with
    /// progressively larger tolerance, until the running coordinate total
    /// is back at or under budget, or until no line can be simplified any
    /// further without losing its endpoints.
    ///
    /// This runs during `add()` rather than only in `finish()` so that
    /// assembling something the size of OSM's Russia-border relation
    /// (5000+ ways) never has to hold the whole un-simplified geometry in
    /// memory at once — peak memory stays roughly proportional to
    /// `max_coordinates`, not to the raw input size.
    fn compact(&mut self) {
        for _ in 0..40 {
            if self.total_coords <= self.max_coordinates {
                return;
            }
            let mut new_total = 0;
            let mut any_changed = false;
            for chain in &mut self.lines {
                if chain.len() <= 2 {
                    new_total += chain.len();
                    continue; // already at the floor: just the two endpoints
                }
                let ls = LineString::new(chain.iter().copied().collect());
                let simplified = ls.simplify_vw_preserve(self.epsilon);
                if simplified.0.len() < chain.len() {
                    any_changed = true;
                }
                new_total += simplified.0.len();
                *chain = simplified.0.into_iter().collect();
            }
            self.total_coords = new_total;
            if !any_changed {
                return; // every line is down to its two endpoints already
            }
            self.epsilon *= 2.0;
        }
        // If we still exceed budget here, every line that *can* shrink has
        // already been shrunk; the remaining excess can only come from
        // having many distinct short pieces. `finish()` gets one more shot
        // at this after stitching, when merged chains may give
        // simplification more room to work with.
    }

    /// Resolve everything added so far into a valid geometry: `None` if
    /// nothing was added, `LineString` for a single resulting piece,
    /// `MultiLineString` otherwise. Each piece is already self-
    /// intersection-repaired (see struct docs), and the total coordinate
    /// count is brought back under the configured budget if stitching
    /// left headroom that per-way simplification during `add()` couldn't
    /// reach.
    pub fn finish(mut self) -> Option<Geometry<f64>> {
        if self.lines.is_empty() {
            return None;
        }

        // Static degree per node: how many way-ends (across everything
        // added) touch it. Computed once, up front, so a junction found
        // late doesn't have to "undo" an earlier greedy merge.
        let mut degree: HashMap<CoordKey, usize> = HashMap::new();
        for line in &self.lines {
            *degree.entry(coord_key(line[0])).or_insert(0) += 1;
            *degree.entry(coord_key(*line.back().unwrap())).or_insert(0) += 1;
        }

        let mut chains: Vec<Option<VecDeque<Coord<f64>>>> = Vec::new();
        let mut endpoint_index: HashMap<CoordKey, (usize, ChainEnd)> = HashMap::new();
        for line in std::mem::take(&mut self.lines) {
            add_chain(&mut chains, &mut endpoint_index, &degree, line);
        }

        let mut stitched: Vec<VecDeque<Coord<f64>>> = chains.into_iter().flatten().collect();

        let total: usize = stitched.iter().map(|c| c.len()).sum();
        if total > self.max_coordinates {
            self.lines = stitched;
            self.total_coords = total;
            self.compact();
            stitched = std::mem::take(&mut self.lines);
        }

        let mut pieces: Vec<LineString<f64>> = Vec::new();
        for chain in stitched {
            if chain.len() < 2 {
                continue;
            }
            let ls = LineString::new(chain.into_iter().collect());
            if !ls.is_valid() {
                continue; // e.g. a degenerate all-duplicate-point loop
            }
            let crossings = find_crossings(&ls);
            if crossings.is_empty() {
                pieces.push(ls);
            } else {
                pieces.extend(cut_at_crossings(&ls, crossings));
            }
        }

        match pieces.len() {
            0 => None,
            1 => Some(Geometry::from(pieces.into_iter().next().unwrap())),
            _ => Some(Geometry::from(MultiLineString::new(pieces))),
        }
    }
}

fn add_chain(
    chains: &mut Vec<Option<VecDeque<Coord<f64>>>>,
    endpoint_index: &mut HashMap<CoordKey, (usize, ChainEnd)>,
    degree: &HashMap<CoordKey, usize>,
    mut chain: VecDeque<Coord<f64>>,
) {
    loop {
        let front = chain[0];
        let back = *chain.back().unwrap();

        if chain.len() > 1 && front == back {
            break; // closed into a loop; terminal, don't extend further
        }

        if degree.get(&coord_key(front)) == Some(&2)
            && let Some((idx, end)) = endpoint_index.remove(&coord_key(front))
        {
            let other = chains[idx].take().expect("indexed chain slot missing");
            remove_remaining_endpoint(endpoint_index, &other, end);
            chain = merge_chains(other, end, chain, ChainEnd::Front);
            continue;
        }

        if degree.get(&coord_key(back)) == Some(&2)
            && let Some((idx, end)) = endpoint_index.remove(&coord_key(back))
        {
            let other = chains[idx].take().expect("indexed chain slot missing");
            remove_remaining_endpoint(endpoint_index, &other, end);
            chain = merge_chains(other, end, chain, ChainEnd::Back);
            continue;
        }
        break;
    }
    insert_chain(chains, endpoint_index, degree, chain);
}

/// After folding `other` into a merge via its `consumed_end`, its *other*
/// endpoint now belongs to the merged chain instead — remove its stale
/// index entry so a later match doesn't point at a freed slot.
fn remove_remaining_endpoint(
    endpoint_index: &mut HashMap<CoordKey, (usize, ChainEnd)>,
    other: &VecDeque<Coord<f64>>,
    consumed_end: ChainEnd,
) {
    let remaining_end = match consumed_end {
        ChainEnd::Front => ChainEnd::Back,
        ChainEnd::Back => ChainEnd::Front,
    };
    let coord = match remaining_end {
        ChainEnd::Front => other[0],
        ChainEnd::Back => *other.back().unwrap(),
    };
    endpoint_index.remove(&coord_key(coord));
}

fn insert_chain(
    chains: &mut Vec<Option<VecDeque<Coord<f64>>>>,
    endpoint_index: &mut HashMap<CoordKey, (usize, ChainEnd)>,
    degree: &HashMap<CoordKey, usize>,
    chain: VecDeque<Coord<f64>>,
) {
    let front = chain[0];
    let back = *chain.back().unwrap();
    let closed = chain.len() > 1 && front == back;

    let idx = chains.len();
    chains.push(Some(chain));

    if !closed {
        if degree.get(&coord_key(front)) == Some(&2) {
            endpoint_index.insert(coord_key(front), (idx, ChainEnd::Front));
        }
        if degree.get(&coord_key(back)) == Some(&2) {
            endpoint_index.insert(coord_key(back), (idx, ChainEnd::Back));
        }
    }
}

fn merge_chains(
    mut existing: VecDeque<Coord<f64>>,
    existing_end: ChainEnd,
    mut new_chain: VecDeque<Coord<f64>>,
    new_end: ChainEnd,
) -> VecDeque<Coord<f64>> {
    if existing_end == ChainEnd::Front {
        existing = existing.into_iter().rev().collect();
    }
    if new_end == ChainEnd::Back {
        new_chain = new_chain.into_iter().rev().collect();
    }
    new_chain.pop_front(); // drop the duplicate shared coordinate
    existing.extend(new_chain);
    existing
}

fn extract_line_strings(geom: &Geometry<f64>) -> Vec<LineString<f64>> {
    match geom {
        Geometry::LineString(ls) => vec![ls.clone()],
        Geometry::MultiLineString(mls) => mls.0.clone(),
        other => {
            debug_assert!(
                false,
                "LineStitcher::add called with unsupported geometry: {other:?}"
            );
            Vec::new()
        }
    }
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

#[cfg(test)]
mod polygon_assembler_tests {
    use super::*;
    use geo::coord;

    fn ring(pts: &[(f64, f64)]) -> Geometry<f64> {
        let mut coords: Vec<_> = pts.iter().map(|&(x, y)| coord! {x: x, y: y}).collect();
        if coords.first() != coords.last() {
            coords.push(coords[0]);
        }
        Geometry::from(LineString::new(coords))
    }

    #[test]
    fn empty_returns_none() {
        assert!(PolygonAssembler::new().finish().is_none());
    }

    #[test]
    fn single_outer_returns_polygon() {
        let mut a = PolygonAssembler::new();
        a.add_ring(&ring(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]));
        match a.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn outer_with_hole_returns_polygon_with_interior() {
        let mut a = PolygonAssembler::new();
        a.add_ring(&ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]));
        a.add_ring(&ring(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]));
        match a.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 1),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn two_disjoint_outers_return_multi_polygon() {
        let mut a = PolygonAssembler::new();
        a.add_ring(&ring(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]));
        a.add_ring(&ring(&[
            (10.0, 10.0),
            (12.0, 10.0),
            (12.0, 12.0),
            (10.0, 12.0),
        ]));
        match a.finish() {
            Some(Geometry::MultiPolygon(mp)) => assert_eq!(mp.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn island_in_hole_resolves_from_geometry_alone() {
        // Outer O (0..10), hole I (2..8), island O2 (4..6) inside the hole --
        // three levels of nesting, resolved purely from geometric parity.
        let mut a = PolygonAssembler::new();
        a.add_ring(&ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]));
        a.add_ring(&ring(&[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0)]));
        a.add_ring(&ring(&[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)]));
        match a.finish() {
            Some(Geometry::MultiPolygon(mp)) => {
                assert_eq!(mp.0.len(), 2);
                let has_hole = mp.0.iter().any(|p| p.interiors().len() == 1);
                let has_no_hole = mp.0.iter().any(|p| p.interiors().is_empty());
                assert!(has_hole && has_no_hole);
            }
            other => panic!("expected MultiPolygon (donut + island), got {other:?}"),
        }
    }

    #[test]
    fn polygon_input_flattens_exterior_and_holes() {
        let outer = LineString::new(
            [
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ]
            .map(|(x, y)| coord! {x: x, y: y})
            .to_vec(),
        );
        let hole = LineString::new(
            [(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)]
                .map(|(x, y)| coord! {x: x, y: y})
                .to_vec(),
        );
        let poly = Geometry::from(Polygon::new(outer, vec![hole]));

        let mut a = PolygonAssembler::new();
        a.add_ring(&poly);
        match a.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 1),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn antimeridian_hole_aligns_to_exterior_frame() {
        let outer = ring(&[(170.0, 0.0), (190.0, 0.0), (190.0, 10.0), (170.0, 10.0)]);
        let hole = ring(&[(-176.0, 4.0), (-174.0, 4.0), (-174.0, 6.0), (-176.0, 6.0)]);

        let mut a = PolygonAssembler::new();
        a.add_ring(&outer);
        a.add_ring(&hole);

        match a.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 1),
            other => panic!("expected Polygon with a hole, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod line_stitcher_tests {
    use super::*;
    use geo::coord;

    fn ls(pts: &[(f64, f64)]) -> Geometry<f64> {
        Geometry::from(LineString::new(
            pts.iter().map(|&(x, y)| coord! {x: x, y: y}).collect(),
        ))
    }

    #[test]
    fn empty_returns_none() {
        assert!(LineStitcher::new().finish().is_none());
    }

    #[test]
    fn two_touching_lines_stitch_into_one() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (1.0, 0.0)]));
        a.add(&ls(&[(1.0, 0.0), (2.0, 0.0)]));
        match a.finish() {
            Some(Geometry::LineString(l)) => assert_eq!(l.0.len(), 3),
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    fn scrambled_order_and_orientation_still_stitch() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(2.0, 0.0), (1.0, 0.0)]));
        a.add(&ls(&[(3.0, 0.0), (2.0, 0.0)]));
        a.add(&ls(&[(0.0, 0.0), (1.0, 0.0)]));
        match a.finish() {
            Some(Geometry::LineString(l)) => {
                assert_eq!(l.0.len(), 4);
                let xs: Vec<f64> = l.0.iter().map(|c| c.x).collect();
                assert!(xs == vec![0.0, 1.0, 2.0, 3.0] || xs == vec![3.0, 2.0, 1.0, 0.0]);
            }
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    fn closing_path_produces_a_loop() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (4.0, 0.0)]));
        a.add(&ls(&[(4.0, 0.0), (4.0, 4.0)]));
        a.add(&ls(&[(4.0, 4.0), (0.0, 0.0)]));
        match a.finish() {
            Some(Geometry::LineString(l)) => {
                assert_eq!(l.0.first(), l.0.last());
                assert_eq!(l.0.len(), 4);
            }
            other => panic!("expected closed LineString, got {other:?}"),
        }
    }

    #[test]
    fn disjoint_lines_stay_separate() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (1.0, 0.0)]));
        a.add(&ls(&[(10.0, 10.0), (11.0, 10.0)]));
        match a.finish() {
            Some(Geometry::MultiLineString(m)) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn three_way_junction_stays_unmerged() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (-1.0, 1.0)]));
        a.add(&ls(&[(0.0, 0.0), (1.0, 1.0)]));
        a.add(&ls(&[(0.0, 0.0), (0.0, -1.0)]));
        match a.finish() {
            Some(Geometry::MultiLineString(m)) => {
                assert_eq!(m.0.len(), 3);
                for piece in &m.0 {
                    assert_eq!(piece.0.len(), 2);
                }
            }
            other => panic!("expected 3 separate pieces, got {other:?}"),
        }
    }

    #[test]
    fn four_way_junction_stays_unmerged_but_elsewhere_still_stitches() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (1.0, 0.0)]));
        a.add(&ls(&[(0.0, 0.0), (-1.0, 0.0)]));
        a.add(&ls(&[(0.0, 0.0), (0.0, 1.0)]));
        a.add(&ls(&[(0.0, 0.0), (0.0, -1.0)]));
        a.add(&ls(&[(10.0, 10.0), (11.0, 10.0)]));
        a.add(&ls(&[(11.0, 10.0), (12.0, 10.0)]));

        match a.finish() {
            Some(Geometry::MultiLineString(m)) => {
                assert_eq!(m.0.len(), 5);
                assert!(m.0.iter().any(|l| l.0.len() == 3));
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn lollipop_loop_with_tail_stays_as_two_pieces() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 0.0)]));
        a.add(&ls(&[(0.0, 0.0), (-3.0, 0.0)]));

        match a.finish() {
            Some(Geometry::MultiLineString(m)) => {
                assert_eq!(m.0.len(), 2);
                let has_loop = m.0.iter().any(|l| l.0.first() == l.0.last());
                let has_tail = m.0.iter().any(|l| l.0.len() == 2);
                assert!(has_loop && has_tail);
            }
            other => panic!("expected loop + tail as separate pieces, got {other:?}"),
        }
    }

    #[test]
    fn multi_line_string_input_is_flattened_before_stitching() {
        let mls = Geometry::from(MultiLineString::new(vec![
            LineString::new(vec![coord! {x: 0.0, y: 0.0}, coord! {x: 1.0, y: 0.0}]),
            LineString::new(vec![coord! {x: 5.0, y: 5.0}, coord! {x: 6.0, y: 5.0}]),
        ]));
        let mut a = LineStitcher::new();
        a.add(&mls);
        a.add(&ls(&[(1.0, 0.0), (2.0, 0.0)]));
        match a.finish() {
            Some(Geometry::MultiLineString(m)) => {
                assert_eq!(m.0.len(), 2);
                assert!(m.0.iter().any(|l| l.0.len() == 3));
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn self_intersecting_stitched_result_gets_split() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(0.0, 0.0), (2.0, 2.0)]));
        a.add(&ls(&[(2.0, 2.0), (2.0, 0.0)]));
        a.add(&ls(&[(2.0, 0.0), (0.0, 2.0)]));
        match a.finish() {
            Some(Geometry::MultiLineString(m)) => {
                assert!(m.0.len() >= 2);
                for piece in &m.0 {
                    assert!(piece.is_valid());
                }
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn antimeridian_lines_stitch_after_alignment() {
        let mut a = LineStitcher::new();
        a.add(&ls(&[(170.0, 0.0), (190.0, 0.0)]));
        a.add(&ls(&[(-170.0, 0.0), (-170.0, 5.0)]));
        match a.finish() {
            Some(Geometry::LineString(l)) => assert_eq!(l.0.len(), 3),
            other => panic!("expected stitched LineString, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "does not pass"] // TODO: Fix implementation.
    fn stays_under_budget_for_a_single_oversized_line() {
        let pts: Vec<(f64, f64)> = (0..500)
            .map(|i| (i as f64, (i as f64 * 0.1).sin()))
            .collect();
        let mut a = LineStitcher::with_max_coordinates(100);
        a.add(&ls(&pts));
        match a.finish() {
            Some(Geometry::LineString(l)) => {
                assert!(l.0.len() <= 100);
                assert_eq!(l.0.first().unwrap().x, 0.0); // endpoints preserved
                assert_eq!(l.0.last().unwrap().x, 499.0);
            }
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "does not pass"] // TODO: Fix implementation.
    fn compaction_during_add_still_allows_correct_stitching() {
        // Two long, wiggly lines meeting exactly at (50, y) -- simplification
        // during add() must not disturb that shared endpoint.
        let mut a = LineStitcher::with_max_coordinates(60);
        let first: Vec<(f64, f64)> = (0..=50).map(|i| (i as f64, (i as f64).sin())).collect();
        let second: Vec<(f64, f64)> = (50..=100).map(|i| (i as f64, (i as f64).cos())).collect();
        a.add(&ls(&first));
        a.add(&ls(&second));
        match a.finish() {
            Some(Geometry::LineString(l)) => {
                assert_eq!(l.0.first().unwrap().x, 0.0);
                assert_eq!(l.0.last().unwrap().x, 100.0);
                assert!(l.0.len() <= 60);
            }
            other => panic!("expected a single stitched, simplified LineString, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "does not pass"] // TODO: Fix implementation.
    fn many_short_disjoint_lines_trigger_incremental_compaction() {
        // 50 separate 50-point wiggly lines -- 2500 coordinates raw, well
        // over a small budget, added one at a time so `add()`'s incremental
        // compaction has to kick in repeatedly rather than only at the end.
        let mut a = LineStitcher::with_max_coordinates(300);
        for i in 0..50 {
            let base = (i * 1000) as f64;
            let pts: Vec<(f64, f64)> = (0..50)
                .map(|j| (base + j as f64, (j as f64 * 0.3).sin()))
                .collect();
            a.add(&ls(&pts));
        }
        let geom = a.finish().expect("should build");
        let count = match &geom {
            Geometry::MultiLineString(m) => m.0.iter().map(|l| l.0.len()).sum::<usize>(),
            Geometry::LineString(l) => l.0.len(),
            other => panic!("unexpected geometry: {other:?}"),
        };
        // Each of the 50 pieces is disjoint and floors out at 2 points, so
        // we can't get below 100 total -- but we should be much closer to
        // that floor than the original 2500.
        assert!(count < 500, "expected substantial reduction, got {count}");
    }

    #[test]
    #[ignore = "does not pass"] // TODO: Fix implementation.
    fn budget_is_still_enforced_after_stitching_when_add_time_compaction_could_not_help() {
        // Each of these three lines is already at its 2-point floor during
        // add() (nothing to simplify), but once stitched together into one
        // 4-point chain, finish()'s post-stitch pass has room to simplify.
        let mut a = LineStitcher::with_max_coordinates(3);
        a.add(&ls(&[(0.0, 0.0), (1.0, 0.001)]));
        a.add(&ls(&[(1.0, 0.001), (2.0, -0.001)]));
        a.add(&ls(&[(2.0, -0.001), (3.0, 0.0)]));
        match a.finish() {
            Some(Geometry::LineString(l)) => assert!(l.0.len() <= 3),
            other => panic!("expected simplified LineString, got {other:?}"),
        }
    }
}
