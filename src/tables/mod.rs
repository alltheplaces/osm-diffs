//! Read-only data structures, stored on disk, mapped into virtual memory.
//!
//! The tables in this module support very large data volumes: As long
//! as there is enough disk space, a table can be larger than the
//! physical RAM installed on the machine. This makes it possible to
//! process the entire OpenStreetMap planet on cheap worker machine.

mod coords_map;
mod graph;
mod string_counts;
mod u64_set;

pub use coords_map::CoordsMap;
pub use graph::{Edge, GraphTable};
pub use string_counts::StringCounts;
pub use u64_set::U64Set;
