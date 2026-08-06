//! Assembles an arbitrarily-ordered bag of polygons into their plain
//! geometric union, ignoring containment. See [`PolygonUnion`].

use geo::algorithm::bool_ops::BooleanOps;
use geo::orient::{Direction, Orient};
use geo::{Geometry, MultiPolygon, Polygon};

use super::centroid_x;
use super::polygon_assembler::{compact_polygons, polygon_coord_count};

/// Default cap on total coordinates a [`PolygonUnion`] will produce; see
/// [`PolygonUnion::with_max_coordinates`] to override it. Mirrors
/// [`super::PolygonAssembler`]'s own default.
const DEFAULT_MAX_UNION_COORDINATES: usize = 2000;

/// Assembles an arbitrarily-ordered collection of polygons — as would come
/// from the members of an OSM relation whose members simply *are* the
/// area, rather than jointly describing it via role-agnostic containment
/// (e.g. a `type=building` relation, whose `outline` member is the exact
/// union of its `part` members) — into their plain geometric union.
///
/// This is [`PolygonAssembler`](super::PolygonAssembler)'s complement, not
/// a variant of it: `PolygonAssembler` resolves fill from nesting (a ring
/// contained in another becomes a hole), which is the correct rule for
/// `type=multipolygon`/`boundary` relations but actively wrong for
/// anything else — a `building` relation's `part` members are meant to
/// *add* area, not punch holes in the `outline`. `PolygonUnion` instead
/// just unions whatever whole polygons it's given, so a member fully
/// inside another contributes nothing new (rather than becoming a hole)
/// and a member that only partially overlaps extends the total area, same
/// as any other union. See
/// [`GeometryBuilder::new`](super::GeometryBuilder::new) for how a caller
/// picks between the two (this module doesn't know anything about OSM
/// relation types itself -- that's decided by the caller, e.g. from a
/// relation's `type` tag).
///
/// Unlike `PolygonAssembler`, this never flattens a polygon into loose
/// rings: each added `Polygon`'s own holes stay attached to it (they were
/// already resolved correctly, e.g. by a prior `build_ring` or by a
/// recursively-assembled sub-relation), and the union folds whole
/// polygons together via `geo`'s [`BooleanOps::union`].
///
/// # Antimeridian handling
/// Same approach as `PolygonAssembler`: each polygon's rings are assumed
/// to already be internally antimeridian-safe (e.g. via `build_ring`), and
/// are re-aligned via [`align_polygon_to_reference_x`] to the first-added
/// polygon's reference longitude before being folded in.
///
/// # Coordinate budget
/// The running union is kept at or under `max_coordinates` (default
/// [`DEFAULT_MAX_UNION_COORDINATES`]) by simplifying it (via `geo`'s
/// topology-preserving `SimplifyVwPreserve`, reusing
/// `PolygonAssembler`'s own compaction helper) whenever `add()` would
/// otherwise push it over budget, for the same "don't hold an unbounded
/// amount of un-simplified geometry in memory" reason `PolygonAssembler`
/// and `LineStitcher` do this incrementally rather than only in
/// `finish()`.
pub struct PolygonUnion {
    accumulated: MultiPolygon<f64>,
    reference_x: Option<f64>,
    max_coordinates: usize,
    epsilon: f64,
}

impl PolygonUnion {
    pub fn new() -> Self {
        Self::with_max_coordinates(DEFAULT_MAX_UNION_COORDINATES)
    }

    pub fn with_max_coordinates(max_coordinates: usize) -> Self {
        Self {
            accumulated: MultiPolygon::new(Vec::new()),
            reference_x: None,
            max_coordinates,
            epsilon: 1e-7,
        }
    }

    /// Add a `Polygon`/`MultiPolygon` (each polygon's own holes stay
    /// attached to it) to the running union.
    pub fn add(&mut self, geom: &Geometry<f64>) {
        for mut polygon in extract_polygons(geom) {
            match self.reference_x {
                Some(rx) => align_polygon_to_reference_x(&mut polygon, rx),
                None => self.reference_x = Some(centroid_x(polygon.exterior())),
            }
            self.accumulated = self.accumulated.union(&polygon);
        }

        let total: usize = self.accumulated.0.iter().map(polygon_coord_count).sum();
        if total > self.max_coordinates {
            compact_polygons(
                &mut self.accumulated.0,
                self.max_coordinates,
                &mut self.epsilon,
            );
        }
    }

