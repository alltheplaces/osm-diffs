//! Accumulates a mixed, arbitrarily-ordered stream of `geo` geometries
//! into one combined geometry. See [`GeometryBuilder`].

use geo::algorithm::bool_ops::BooleanOps;
use geo::algorithm::intersects::Intersects;
use geo::{Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPolygon, Point};

use super::{LineStitcher, PolygonAssembler, PolygonUnion, build_points};

/// Accumulates an arbitrarily-typed, arbitrarily-ordered stream of `geo`
/// geometries — as would come from resolving the members of an OSM
/// relation of unknown or mixed geometry type — and assembles them into
/// one combined geometry.
///
/// Each added geometry is routed by kind:
/// * `LineString`/`MultiLineString` (and, degenerately, `Line`) go to an
///   internal [`LineStitcher`], which stitches touching endpoints into
///   longer paths.
/// * `Polygon`/`MultiPolygon` (and, degenerately, `Rect`/`Triangle`) go to
///   an internal polygon accumulator — either a [`PolygonAssembler`],
///   which resolves fill from geometric nesting, or a [`PolygonUnion`],
///   which just unions whole polygons together ignoring containment.
///   Which one is picked at construction time via [`PolygonFill`] (see
///   [`with_polygon_fill`](Self::with_polygon_fill)); containment is the
///   default and the only option plain `new()` gives you.
/// * `Point`/`MultiPoint` coordinates are simply collected; there's
///   nothing to stitch or nest, so no assembler is needed for them.
/// * `GeometryCollection` is flattened: each member is added individually.
///
/// # Result shape
/// [`finish`](Self::finish) combines whatever was added into the smallest
/// geometry type that represents it:
/// * Only one kind was added (the common case: an OSM multipolygon
///   relation is *all* rings, a route relation is *all* ways) — its
///   assembler's own `finish()` result is returned directly, doing no
///   extra work at all.
/// * More than one kind was added — the result is a [`Geometry::GeometryCollection`]
///   of the non-empty parts (polygon area first, then line, then points),
///   after two corrections so the combination is a true set-theoretic
///   union rather than a naive bag of parts:
///     1. Any part of the stitched lines that falls inside the assembled
///        polygon area is clipped away (via `geo`'s [`BooleanOps::clip`]):
///        that area is already part of the union through the polygon, so
///        keeping it in the line piece too would just be a redundant
///        (and, for consumers expecting a partition, confusing) overlap.
///     2. Any point that lies on the (clipped) lines or on/in the polygon
///        area is dropped for the same reason: it contributes no new
///        territory to the union.
///
/// This makes the common single-kind case allocation-free beyond what the
/// relevant assembler already does, while staying correct — if unavoidably
/// pricier, due to the clip and containment checks — for the rarer mixed
/// case.
///
/// # Antimeridian handling
/// Delegated entirely to the inner [`LineStitcher`] and polygon
/// accumulator, each of which aligns everything it receives to its own
/// first-added reference longitude. Points are not antimeridian-aligned:
/// OSM node coordinates are never split across the dateline the way a
/// way's vertices can be, so there's nothing to align them *to*.
pub struct GeometryBuilder {
    lines: LineStitcher,
    polygons: PolygonAccumulator,
    points: Vec<Coord<f64>>,
    has_lines: bool,
    has_polygons: bool,
}

/// How [`GeometryBuilder`] should combine `Polygon`/`MultiPolygon`
/// members into one shape; see
/// [`GeometryBuilder::with_polygon_fill`]. Purely a geometric choice --
/// callers are expected to have already decided, from whatever
/// domain-specific knowledge applies (e.g. an OSM relation's `type` tag),
/// which one describes their input.
pub enum PolygonFill {
    /// Resolve fill by geometric nesting (a member contained in another
    /// becomes a hole) -- naturally handles arbitrary nesting depth (holes
    /// with islands with holes, disjoint outer shells, ...) without
    /// needing to know which member is meant to be a shell versus a hole.
    /// Correct when every member is meant to jointly describe one area via
    /// containment (e.g. OSM's `multipolygon`/`boundary` relations, where
    /// declared roles are advisory and nesting is what actually decides
    /// shell vs. hole).
    Containment,
    /// Union every member together, ignoring containment: a member fully
    /// inside another contributes nothing new, rather than becoming a
    /// hole. Correct when members simply *are* the area rather than
    /// jointly describing it via nesting (e.g. an OSM `building` relation,
    /// whose `outline` member is the exact union of its `part` members --
    /// containment fill would wrongly punch the parts out as holes).
    Union,
}

