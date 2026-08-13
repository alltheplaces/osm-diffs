//! Small, generic helpers shared by more than one place in the crate.
//! A natural home for other such helpers as they accumulate -- e.g.
//! `crate::memstats` is arguably one of these too, just not moved here
//! (yet).

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_hex() {
        assert_eq!(to_hex(&[]), "");
        assert_eq!(to_hex(&[0x00, 0xff, 0x0a]), "00ff0a");
    }
}