    /// Resolve everything added so far into a valid geometry. `None` if
    /// nothing was added, or the result has zero area.
    pub fn finish(mut self) -> Option<Geometry<f64>> {
        if self.accumulated.0.is_empty() {
            return None;
        }

        let total: usize = self.accumulated.0.iter().map(polygon_coord_count).sum();
        if total > self.max_coordinates {
            compact_polygons(
                &mut self.accumulated.0,
                self.max_coordinates,
                &mut self.epsilon,
            );
        }

        let oriented: Vec<Polygon<f64>> = self
            .accumulated
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

/// Shifts every ring of `polygon` (exterior and interiors alike) by
/// whichever multiple of 360° brings the *exterior's* centroid closest to
/// `reference_x` -- one shift for the whole polygon, so an interior ring
/// stays positioned relative to its own exterior rather than being
/// independently realigned to a possibly different shift.
fn align_polygon_to_reference_x(polygon: &mut Polygon<f64>, reference_x: f64) {
    let cx = centroid_x(polygon.exterior());
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
        polygon.exterior_mut(|ring| {
            for c in ring.0.iter_mut() {
                c.x += best_shift;
            }
        });
        for i in 0..polygon.interiors().len() {
            polygon.interiors_mut(|rings| {
                for c in rings[i].0.iter_mut() {
                    c.x += best_shift;
                }
            });
        }
    }
}

/// Flattens `geom` into its constituent whole polygons, each keeping its
/// own holes. `geom` is always `Geometry::Polygon` or
/// `Geometry::MultiPolygon`, since it can only come from
/// `GeometryBuilder::add`'s own routing (see that type's docs).
fn extract_polygons(geom: &Geometry<f64>) -> Vec<Polygon<f64>> {
    match geom {
        Geometry::Polygon(p) => vec![p.clone()],
        Geometry::MultiPolygon(mp) => mp.0.clone(),
        other => {
            debug_assert!(
                false,
                "PolygonUnion::add called with unsupported geometry: {other:?}"
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{LineString, coord};

    fn ring(pts: &[(f64, f64)]) -> LineString<f64> {
        let mut coords: Vec<_> = pts.iter().map(|&(x, y)| coord! {x: x, y: y}).collect();
        if coords.first() != coords.last() {
            coords.push(coords[0]);
        }
        LineString::new(coords)
    }

    fn polygon(pts: &[(f64, f64)]) -> Geometry<f64> {
        Geometry::from(Polygon::new(ring(pts), vec![]))
    }

    #[test]
    fn empty_returns_none() {
        assert!(PolygonUnion::new().finish().is_none());
    }

    #[test]
    fn single_polygon_returns_itself() {
        let mut u = PolygonUnion::new();
        u.add(&polygon(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]));
        match u.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    /// The scenario from
    /// <https://github.com/alltheplaces/osm-diffs/issues/533>: an
    /// `outline` polygon that's the exact union of several `part`
    /// polygons should union back to the full outline, not (as
    /// `PolygonAssembler`'s nesting rule would do) have the parts punched
    /// out as holes.
    #[test]
    fn polygon_fully_inside_another_contributes_no_hole() {
        let mut u = PolygonUnion::new();
        u.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        u.add(&polygon(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]));
        match u.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected the inner polygon to vanish into the union, got {other:?}"),
        }
    }

    #[test]
    fn parts_exactly_tiling_an_outline_reconstruct_the_outline() {
        // Three "building part" rectangles exactly tiling one "outline"
        // rectangle, split at x=3.3 and x=6.7 -- mirrors #533's actual
        // scenario (an outline that's the geometric union of its parts).
        let mut u = PolygonUnion::new();
        u.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ])); // outline
        u.add(&polygon(&[
            (0.0, 0.0),
            (3.3, 0.0),
            (3.3, 10.0),
            (0.0, 10.0),
        ])); // part 1
        u.add(&polygon(&[
            (3.3, 0.0),
            (6.7, 0.0),
            (6.7, 10.0),
            (3.3, 10.0),
        ])); // part 2
        u.add(&polygon(&[
            (6.7, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (6.7, 10.0),
        ])); // part 3
        match u.finish() {
            Some(Geometry::Polygon(p)) => {
                assert_eq!(p.interiors().len(), 0);
                use geo::Area;
                assert!((p.unsigned_area() - 100.0).abs() < 1e-9);
            }
            other => panic!("expected the reconstructed outline, got {other:?}"),
        }
    }

