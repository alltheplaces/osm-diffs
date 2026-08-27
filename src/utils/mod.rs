//! Small, generic helpers shared by more than one otherwise-independent
//! part of the crate -- e.g. `UtcTimestamp` below is used by both
//! `places::Place::fetched` and `pipeline::conflate::writer`. A natural
//! home for other such helpers as they accumulate, as long as they're
//! genuinely small and genuinely cross-cutting.

pub(crate) mod parquet;

/// Renders `bytes` as a lowercase hex string, e.g. for a digest.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

/// (De)serializes [`time::UtcDateTime`] as RFC 3339 strings, e.g.
/// "2026-03-04T15:16:17Z" -- for use as
/// `#[serde(with = "crate::utils::rfc3339")]`. `time`'s own `serde`
/// feature would do this for us, but isn't enabled in this crate; this
/// is small enough not to be worth the extra dependency surface.
pub(crate) mod rfc3339 {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};
    use time::{UtcDateTime, format_description::well_known::Rfc3339};

    pub fn serialize<S: Serializer>(t: &UtcDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&t.format(&Rfc3339).map_err(S::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<UtcDateTime, D::Error> {
        let text = String::deserialize(d)?;
        UtcDateTime::parse(&text, &Rfc3339).map_err(D::Error::custom)
    }
}

/// A [`time::UtcDateTime`] usable in a struct that also derives
/// [`deepsize::DeepSizeOf`] (needed by anything externally sorted via a
/// `MemoryLimitedBufferBuilder`) -- the upstream `time` crate doesn't
/// implement `DeepSizeOf` itself, and can't be made to here either,
/// since both the trait and `UtcDateTime` are foreign to this crate, so
/// Rust's orphan rule blocks `impl DeepSizeOf for UtcDateTime` directly.
/// Wrapping it in a local type sidesteps that. `UtcDateTime` has no heap
/// allocation of its own, so `deep_size_of_children` correctly returning
/// 0 below isn't an approximation -- `DeepSizeOf::deep_size_of()`'s
/// default implementation already adds `size_of::<Self>()` on top,
/// which is all a fixed-size, non-allocating value like this one ever
/// needs.
///
/// Used e.g. for [`crate::places::Place::fetched`] and
/// `conflate::writer::ParquetRow`'s modification-timestamp fields --
/// real `UtcDateTime` ergonomics in Rust code, rather than a bare,
/// easy-to-misinterpret `i64`/`u64`.
///
/// (De)serializes as a plain `i64` Unix-milliseconds integer, *not* as
/// an RFC 3339 string via `crate::utils::rfc3339` -- unlike that
/// helper's intended use (one-off JSON output in `provenance.rs`), this
/// type's `Serialize`/`Deserialize` impl is on the hot path: `Place`
/// (which embeds this) gets serialized to MessagePack on every spilled
/// chunk of every external sort of the ATP dataset. A formatted string
/// there would cost real formatting/parsing time and extra spilled
/// bytes, for no benefit nothing downstream ever reads as text.
/// Milliseconds, not whole seconds: AllThePlaces' `spider:collection_time`
/// carries sub-second precision (microseconds, as of August 2026), and
/// this is the finest granularity `conflated.parquet`'s own
/// `Timestamp(Millisecond, UTC)` output columns can represent anyway
/// (see `pipeline::conflate::writer`) -- truncating to whole seconds
/// here, before that value is ever written out, would silently throw
/// away real precision the final output could otherwise keep.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UtcTimestamp(pub time::UtcDateTime);

impl UtcTimestamp {
    /// Milliseconds since the Unix epoch -- the conversion every writer
    /// of a `Timestamp(Millisecond, UTC)` column needs. Not just
    /// `unix_timestamp() * 1000`: that goes through `unix_timestamp()`,
    /// which is defined to discard everything below whole seconds,
    /// silently truncating any sub-second precision `self.0` actually
    /// carries. Computed from `unix_timestamp_nanos()` instead so this
    /// is a true millisecond truncation (still lossy relative to
    /// nanoseconds, but not more than the `Millisecond` column type
    /// itself already implies).
    pub fn unix_timestamp_millis(&self) -> i64 {
        i64::try_from(self.0.unix_timestamp_nanos() / 1_000_000)
            .expect("timestamp out of i64 millisecond range")
    }

    /// Inverse of [`unix_timestamp_millis`](Self::unix_timestamp_millis).
    /// Also what `osm::Feature.timestamp` needs (see
    /// `pipeline::conflate::writer`): `osm_pbf_iter`'s `Info.timestamp`
    /// (for a `DenseNodes`-encoded node, which is nearly every node in a
    /// real-world extract) is already `date_granularity`-scaled to
    /// milliseconds by that crate itself, per the OSM PBF format spec's
    /// own description of `date_granularity` -- there is no
    /// whole-seconds form to call `from_unix_timestamp` on directly (see
    /// #749: that mismatch is exactly what crashed the first real
    /// containerized smoke test in issue #722).
    pub fn from_unix_timestamp_millis(millis: i64) -> Result<Self, time::error::ComponentRange> {
        time::UtcDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).map(Self)
    }
}

