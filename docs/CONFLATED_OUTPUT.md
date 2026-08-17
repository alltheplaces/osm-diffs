# `conflated.parquet`

This is the pipeline’s public output: for every feature AllThePlaces
scrapes that plausibly maps to OpenStreetMap, one row pairing the ATP
side with whatever OSM feature `conflate()` matched it to — or `NULL`
on the OSM side, if it found no match. It’s written by
[`src/pipeline/conflate/writer.rs`](../src/pipeline/conflate/writer.rs);
see [`src/pipeline/conflate/mod.rs`](../src/pipeline/conflate/mod.rs)
for how a match is decided.

## Schema

```text
atp                                          STRUCT, nullable
  tags        MAP<UTF8, UTF8>                non-null map, non-null keys and values
  fetched                                    STRUCT, non-null
    timestamp TIMESTAMP(ms, UTC)             non-null
    spider    UTF8                           non-null
  geometry    BINARY (WKB)                   non-null

osm                                          STRUCT, nullable
  type        UTF8                           non-null ("node" | "way" | "relation")
  id          UINT64                         non-null
  tags        MAP<UTF8, UTF8>                non-null map, non-null keys and values
  modified                                   STRUCT, non-null
    timestamp TIMESTAMP(ms, UTC)             non-null
    changeset UINT64                         non-null
    version   UINT32                         non-null
  way_members LIST<UINT64>                   nullable — present only when type = "way"
  relation_members LIST<STRUCT>              nullable — present only when type = "relation"
    type      UTF8                           non-null ("node" | "way" | "relation")
    id        UINT64                         non-null
    role      UTF8                           non-null (empty string if OSM gave the member no role)
  geometry    BINARY (WKB)                   non-null
```

A few things worth calling out that aren’t obvious from the shape alone:

- **`atp` is nullable, but every row emitted today has it set.** The
  slot exists for a possible future “this OSM feature exists, but ATP
  doesn’t track it” row (see
  [alltheplaces/osm-diffs#682](https://github.com/alltheplaces/osm-diffs/issues/682)),
  not because `conflate()` produces one today.
- **`osm` is `NULL` exactly when nothing in OpenStreetMap matched.**
  That’s the normal, common case — most ATP features don’t have an OSM
  counterpart yet — not a data-quality problem.
- **`atp.fetched` and `osm.modified` mirror each other on purpose**: both
  answer “who produced this side of the row, and when” — `atp.fetched`
  for AllThePlaces’ own scrape (`spider:collection_time` from the
  spider that produced this feature; every feature from one spider run
  shares the same timestamp), `osm.modified` for OpenStreetMap’s own
  edit metadata (`timestamp`/`changeset`/`version` of the OSM element’s
  most recent edit — OSM’s edit history itself isn’t exposed, just its
  current state).
- **`osm.way_members`/`osm.relation_members` are `NULL`, not an empty
  list, when they don’t apply** — a `NULL` `way_members` means “this
  isn’t a way”, not “this way has no members”. Only one of the two is
  ever non-`NULL` for a given row, matching `osm.type`.
  `way_members` is a way’s node references in OSM’s declared order,
  as plain node IDs. `relation_members` is a relation’s members in OSM’s
  declared order; each member’s own `id` uses the same
  `osm_id * 10 + {1,2,3}` encoding internal to this pipeline being
  already decoded away by the time it reaches this column — what you
  get is a plain OSM ID plus a `type` telling you which of node/way/
  relation it refers to.
- **Timestamps are milliseconds, not seconds** — Parquet’s `TIMESTAMP`
  logical type has no “seconds” unit (only millis/micros/nanos), so
  seconds-precision source data (both `spider:collection_time` and
  OSM’s edit timestamps only ever carry whole seconds) is stored as
  milliseconds throughout, always exact multiples of 1000.
- **`geometry` is real OGC Simple Features WKB**, not a centroid or a
  synthetic point — `atp.geometry` is whatever point/line/polygon ATP’s
  own scrape carries, `osm.geometry` is the actual assembled shape of
  the matched node/way/relation (a `Polygon`/`MultiPolygon` for an area,
  a `LineString` for a way that isn’t one, a `Point` for a node). Not
  strict [GeoParquet](https://geoparquet.org/): there’s no top-level
  `geo` metadata key, since the geometry columns are nested inside
  `atp`/`osm` rather than top-level (tracked in
  [alltheplaces/osm-diffs#663](https://github.com/alltheplaces/osm-diffs/issues/663)).
  Typing rides on Arrow’s own WKB extension-type mechanism instead
  (`EPSG:4326`, spherical edges) — tools that understand Arrow
  extension types (DuckDB, GeoArrow-aware readers) pick it up
  automatically; a generic Parquet reader just sees `BINARY`.
- **No internal sort key is exposed.** Rows are written in ascending S2
  cell order (better compression, better spatial query performance),
  but that cell ID itself isn’t a column — it’s liable to change (e.g.
  if centroid computation for non-point geometry moves to a different
  library), so it was never made part of the public contract.

## Data provenance

Every file carries a [CycloneDX](https://cyclonedx.org/) JSON document
in its Parquet key-value metadata, under the key `org.cyclonedx.bom` —
answering “which version of this pipeline, using exactly what input,
and at what time, produced this file”. It records the AllThePlaces
dump and OpenStreetMap planet snapshot consumed (with their own
upstream identifiers/timestamps/hashes where available), the pipeline
version and its own supply-chain identity, and the run’s
start/end time. See [`src/provenance.rs`](../src/provenance.rs) for
exactly what’s in it, and
[`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md) for the
concepts behind it (that document is about the *release container
image’s* own SBOM/attestations — a related but distinct thing from
this per-file data provenance).

## Encoding

ZSTD level 22 throughout. Bloom filters are enabled on the columns most
likely to be point-looked-up (`atp.fetched.spider`, `atp.tags`,
`osm.id`, `osm.type`, `osm.tags`, `osm.modified.changeset`) — not on
geometry, `way_members`/`relation_members`, or `osm.modified.version`.

## A quick look

```sql
-- Real OSM shapes, not synthetic points:
SELECT osm.type, osm.id, atp.fetched.spider, ST_GeometryType(osm.geometry)
FROM read_parquet('conflated.parquet')
WHERE osm.id IS NOT NULL
LIMIT 10;
```

DuckDB (with the `spatial` extension loaded) reads `atp.geometry`/
`osm.geometry` as its native `GEOMETRY` type and `atp.fetched.timestamp`/
`osm.modified.timestamp` as `TIMESTAMP WITH TIME ZONE`, straight out of
the box — no manual WKB decoding or epoch-millisecond math needed.