/// Which of the two polygon accumulators [`GeometryBuilder`] routes
/// `Polygon`/`MultiPolygon` input to, per the requested [`PolygonFill`].
enum PolygonAccumulator {
    Containment(PolygonAssembler),
    Union(PolygonUnion),
}

impl PolygonAccumulator {
    fn add(&mut self, g: &Geometry<f64>) {
        match self {
            PolygonAccumulator::Containment(a) => a.add_ring(g),
            PolygonAccumulator::Union(u) => u.add(g),
        }
    }

    fn finish(self) -> Option<Geometry<f64>> {
        match self {
            PolygonAccumulator::Containment(a) => a.finish(),
            PolygonAccumulator::Union(u) => u.finish(),
        }
    }
}

impl GeometryBuilder {
    /// Same as [`with_polygon_fill`](Self::with_polygon_fill)`(`[`PolygonFill::Containment`]`)`.
    pub fn new() -> Self {
        Self::with_polygon_fill(PolygonFill::Containment)
    }

    /// Like [`new`](Self::new), but lets the caller pick how
    /// `Polygon`/`MultiPolygon` members are combined -- see [`PolygonFill`].
    pub fn with_polygon_fill(fill: PolygonFill) -> Self {
        let polygons = match fill {
            PolygonFill::Containment => PolygonAccumulator::Containment(PolygonAssembler::new()),
            PolygonFill::Union => PolygonAccumulator::Union(PolygonUnion::new()),
        };
        Self {
            lines: LineStitcher::new(),
            polygons,
            points: Vec::new(),
            has_lines: false,
            has_polygons: false,
        }
    }

    /// Add one geometry of any kind, routing it to the matching
    /// accumulator (see struct docs). A `GeometryCollection` is flattened
    /// by adding each of its members individually.
    pub fn add(&mut self, g: &Geometry<f64>) {
        match g {
            Geometry::LineString(_) | Geometry::MultiLineString(_) => {
                self.has_lines = true;
                self.lines.add(g);
            }
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => {
                self.has_polygons = true;
                self.polygons.add(g);
            }
            Geometry::Point(p) => self.points.push(p.0),
            Geometry::MultiPoint(mp) => self.points.extend(mp.0.iter().map(|p| p.0)),
            Geometry::Line(line) => {
                self.has_lines = true;
                self.lines
                    .add(&Geometry::from(LineString::new(vec![line.start, line.end])));
            }
            Geometry::Triangle(t) => {
                self.has_polygons = true;
                self.polygons.add(&Geometry::from((*t).to_polygon()));
            }
            Geometry::Rect(r) => {
                self.has_polygons = true;
                self.polygons.add(&Geometry::from((*r).to_polygon()));
            }
            Geometry::GeometryCollection(gc) => {
                for geom in &gc.0 {
                    self.add(geom);
                }
            }
        }
    }

    /// Resolve everything added so far into a single combined geometry.
    /// `None` if nothing was added. See struct docs for how mixed-kind
    /// input is combined.
    pub fn finish(self) -> Option<Geometry<f64>> {
        let has_points = !self.points.is_empty();

        // Fast path: only one kind was ever added, so there's nothing to
        // combine and no need to even look at the other two accumulators.
        match (self.has_lines, self.has_polygons, has_points) {
            (false, false, false) => return None,
            (true, false, false) => return self.lines.finish(),
            (false, true, false) => return self.polygons.finish(),
            (false, false, true) => return build_points(self.points),
            _ => {}
        }

        let mut line_geom = if self.has_lines {
            self.lines.finish()
        } else {
            None
        };
        let poly_geom = if self.has_polygons {
            self.polygons.finish()
        } else {
            None
        };

        // Clip away whatever part of the stitched lines already falls
        // inside the assembled polygon area, so it isn't double-counted as
        // separate territory in the union.
        if let (Some(lg), Some(pg)) = (&line_geom, &poly_geom) {
            let outside = multi_polygon_of(pg).clip(&multi_line_string_of(lg), true);
            line_geom = match outside.0.len() {
                0 => None,
                1 => Some(Geometry::from(outside.0.into_iter().next().unwrap())),
                _ => Some(Geometry::from(outside)),
            };
        }

        // Drop any point that contributes no new territory: one already on
        // the (clipped) lines, or on/in the polygon area.
        let points_geom = if has_points {
            let mut coords = self.points;
            coords.retain(|c| {
                let p = Point::from(*c);
                !line_geom.as_ref().is_some_and(|g| g.intersects(&p))
                    && !poly_geom.as_ref().is_some_and(|g| g.intersects(&p))
            });
            build_points(coords)
        } else {
            None
        };

        let parts: Vec<Geometry<f64>> = [poly_geom, line_geom, points_geom]
            .into_iter()
            .flatten()
            .collect();

        match parts.len() {
            0 => None,
            1 => Some(parts.into_iter().next().unwrap()),
            _ => Some(Geometry::GeometryCollection(GeometryCollection::new_from(
                parts,
            ))),
        }
    }
}