impl deepsize::DeepSizeOf for UtcTimestamp {
    fn deep_size_of_children(&self, _context: &mut deepsize::Context) -> usize {
        0
    }
}

impl serde::Serialize for UtcTimestamp {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(self.unix_timestamp_millis())
    }
}

impl<'de> serde::Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let millis = <i64 as serde::Deserialize>::deserialize(d)?;
        UtcTimestamp::from_unix_timestamp_millis(millis).map_err(serde::de::Error::custom)
    }
}

impl From<time::UtcDateTime> for UtcTimestamp {
    fn from(t: time::UtcDateTime) -> Self {
        UtcTimestamp(t)
    }
}

impl std::ops::Deref for UtcTimestamp {
    type Target = time::UtcDateTime;
    fn deref(&self) -> &time::UtcDateTime {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepsize::DeepSizeOf;

    #[test]
    fn test_to_hex() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00, 0xff, 0x0a]), "00ff0a");
    }

    #[test]
    fn test_utc_timestamp_round_trips_through_messagepack() {
        let t = UtcTimestamp(time::UtcDateTime::from_unix_timestamp(1_770_000_000).unwrap());
        let bytes = rmp_serde::to_vec(&t).expect("serialize");
        // A plain i64 in MessagePack fixint/int64 encoding, not an
        // RFC 3339 string -- e.g. "2026-02-01T20:00:00Z" alone would be
        // 20+ bytes.
        assert!(
            bytes.len() <= 9,
            "expected a compact int encoding, got {bytes:?}"
        );
        let round_tripped: UtcTimestamp = rmp_serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(round_tripped, t);
    }

    /// Regression test: an earlier version of this type serialized via
    /// `unix_timestamp()`, which discards everything below whole
    /// seconds -- silently truncating AllThePlaces' `spider:collection_time`
    /// (which does carry sub-second precision, e.g. microseconds as of
    /// August 2026) down to the second on every `Place`'s round trip
    /// through the ATP external sort, well before it ever reached
    /// `conflated.parquet`'s millisecond-precision output columns.
    #[test]
    fn test_utc_timestamp_preserves_millisecond_precision_through_messagepack() {
        let t = UtcTimestamp(
            time::UtcDateTime::from_unix_timestamp_nanos(1_780_209_952_804_399_000).unwrap(),
        );
        assert_eq!(t.unix_timestamp_millis(), 1_780_209_952_804);

        let bytes = rmp_serde::to_vec(&t).expect("serialize");
        let round_tripped: UtcTimestamp = rmp_serde::from_slice(&bytes).expect("deserialize");
        assert_eq!(round_tripped.unix_timestamp_millis(), 1_780_209_952_804);
    }

    #[test]
    fn test_utc_timestamp_deep_size_of_is_shallow_only() {
        let t = UtcTimestamp(time::UtcDateTime::now());
        assert_eq!(t.deep_size_of(), size_of::<UtcTimestamp>());
    }
}
