//! Assembles an arbitrarily-ordered bag of rings into a valid `Polygon` or
//! `MultiPolygon`. See [`PolygonAssembler`].

use geo::algorithm::bool_ops::BooleanOps;
use geo::orient::{Direction, Orient};
use geo::{Geometry, LineString, MultiPolygon, Polygon, SimplifyVwPreserve};

use super::{align_to_reference_x, centroid_x};

/// Default cap on total coordinates (summed across every stored ring) a
/// [`PolygonAssembler`] will produce; see
/// [`PolygonAssembler::with_max_coordinates`] to override it.
const DEFAULT_MAX_RING_COORDINATES: usize = 2000;

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
///
/// # Coordinate budget and simplification
/// The total number of coordinates held across all stored rings is kept
/// at or under `max_coordinates` (default
/// [`DEFAULT_MAX_RING_COORDINATES`]) by simplifying stored rings (via
/// `geo`'s topology-preserving `SimplifyVwPreserve`) whenever `add_ring()`
/// would otherwise push the running total over budget — not deferred to
/// `finish()`, for the same reason as `LineStitcher`'s budget: a
/// multipolygon relation with thousands of member ways shouldn't need to
/// hold the whole un-simplified ring set in memory at once. Each ring is
/// simplified independently (as a throwaway single-ring `Polygon`, so the
/// algorithm's own self-intersection guard and its 4-point floor apply),
/// without checking against any other stored ring — a hole simplified
/// this way could in principle drift slightly relative to its shell,
/// analogous to the cross-line caveat already documented for
/// `LineStitcher`.
///
/// `finish()` makes one further simplification pass, this time on the
/// resolved `Polygon`/`MultiPolygon` pieces (exterior and interiors
/// together, so it can't introduce a self-intersection between the two),
/// since the union that resolves fill can introduce new vertices at
/// ring-to-ring intersections that per-ring simplification during
/// `add_ring()` couldn't anticipate.
pub struct PolygonAssembler {
    rings: Vec<LineString<f64>>,
    reference_x: Option<f64>,
    max_coordinates: usize,
    total_coords: usize,
    /// Simplification tolerance, grown monotonically (see `LineStitcher`'s
    /// own `epsilon` field for why).
    epsilon: f64,
}

impl PolygonAssembler {
    pub fn new() -> Self {
        Self::with_max_coordinates(DEFAULT_MAX_RING_COORDINATES)
    }

    pub fn with_max_coordinates(max_coordinates: usize) -> Self {
        Self {
            rings: Vec::new(),
            reference_x: None,
            max_coordinates,
            total_coords: 0,
            epsilon: 1e-7,
        }
    }

    /// Add a ring: a closed `LineString`, or a `Polygon`/`MultiPolygon`
    /// whose exterior and interior rings are flattened in individually.
    /// The running coordinate total is enforced here — see `compact`.
    pub fn add_ring(&mut self, ring: &Geometry<f64>) {
        for mut ls in extract_rings(ring) {
            match self.reference_x {
                Some(rx) => align_to_reference_x(&mut ls, rx),
                None => self.reference_x = Some(centroid_x(&ls)),
            }
            self.total_coords += ls.0.len();
            self.rings.push(ls);
        }

        if self.total_coords > self.max_coordinates {
            self.compact();
        }
    }

    /// Re-simplify every stored ring in place, with progressively larger
    /// tolerance, until the running coordinate total is back at or under
    /// budget, or until no ring can be simplified any further without
    /// shrinking below the 4 coordinates a ring needs (3 distinct vertices
    /// plus the closing repeat).
    ///
    /// Mirrors `LineStitcher`'s own `compact`: runs during `add_ring()`
    /// rather than only in `finish()`, so peak memory stays roughly
    /// proportional to `max_coordinates`, not to the raw input size.
    fn compact(&mut self) {
        for _ in 0..40 {
            if self.total_coords <= self.max_coordinates {
                return;
            }
            let mut new_total = 0;
            let mut can_still_shrink = false;
            for ring in &mut self.rings {
                if ring.0.len() <= 4 {
                    new_total += ring.0.len();
                    continue; // already at the floor: a triangle plus its closing repeat
                }
                can_still_shrink = true;
                // A throwaway single-ring Polygon, so SimplifyVwPreserve's
                // ring-aware 4-point floor applies (the plain LineString
                // impl's floor is 2, which would let a ring collapse to a
                // single degenerate point).
                let poly = Polygon::new(ring.clone(), vec![]);
                let simplified = poly.simplify_vw_preserve(self.epsilon);
                let new_ring = simplified.exterior().clone();
                new_total += new_ring.0.len();
                *ring = new_ring;
            }
            self.total_coords = new_total;
            if !can_still_shrink {
                return; // every ring is down to its 4-point floor already
            }
            self.epsilon *= 2.0;
        }
    }

