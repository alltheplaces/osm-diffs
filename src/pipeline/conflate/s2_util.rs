use s2::cellid::CellID;
use s2::cellunion::CellUnion;

pub struct MergedCellRanges {
    iter: std::vec::IntoIter<CellID>,
    pending: Option<(CellID, CellID)>,
}

impl MergedCellRanges {
    pub fn new(covering: CellUnion) -> Self {
        Self {
            iter: covering.0.into_iter(),
            pending: None,
        }
    }
}

impl Iterator for MergedCellRanges {
    type Item = (CellID, CellID);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pending.is_none() {
            let first = self.iter.next()?;
            self.pending = Some((first.range_min(), first.range_max()));
        }

        loop {
            match self.iter.next() {
                None => return self.pending.take(),
                Some(cell_id) => {
                    let (_lo, hi) = self.pending.as_mut().unwrap();
                    let next_lo = cell_id.range_min();
                    let next_hi = cell_id.range_max();

                    if next_lo.0 <= hi.0.saturating_add(1) {
                        if next_hi.0 > hi.0 {
                            *hi = next_hi;
                        }
                    } else {
                        let emitted = self.pending.replace((next_lo, next_hi));
                        return emitted;
                    }
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (lo, hi) = self.iter.size_hint();
        let has_pending = usize::from(self.pending.is_some());
        (
            has_pending.max(lo.min(1)),
            hi.map(|h| h.saturating_add(has_pending)),
        )
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use s2::latlng::LatLng;

    /// Convenience: leaf cell at a lat/lng, then lifted to `level`.
    fn cell_at(lat: f64, lng: f64, level: u64) -> CellID {
        CellID::from(LatLng::from_degrees(lat, lng)).parent(level)
    }

    /// Build a sorted CellUnion without normalising (preserves overlaps for
    /// the tests that need them).
    ///
    /// `raw_union` sorts without normalising, so we can construct overlapping
    /// unions (parent + child) that exercise the merge path directly.
    /// A normalised `CellUnion` would erase the child, making that test vacuous.
    fn raw_union(mut cells: Vec<CellID>) -> CellUnion {
        cells.sort();
        CellUnion(cells)
    }

    // ── basic coverage ────────────────────────────────────────────────────────

    #[test]
    fn empty_covering_yields_nothing() {
        let result: Vec<_> = MergedCellRanges::new(CellUnion(vec![])).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn single_cell_yields_its_own_range() {
        let c = cell_at(48.85, 2.35, 12); // Paris, level 12
        let result: Vec<_> = MergedCellRanges::new(raw_union(vec![c])).collect();
        assert_eq!(result, vec![(c.range_min(), c.range_max())]);
    }

    // ── merging behaviour ─────────────────────────────────────────────────────

    #[test]
    fn non_adjacent_cells_produce_separate_ranges() {
        // Zürich and Wellington are on opposite sides of the globe, so their
        // level-6 cells are guaranteed to be far apart in ID space.
        let a = cell_at(47.37, 8.54, 6);
        let b = cell_at(-41.29, 174.78, 6);
        let (first, second) = if a < b { (a, b) } else { (b, a) };

        let result: Vec<_> = MergedCellRanges::new(raw_union(vec![first, second])).collect();

        assert_eq!(
            result,
            vec![
                (first.range_min(), first.range_max()),
                (second.range_min(), second.range_max()),
            ]
        );
    }

    /// The only test that actually triggers the next_lo.0 <= hi.0 + 1 branch.
    /// Valid, non-overlapping S2 cells always have a gap of ≥ 2 between their leaf ranges
    /// (both range_min/range_max are always odd), so the coalescing condition only fires
    /// for overlapping inputs.
    #[test]
    fn overlapping_cells_are_coalesced_into_one_range() {
        // A level-10 cell and its level-9 parent overlap; pass them as a
        // non-normalised union to exercise the merge path directly.
        let child = cell_at(0.0, 0.0, 10);
        let parent = child.parent(9); // parent's range strictly contains child's

        let result: Vec<_> = MergedCellRanges::new(raw_union(vec![child, parent])).collect();

        assert_eq!(
            result.len(),
            1,
            "overlapping cells must collapse to one range"
        );
        let (lo, hi) = result[0];
        // The merged range must cover the full parent extent.
        assert!(lo <= parent.range_min());
        assert!(hi >= parent.range_max());
    }

    #[test]
    fn multiple_cells_some_overlapping_some_not() {
        // Three disjoint coarse cells, with the middle one duplicated at a
        // finer level inside it.  Expected: 3 non-overlapping output ranges.
        let a = cell_at(10.0, 10.0, 6);
        let b = cell_at(20.0, 20.0, 6);
        let b_child = cell_at(20.0, 20.0, 10); // contained in b
        let c = cell_at(30.0, 30.0, 6);

        let result: Vec<_> = MergedCellRanges::new(raw_union(vec![a, b, b_child, c])).collect();

        assert_eq!(result.len(), 3);
        // First and last ranges correspond to a and c exactly.
        let (first, last) = (result.first().unwrap(), result.last().unwrap());
        assert_eq!(*first, (a.range_min(), a.range_max()));
        assert_eq!(*last, (c.range_min(), c.range_max()));
        // Middle range covers at least b's full extent.
        assert!(result[1].0 <= b.range_min());
        assert!(result[1].1 >= b.range_max());
    }

    // ── size_hint ─────────────────────────────────────────────────────────────

    #[test]
    fn size_hint_empty() {
        let iter = MergedCellRanges::new(CellUnion(vec![]));
        assert_eq!(iter.size_hint(), (0, Some(0)));
    }

    #[test]
    fn size_hint_single_cell_not_yet_consumed() {
        let iter = MergedCellRanges::new(raw_union(vec![cell_at(0.0, 0.0, 10)]));
        // One cell → exactly one range guaranteed.
        assert_eq!(iter.size_hint(), (1, Some(1)));
    }

    #[test]
    fn size_hint_upper_bound_equals_cell_count_before_iteration() {
        // Worst case: no merging → one range per cell.
        let cells = vec![
            cell_at(0.0, 0.0, 5),
            cell_at(20.0, 20.0, 5),
            cell_at(40.0, 40.0, 5),
        ];
        let n = cells.len();
        let iter = MergedCellRanges::new(raw_union(cells));
        let (lo, hi) = iter.size_hint();
        assert_eq!(lo, 1, "at least one output range");
        assert_eq!(hi, Some(n), "at most one range per input cell");
    }

    // `size_hint_stays_valid_throughout_iteration` pins the exact sequence
    // `(3 → 2 → 1 → 0)` for three non-merging cells, which follows directly
    // from the has_pending + inner.size_hint() accounting.
    #[test]
    fn size_hint_stays_valid_throughout_iteration() {
        // Three non-overlapping, non-adjacent cells → three output ranges.
        let cells = vec![
            cell_at(0.0, 0.0, 5),
            cell_at(20.0, 20.0, 5),
            cell_at(40.0, 40.0, 5),
        ];
        let mut iter = MergedCellRanges::new(raw_union(cells));

        // Snapshot size_hint, advance, then verify the remaining items never
        // exceeded the previous upper bound.
        let (_, hi0) = iter.size_hint();
        assert_eq!(hi0, Some(3));
        iter.next();

        let (_, hi1) = iter.size_hint();
        assert_eq!(hi1, Some(2));
        iter.next();

        let (_, hi2) = iter.size_hint();
        assert_eq!(hi2, Some(1));
        iter.next(); // exhaust

        assert_eq!(iter.size_hint(), (0, Some(0)));
    }

    /// Tests a subtle edge case: after the inner iterator is drained
    /// but one range is still pending, `has_pending = 1` must keep
    // the upper bound at 1 rather than letting it collapse to `Some(0)`.
    #[test]
    fn size_hint_upper_bound_accounts_for_pending_on_last_cell() {
        // After consuming all but the final (pending) range the inner iterator
        // is empty; has_pending must keep the upper bound at 1, not 0.
        let cells = vec![cell_at(0.0, 0.0, 5), cell_at(20.0, 20.0, 5)];
        let mut iter = MergedCellRanges::new(raw_union(cells));
        iter.next(); // emits first range; second is now pending, inner iter empty
        assert_eq!(iter.size_hint(), (1, Some(1)));
    }
}
