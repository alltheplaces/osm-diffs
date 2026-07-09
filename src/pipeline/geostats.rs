use anyhow::{Ok, Result};
use parquet::{
    basic::LogicalType,
    geospatial::accumulator::{
        GeoStatsAccumulator, GeoStatsAccumulatorFactory, ParquetGeoStatsAccumulator,
        VoidGeoStatsAccumulator, init_geo_stats_accumulator_factory,
    },
    schema::types::ColumnDescPtr,
};
use std::sync::Arc;

// As of Apache Parquet version 59.0, geospatial statistics ony get
// computed for GEOMETRY fields, whose coordinates are in a projected,
// rectangular reference system.  For GEOGRAPHY fields, whose
// coordinates are in a (unprojected) spherical reference system,
// Parquet computes no GeoStats. Their bounding box computation simply
// takes the min/max of every x/y; this gives wrong results for shapes
// that cross the poles or the anti-meridian. As we’re already using
// the S2 library for spherical geometry, we could relatively easily
// implement a GeoStats callback that does computation this
// properly. However, this particular part of the Rust port of S2 is
// not implemented yet. Therefore, we cheat by invoking the
// Parquet-supplied GeoStats accumulator also for GEOGRAPHY fields
// with spherical coordinates. In practice, neither AllThePlaces nor
// OpenStreetMap contain any gas stations or hotels whose shapes cross
// the poles, so this is not a major problem for us.
//
// Apache Parquet was originally written in Java, and it clearly shows
// also in its Rust interface. Well, the API of Apache Parquet is
// what it is; greetings from Java land. For a proposal to remove
// this global state, which would require an API change to Apache
// Parquet, see https://github.com/apache/arrow-rs/issues/10312.
struct CustomGeoStatsAccumulatorFactory {}

impl GeoStatsAccumulatorFactory for CustomGeoStatsAccumulatorFactory {
    fn new_accumulator(&self, desc: &ColumnDescPtr) -> Box<dyn GeoStatsAccumulator> {
        match desc.logical_type_ref() {
            Some(LogicalType::Geography { .. }) => Box::new(ParquetGeoStatsAccumulator::default()),
            Some(LogicalType::Geometry { .. }) => Box::new(ParquetGeoStatsAccumulator::default()),
            _ => Box::new(VoidGeoStatsAccumulator::default()),
        }
    }
}

pub fn init() -> Result<()> {
    let factory = Arc::new(CustomGeoStatsAccumulatorFactory {});
    init_geo_stats_accumulator_factory(factory)?;
    Ok(())
}