    #[test]
    fn disjoint_polygons_return_multi_polygon() {
        let mut u = PolygonUnion::new();
        u.add(&polygon(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]));
        u.add(&polygon(&[
            (10.0, 10.0),
            (12.0, 10.0),
            (12.0, 12.0),
            (10.0, 12.0),
        ]));
        match u.finish() {
            Some(Geometry::MultiPolygon(mp)) => assert_eq!(mp.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn overlapping_polygons_merge_into_one_shape() {
        let mut u = PolygonUnion::new();
        u.add(&polygon(&[(0.0, 0.0), (6.0, 0.0), (6.0, 6.0), (0.0, 6.0)]));
        u.add(&polygon(&[(3.0, 3.0), (9.0, 3.0), (9.0, 9.0), (3.0, 9.0)]));
        match u.finish() {
            Some(Geometry::Polygon(p)) => {
                use geo::Area;
                // Two 36-area squares overlapping in a 9-area square: 63 total.
                assert!((p.unsigned_area() - 63.0).abs() < 1e-9);
            }
            other => panic!("expected a merged Polygon, got {other:?}"),
        }
    }

    #[test]
    fn multi_polygon_input_flattens_to_its_constituent_polygons() {
        let mp = Geometry::from(MultiPolygon::new(vec![
            Polygon::new(
                ring(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]),
                vec![],
            ),
            Polygon::new(
                ring(&[(10.0, 10.0), (12.0, 10.0), (12.0, 12.0), (10.0, 12.0)]),
                vec![],
            ),
        ]));
        let mut u = PolygonUnion::new();
        u.add(&mp);
        match u.finish() {
            Some(Geometry::MultiPolygon(m)) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn preexisting_holes_survive_when_disjoint_from_other_members() {
        // A donut (outer with a hole) unioned with a disjoint square: the
        // donut's own hole must survive, since nothing fills it back in.
        let donut = Geometry::from(Polygon::new(
            ring(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)]),
            vec![ring(&[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)])],
        ));
        let mut u = PolygonUnion::new();
        u.add(&donut);
        u.add(&polygon(&[
            (20.0, 20.0),
            (22.0, 20.0),
            (22.0, 22.0),
            (20.0, 22.0),
        ]));
        match u.finish() {
            Some(Geometry::MultiPolygon(mp)) => {
                assert_eq!(mp.0.len(), 2);
                assert!(mp.0.iter().any(|p| p.interiors().len() == 1));
            }
            other => panic!("expected MultiPolygon with the donut's hole intact, got {other:?}"),
        }
    }

    #[test]
    fn antimeridian_polygons_align_to_a_common_frame() {
        let a = polygon(&[(170.0, 0.0), (190.0, 0.0), (190.0, 10.0), (170.0, 10.0)]);
        // Same square really, but expressed the "other way around" the
        // antimeridian, as an independently-unwrapped ring would be.
        let b = polygon(&[(-190.0, 0.0), (-170.0, 0.0), (-170.0, 10.0), (-190.0, 10.0)]);

        let mut u = PolygonUnion::new();
        u.add(&a);
        u.add(&b);
        match u.finish() {
            Some(Geometry::Polygon(p)) => {
                use geo::Area;
                assert!((p.unsigned_area() - 200.0).abs() < 1e-6);
            }
            other => panic!("expected the two squares to align and merge, got {other:?}"),
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
    fn stays_under_budget_for_a_single_oversized_polygon() {
        let mut u = PolygonUnion::with_max_coordinates(100);
        u.add(&polygon(&circle_pts(500, 0.0, 0.0, 100.0)));
        match u.finish() {
            Some(Geometry::Polygon(p)) => assert!(p.exterior().0.len() <= 100),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn many_short_disjoint_polygons_trigger_incremental_compaction() {
        let mut u = PolygonUnion::with_max_coordinates(300);
        for i in 0..50 {
            let base = (i * 1000) as f64;
            u.add(&polygon(&circle_pts(50, base, 0.0, 10.0)));
        }
        let geom = u.finish().expect("should build");
        let count = match &geom {
            Geometry::MultiPolygon(mp) => mp.0.iter().map(polygon_coord_count).sum::<usize>(),
            Geometry::Polygon(p) => polygon_coord_count(p),
            other => panic!("unexpected geometry: {other:?}"),
        };
        assert!(count < 800, "expected substantial reduction, got {count}");
    }
}
