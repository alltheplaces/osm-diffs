//! Read-only data structures, stored on disk, mapped into virtual memory.
//!
//! The tables in this module support very large data volumes: As long
//! as there is enough disk space, a table can be larger than the
//! physical RAM installed on the machine. This makes it possible to
//! process the entire OpenStreetMap planet on cheap worker machine.

#[allow(unused)]
mod blob_table;
mod coords_map;
mod graph;
#[allow(unused)]
mod records;
mod string_counts;
mod string_pool;
mod u64_set;

mod features {
    include!(concat!(env!("OUT_DIR"), "/tables.features.rs"));
}

#[allow(unused)]
pub use blob_table::BlobTable;
pub use coords_map::CoordsMap;
pub use features::{Feature, FeatureToIndex};
pub use graph::{Edge, GraphTable};
#[allow(unused)]
pub use records::{RecordReader, RecordWriter};
pub use string_counts::StringCounts;
pub use string_pool::StringPool;
pub use u64_set::U64Set;