/// `pg` as a `MultiPolygon`, for feeding into [`BooleanOps::clip`]; `pg` is
/// always [`Geometry::Polygon`] or [`Geometry::MultiPolygon`], since it can
/// only come from [`PolygonAssembler::finish`].
fn multi_polygon_of(pg: &Geometry<f64>) -> MultiPolygon<f64> {
    match pg {
        Geometry::Polygon(p) => MultiPolygon::new(vec![p.clone()]),
        Geometry::MultiPolygon(mp) => mp.clone(),
        other => unreachable!("PolygonAssembler::finish returned {other:?}"),
    }
}

/// `lg` as a `MultiLineString`, for feeding into [`BooleanOps::clip`]; `lg`
/// is always [`Geometry::LineString`] or [`Geometry::MultiLineString`],
/// since it can only come from [`LineStitcher::finish`].
fn multi_line_string_of(lg: &Geometry<f64>) -> MultiLineString<f64> {
    match lg {
        Geometry::LineString(ls) => MultiLineString::new(vec![ls.clone()]),
        Geometry::MultiLineString(mls) => mls.clone(),
        other => unreachable!("LineStitcher::finish returned {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Line, MultiPoint, Polygon, Rect, Triangle, coord};

    fn c(x: f64, y: f64) -> Coord<f64> {
        coord! { x: x, y: y }
    }

    fn ls(pts: &[(f64, f64)]) -> Geometry<f64> {
        Geometry::from(LineString::new(pts.iter().map(|&(x, y)| c(x, y)).collect()))
    }

    /// Unlike [`PolygonAssembler::add_ring`], `GeometryBuilder::add` routes
    /// purely by `Geometry` variant, so a polygon has to actually be a
    /// `Geometry::Polygon` here rather than a bare closed `LineString`
    /// (which `add` would -- correctly -- treat as a line).
    fn polygon(pts: &[(f64, f64)]) -> Geometry<f64> {
        let mut coords: Vec<_> = pts.iter().map(|&(x, y)| c(x, y)).collect();
        if coords.first() != coords.last() {
            coords.push(coords[0]);
        }
        Geometry::from(Polygon::new(LineString::new(coords), vec![]))
    }

    fn point(x: f64, y: f64) -> Geometry<f64> {
        Geometry::from(Point::new(x, y))
    }

    #[test]
    fn empty_returns_none() {
        assert!(GeometryBuilder::new().finish().is_none());
    }

    #[test]
    fn only_lines_takes_the_line_stitcher_fast_path() {
        let mut b = GeometryBuilder::new();
        b.add(&ls(&[(0.0, 0.0), (1.0, 0.0)]));
        b.add(&ls(&[(1.0, 0.0), (2.0, 0.0)]));
        match b.finish() {
            Some(Geometry::LineString(l)) => assert_eq!(l.0.len(), 3),
            other => panic!("expected stitched LineString, got {other:?}"),
        }
    }

    #[test]
    fn only_polygons_takes_the_polygon_assembler_fast_path() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]));
        match b.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn only_points_returns_multi_point() {
        let mut b = GeometryBuilder::new();
        b.add(&point(0.0, 0.0));
        b.add(&point(1.0, 1.0));
        match b.finish() {
            Some(Geometry::MultiPoint(mp)) => assert_eq!(mp.len(), 2),
            other => panic!("expected MultiPoint, got {other:?}"),
        }
    }

    #[test]
    fn multi_point_input_is_flattened() {
        let mp = Geometry::from(MultiPoint::new(vec![
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
        ]));
        let mut b = GeometryBuilder::new();
        b.add(&mp);
        match b.finish() {
            Some(Geometry::MultiPoint(mp)) => assert_eq!(mp.len(), 2),
            other => panic!("expected MultiPoint, got {other:?}"),
        }
    }

    #[test]
    fn multi_line_string_and_multi_polygon_inputs_are_routed_correctly() {
        let mls = Geometry::from(MultiLineString::new(vec![
            LineString::new(vec![c(0.0, 0.0), c(1.0, 0.0)]),
            LineString::new(vec![c(5.0, 5.0), c(6.0, 5.0)]),
        ]));
        let mut lines_only = GeometryBuilder::new();
        lines_only.add(&mls);
        match lines_only.finish() {
            Some(Geometry::MultiLineString(m)) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiLineString, got {other:?}"),
        }

        let mpoly = Geometry::from(MultiPolygon::new(vec![
            Polygon::new(
                LineString::new(vec![
                    c(0.0, 0.0),
                    c(2.0, 0.0),
                    c(2.0, 2.0),
                    c(0.0, 2.0),
                    c(0.0, 0.0),
                ]),
                vec![],
            ),
            Polygon::new(
                LineString::new(vec![
                    c(10.0, 10.0),
                    c(12.0, 10.0),
                    c(12.0, 12.0),
                    c(10.0, 12.0),
                    c(10.0, 10.0),
                ]),
                vec![],
            ),
        ]));
        let mut polys_only = GeometryBuilder::new();
        polys_only.add(&mpoly);
        match polys_only.finish() {
            Some(Geometry::MultiPolygon(mp)) => assert_eq!(mp.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn disjoint_line_and_polygon_combine_into_geometry_collection() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]));
        b.add(&ls(&[(10.0, 10.0), (11.0, 10.0)]));
        match b.finish() {
            Some(Geometry::GeometryCollection(gc)) => {
                assert_eq!(gc.0.len(), 2);
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Polygon(_))));
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::LineString(_))));
            }
            other => panic!("expected GeometryCollection, got {other:?}"),
        }
    }

    #[test]
    fn line_fully_inside_polygon_is_clipped_away_entirely() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        b.add(&ls(&[(2.0, 2.0), (8.0, 8.0)])); // wholly inside, touches no boundary
        match b.finish() {
            // The line contributes no new territory, so only the polygon survives
            // -- and since only one part is left, it's returned unwrapped rather
            // than as a 1-element GeometryCollection.
            Some(Geometry::Polygon(_)) => {}
            other => panic!("expected the line to be clipped away, got {other:?}"),
        }
    }

    #[test]
    fn line_crossing_polygon_boundary_keeps_only_its_outside_portion() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        // Crosses clean through the square at y=5: outside on both ends.
        b.add(&ls(&[(-5.0, 5.0), (15.0, 5.0)]));
        match b.finish() {
            Some(Geometry::GeometryCollection(gc)) => {
                assert_eq!(gc.0.len(), 2);
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Polygon(_))));
                let remaining_line_coords: usize =
                    gc.0.iter()
                        .map(|g| match g {
                            Geometry::LineString(l) => l.0.len(),
                            Geometry::MultiLineString(m) => m.0.iter().map(|l| l.0.len()).sum(),
                            _ => 0,
                        })
                        .sum();
                // Both outside segments ((-5,5)-(0,5) and (10,5)-(15,5)) should
                // remain; none of the coordinates should fall strictly inside
                // the square.
                assert!(remaining_line_coords >= 4);
                for g in &gc.0 {
                    let coords: Vec<Coord<f64>> = match g {
                        Geometry::LineString(l) => l.0.clone(),
                        Geometry::MultiLineString(m) => {
                            m.0.iter().flat_map(|l| l.0.clone()).collect()
                        }
                        _ => continue,
                    };
                    for c in coords {
                        assert!(
                            c.x <= 0.0 || c.x >= 10.0,
                            "expected only the outside portion, found {c:?}"
                        );
                    }
                }
            }
            other => panic!("expected GeometryCollection, got {other:?}"),
        }
    }

    #[test]
    fn point_inside_polygon_is_omitted() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        b.add(&point(5.0, 5.0));
        match b.finish() {
            Some(Geometry::Polygon(_)) => {}
            other => panic!("expected the point to be omitted, got {other:?}"),
        }
    }

    #[test]
    fn point_outside_polygon_is_kept() {
        let mut b = GeometryBuilder::new();
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        b.add(&point(50.0, 50.0));
        match b.finish() {
            Some(Geometry::GeometryCollection(gc)) => {
                assert_eq!(gc.0.len(), 2);
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Polygon(_))));
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Point(_))));
            }
            other => panic!("expected GeometryCollection, got {other:?}"),
        }
    }

    #[test]
    fn point_on_line_is_omitted() {
        let mut b = GeometryBuilder::new();
        b.add(&ls(&[(0.0, 0.0), (10.0, 0.0)]));
        b.add(&point(5.0, 0.0)); // sits exactly on the line
        match b.finish() {
            Some(Geometry::LineString(_)) => {}
            other => panic!("expected the point to be omitted, got {other:?}"),
        }
    }

    #[test]
    fn point_off_line_is_kept() {
        let mut b = GeometryBuilder::new();
        b.add(&ls(&[(0.0, 0.0), (10.0, 0.0)]));
        b.add(&point(5.0, 5.0));
        match b.finish() {
            Some(Geometry::GeometryCollection(gc)) => {
                assert_eq!(gc.0.len(), 2);
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::LineString(_))));
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Point(_))));
            }
            other => panic!("expected GeometryCollection, got {other:?}"),
        }
    }

    #[test]
    fn geometry_collection_input_is_flattened() {
        let gc = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            ls(&[(10.0, 10.0), (11.0, 10.0)]),
            polygon(&[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)]),
        ]));
        let mut b = GeometryBuilder::new();
        b.add(&gc);
        match b.finish() {
            Some(Geometry::GeometryCollection(gc)) => {
                assert_eq!(gc.0.len(), 2);
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::Polygon(_))));
                assert!(gc.0.iter().any(|g| matches!(g, Geometry::LineString(_))));
            }
            other => panic!("expected GeometryCollection, got {other:?}"),
        }
    }

    #[test]
    fn line_variant_is_routed_to_line_stitcher() {
        let mut b = GeometryBuilder::new();
        b.add(&Geometry::from(Line::new(c(0.0, 0.0), c(1.0, 0.0))));
        b.add(&Geometry::from(Line::new(c(1.0, 0.0), c(2.0, 0.0))));
        match b.finish() {
            Some(Geometry::LineString(l)) => assert_eq!(l.0.len(), 3),
            other => panic!("expected stitched LineString, got {other:?}"),
        }
    }

    #[test]
    fn rect_variant_is_routed_to_polygon_assembler() {
        let mut b = GeometryBuilder::new();
        b.add(&Geometry::from(Rect::new(c(0.0, 0.0), c(4.0, 4.0))));
        match b.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn triangle_variant_is_routed_to_polygon_assembler() {
        let mut b = GeometryBuilder::new();
        b.add(&Geometry::from(Triangle::new(
            c(0.0, 0.0),
            c(4.0, 0.0),
            c(0.0, 4.0),
        )));
        match b.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 0),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn with_polygon_fill_containment_nests_the_inner_polygon() {
        let mut b = GeometryBuilder::with_polygon_fill(PolygonFill::Containment);
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]));
        b.add(&polygon(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]));
        match b.finish() {
            Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 1),
            other => panic!("expected Polygon with a hole, got {other:?}"),
        }
    }

    /// The https://github.com/alltheplaces/osm-diffs/issues/533 scenario,
    /// end to end through `GeometryBuilder`: an "outline" member that's
    /// the exact union of the other members should reconstruct the full
    /// outline under `PolygonFill::Union`, not (as `Containment` would)
    /// come back empty.
    #[test]
    fn with_polygon_fill_union_reconstructs_tiling_parts() {
        let mut b = GeometryBuilder::with_polygon_fill(PolygonFill::Union);
        b.add(&polygon(&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ])); // outline
        b.add(&polygon(&[
            (0.0, 0.0),
            (5.0, 0.0),
            (5.0, 10.0),
            (0.0, 10.0),
        ])); // part 1
        b.add(&polygon(&[
            (5.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (5.0, 10.0),
        ])); // part 2
        match b.finish() {
            Some(Geometry::Polygon(p)) => {
                assert_eq!(p.interiors().len(), 0);
                use geo::Area;
                assert!((p.unsigned_area() - 100.0).abs() < 1e-9);
            }
            other => panic!("expected the reconstructed outline, got {other:?}"),
        }
    }

    #[test]
    fn new_matches_with_polygon_fill_containment() {
        let mut a = GeometryBuilder::new();
        let mut b = GeometryBuilder::with_polygon_fill(PolygonFill::Containment);
        for builder in [&mut a, &mut b] {
            builder.add(&polygon(&[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
            ]));
            builder.add(&polygon(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0)]));
        }
        for b in [a, b] {
            match b.finish() {
                Some(Geometry::Polygon(p)) => assert_eq!(p.interiors().len(), 1),
                other => panic!("expected Polygon with a hole, got {other:?}"),
            }
        }
    }
}
