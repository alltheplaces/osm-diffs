//! Bookkeeping for a WKB geometry column written to some output file:
//! how many of each [`WkbGeometryType`] ended up in it, and the single
//! largest geometry seen, by byte size.
//!
//! See `wkb_writer`'s module comment for what WKB is.

use super::{WkbGeometryType, wkb_geometry_type};

/// Accumulated while writing one WKB geometry column: a count per
/// [`WkbGeometryType`] (e.g. becomes a GeoParquet file's `geo` metadata
/// `geometry_types`, or an INFO log line), and the single largest
/// geometry seen so far, by byte size, together with a caller-supplied
/// label identifying which row it came from (e.g. an OSM `type/id`
/// string, or an AllThePlaces spider name) -- so an unusually large
/// geometry can be tracked down without having to scan the whole file.
#[derive(Default)]
pub struct GeometryTally {
    counts: [u64; WkbGeometryType::ALL.len()],
    largest_bytes: usize,
    largest_label: String,
}

impl GeometryTally {
    /// Records one geometry. `label` is only called -- so only pays for
    /// whatever it allocates -- when `wkb` turns out to be the new
    /// largest seen so far.
    pub fn record(&mut self, wkb: &[u8], label: impl FnOnce() -> String) {
        let index = WkbGeometryType::ALL
            .iter()
            .position(|t| *t == wkb_geometry_type(wkb))
            .expect("WkbGeometryType::ALL covers every type wkb_geometry_type can return");
        self.counts[index] += 1;
        if wkb.len() > self.largest_bytes {
            self.largest_bytes = wkb.len();
            self.largest_label = label();
        }
    }

    /// Distinct types actually seen, as GeoParquet `geometry_types`
    /// strings -- e.g. `["Point"]`, or `["LineString", "Polygon"]`.
    pub fn geoparquet_types(&self) -> Vec<&'static str> {
        WkbGeometryType::ALL
            .iter()
            .zip(self.counts.iter())
            .filter(|&(_, &count)| count > 0)
            .map(|(t, _)| t.geoparquet_name())
            .collect()
    }

    /// Logs this tally's per-type counts and largest geometry seen, at
    /// INFO, as one structured record. `log::info!`'s field list has to
    /// be literal field names, so the indices below are hardcoded --
    /// they line up with [`WkbGeometryType::ALL`]'s declaration order.
    pub fn log(&self, message: &str) {
        log::info!(
            point = self.counts[0],
            line_string = self.counts[1],
            polygon = self.counts[2],
            multi_point = self.counts[3],
            multi_line_string = self.counts[4],
            multi_polygon = self.counts[5],
            geometry_collection = self.counts[6],
            largest_bytes = self.largest_bytes,
            largest = self.largest_label.as_str();
            "{}", message
        );
    }
}
