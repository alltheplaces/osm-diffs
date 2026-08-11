//! Search over the sorted `u64` key arrays shared by [BlobTable](crate::tables::BlobTable),
//! [CoordTable](crate::tables::CoordTable), and [U64Set](crate::tables::U64Set).
//!
//! All three tables store their keys as a sorted, mmap'd, little-endian
//! `&[u64]` slice and look them up the same way; this module holds that
//! shared logic once instead of three times.
//!
//! An interpolation-search variant was prototyped here, on the theory
//! that OSM object IDs are mostly contiguous (gaps come only from
//! deletions) and so a position guess should converge faster than plain
//! binary search. A benchmark against a real mmap'd file (not a
//! heap-allocated `Vec`, which can hide the page-touch cost this was
//! meant to avoid) didn't back that up: once the backing pages are
//! resident, interpolation search's extra per-probe arithmetic
//! consistently cost more than the fewer probes saved. It might still
//! win when a table's working set doesn't fit in RAM and lookups are
//! genuinely page-fault-bound, but that wasn't validated, so plain binary
//! search is what's here. See
//! <https://github.com/alltheplaces/osm-diffs/issues/620> for the
//! benchmark and discussion.

/// Returns the index of `key` in `keys`, or `None` if it is absent.
///
/// `keys` must be sorted in ascending order and stored as little-endian
/// `u64`s, as they are on the mmap'd pages backing `BlobTable`,
/// `CoordTable`, and `U64Set` — this function byte-swaps on read via
/// [u64::from_le], so it does not require the keys to already be in
/// native byte order.
pub fn search(keys: &[u64], key: u64) -> Option<usize> {
    let idx = keys.partition_point(|&k| u64::from_le(k) < key);
    (idx < keys.len() && u64::from_le(keys[idx]) == key).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(keys: &[u64]) -> Vec<u64> {
        keys.iter().map(|&k| k.to_le()).collect()
    }

    #[test]
    fn empty() {
        assert_eq!(search(&[], 0), None);
    }

    #[test]
    fn singleton() {
        let keys = le(&[42]);
        assert_eq!(search(&keys, 41), None);
        assert_eq!(search(&keys, 42), Some(0));
        assert_eq!(search(&keys, 43), None);
    }

    #[test]
    fn dense_contiguous_keys() {
        let dense: Vec<u64> = (100..100_000).collect();
        let keys = le(&dense);
        assert_eq!(search(&keys, 99), None);
        assert_eq!(search(&keys, 100), Some(0));
        assert_eq!(search(&keys, 5_000), Some(4_900));
        assert_eq!(search(&keys, 99_999), Some(99_899));
        assert_eq!(search(&keys, 100_000), None);
    }

    #[test]
    fn sparse_keys_with_gaps() {
        // Roughly OSM-like: mostly contiguous, with a few deleted IDs.
        let sparse: Vec<u64> = (0..10_000).filter(|n| n % 7 != 0 && n % 13 != 0).collect();
        let keys = le(&sparse);
        for (idx, &k) in sparse.iter().enumerate() {
            assert_eq!(search(&keys, k), Some(idx));
        }
        // Deleted IDs must be reported absent, not aliased to a neighbor.
        assert_eq!(search(&keys, 7), None);
        assert_eq!(search(&keys, 13), None);
        assert_eq!(search(&keys, 91), None); // 7 * 13
    }

    #[test]
    fn out_of_range_keys() {
        let keys = le(&[10, 20, 30]);
        assert_eq!(search(&keys, 0), None);
        assert_eq!(search(&keys, 9), None);
        assert_eq!(search(&keys, 31), None);
        assert_eq!(search(&keys, u64::MAX), None);
    }
}
