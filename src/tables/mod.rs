//! Read-only data structures, stored on disk, mapped into virtual memory.
//!
//! The tables in this module support very large data volumes: As long
//! as there is enough disk space, a table can be larger than the
//! physical RAM installed on the machine. This makes it possible to
//! process the entire OpenStreetMap planet on cheap worker machine.
//!
//! `crate::places` plays the same role for AllThePlaces data -- durable,
//! cross-pipeline-stage storage for one side of conflation -- but isn't
//! one of these tables: ATP's data volume doesn't need the mmap +
//! page-cache design this module is built around, so it's plain
//! Arrow/Parquet instead (compressed, opened and decoded batch by
//! batch, not mapped into memory).

#[allow(unused)]
mod blob_table;
mod coord_table;
mod feature_index;
#[allow(unused)]
mod geometry_store;
mod geometry_table;
mod graph;
#[allow(unused)]
mod records;
mod sorted_u64_index;
mod string_counts;
mod string_pool;
mod u64_set;

mod features {
    include!(concat!(env!("OUT_DIR"), "/tables.features.rs"));
}

#[allow(unused)]
pub use blob_table::BlobTable;
pub use coord_table::CoordTable;
#[allow(unused)]
pub use feature_index::LocalFeatureRef;
pub use feature_index::{OsmFeatureIndex, OsmFeatures};
pub use features::{Feature, FeatureToIndex, RelationMember};
#[allow(unused)]
pub use geometry_store::GeometryStore;
pub use geometry_table::GeometryTable;
pub use graph::{Edge, GraphTable};
#[allow(unused)]
pub use records::{RecordReader, RecordWriter};
pub use string_counts::StringCounts;
pub use string_pool::StringPool;
pub use u64_set::U64Set;