    /// Resolve everything added so far into a valid geometry.
    /// `None` if nothing was added, or the result has zero area. The total
    /// coordinate count is brought back under the configured budget if the
    /// union that resolves fill left headroom that per-ring simplification
    /// during `add_ring()` couldn't reach.
    pub fn finish(mut self) -> Option<Geometry<f64>> {
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

        let mut polygons = resolved.0;
        let total: usize = polygons.iter().map(polygon_coord_count).sum();
        if total > self.max_coordinates {
            compact_polygons(&mut polygons, self.max_coordinates, &mut self.epsilon);
        }

        let oriented: Vec<Polygon<f64>> = polygons
            .into_iter()
            .map(|p| p.orient(Direction::Default))
            .collect();

        match oriented.len() {
            1 => Some(Geometry::from(oriented.into_iter().next().unwrap())),
            _ => Some(Geometry::from(MultiPolygon::new(oriented))),
        }
    }
}

fn polygon_coord_count(p: &Polygon<f64>) -> usize {
    p.exterior().0.len() + p.interiors().iter().map(|r| r.0.len()).sum::<usize>()
}

/// Re-simplify every polygon in place, with progressively larger
/// tolerance, until the total coordinate count (summed across every
/// exterior and interior ring) is back at or under budget, or until
/// nothing can shrink further. Mirrors [`PolygonAssembler::compact`], but
/// runs on whole `Polygon`s (exterior and interiors together) so it can't
/// introduce a self-intersection between a polygon and its own holes.
fn compact_polygons(polygons: &mut [Polygon<f64>], max_coordinates: usize, epsilon: &mut f64) {
    let mut total: usize = polygons.iter().map(polygon_coord_count).sum();
    for _ in 0..40 {
        if total <= max_coordinates {
            return;
        }
        let mut new_total = 0;
        let mut can_still_shrink = false;
        for p in polygons.iter_mut() {
            let shrinkable =
                p.exterior().0.len() > 4 || p.interiors().iter().any(|r| r.0.len() > 4);
            if shrinkable {
                can_still_shrink = true;
                *p = p.simplify_vw_preserve(*epsilon);
            }
            new_total += polygon_coord_count(p);
        }
        total = new_total;
        if !can_still_shrink {
            return;
        }
        *epsilon *= 2.0;
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

#[cfg(test)]
mod tests {
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

    fn circle_pts(n: usize, cx: f64, cy: f64, radius: f64) -> Vec<(f64, f64)> {
        (0..n)
            .map(|i| {
                let theta = i as f64 / n as f64 * std::f64::consts::TAU;
                (cx + theta.cos() * radius, cy + theta.sin() * radius)
            })
            .collect()
    }

    #[test]
    fn stays_under_budget_for_a_single_oversized_ring() {
        let mut a = PolygonAssembler::with_max_coordinates(100);
        a.add_ring(&ring(&circle_pts(500, 0.0, 0.0, 100.0)));
        match a.finish() {
            Some(Geometry::Polygon(p)) => assert!(p.exterior().0.len() <= 100),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn many_short_disjoint_rings_trigger_incremental_compaction() {
        // 50 separate 50-point circles, far apart -- 2500 coordinates raw,
        // well over a small budget, added one at a time so `add_ring()`'s
        // incremental compaction has to kick in repeatedly rather than
        // only at the end.
        let mut a = PolygonAssembler::with_max_coordinates(300);
        for i in 0..50 {
            let base = (i * 1000) as f64;
            a.add_ring(&ring(&circle_pts(50, base, 0.0, 10.0)));
        }
        let geom = a.finish().expect("should build");
        let count = match &geom {
            Geometry::MultiPolygon(mp) => mp.0.iter().map(polygon_coord_count).sum::<usize>(),
            Geometry::Polygon(p) => polygon_coord_count(p),
            other => panic!("unexpected geometry: {other:?}"),
        };
        // Each of the 50 rings is disjoint and floors out at 4 points, so
        // we can't get below 200 total -- but we should be much closer to
        // that floor than the original 2500.
        assert!(count < 800, "expected substantial reduction, got {count}");
    }
}
