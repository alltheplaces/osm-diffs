//! Stitches arbitrarily-ordered, arbitrarily-oriented `LineString`s into
//! longer paths wherever their endpoints touch. See [`LineStitcher`].

use std::collections::{HashMap, VecDeque};

use geo::algorithm::validation::Validation;
use geo::{Coord, Geometry, LineString, MultiLineString, SimplifyVwPreserve};

use super::{align_to_reference_x, centroid_x, cut_at_crossings, find_crossings};

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
            let mut can_still_shrink = false;
            for chain in &mut self.lines {
                if chain.len() <= 2 {
                    new_total += chain.len();
                    continue; // already at the floor: just the two endpoints
                }
                can_still_shrink = true;
                let ls = LineString::new(chain.iter().copied().collect());
                let simplified = ls.simplify_vw_preserve(self.epsilon);
                new_total += simplified.0.len();
                *chain = simplified.0.into_iter().collect();
            }
            self.total_coords = new_total;
            if !can_still_shrink {
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

#[cfg(test)]
mod tests {
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
    fn compaction_during_add_still_allows_correct_stitching() {
        // Two long, wiggly lines meeting exactly at (50, y) -- simplification
        // during add() must not disturb that shared endpoint.
        let mut a = LineStitcher::with_max_coordinates(60);
        let first: Vec<(f64, f64)> = (0..=50).map(|i| (i as f64, (i as f64).sin())).collect();
        let second: Vec<(f64, f64)> = (50..=100).map(|i| (i as f64, (i as f64).sin())).collect();
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
