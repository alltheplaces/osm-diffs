# `conflated.parquet`

For every feature [AllThePlaces](https://alltheplaces.xyz/) scrapes
that plausibly maps to something in
[OpenStreetMap](https://www.openstreetmap.org/), this file has one row
pairing the AllThePlaces feature with whichever OpenStreetMap feature
it was matched to — or with no OpenStreetMap match at all, if none was
found.

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
    role      UTF8                           nullable — null if OSM gives the member no role
  geometry    BINARY (WKB)                   non-null
```

A few things worth calling out that aren’t obvious from the above:

- **`osm` is `null` whenever nothing in OpenStreetMap matched.** That
  can happen for perfectly ordinary reasons — it doesn’t mean anything
  is wrong with the conflation pipeline; rather, it’s a signal that
  the feature may be missing from OpenStreetMap.
- **`atp.fetched` and `osm.modified` mirror each other on purpose**:
  both answer “who produced this side of the row, and when” —
  `atp.fetched` for AllThePlaces’ own scrape, `osm.modified` for
  OpenStreetMap’s own edit metadata (the timestamp, changeset, and
  version number of that feature’s most recent edit). We don’t
  preserve OpenStreetMap’s full edit history here, only its current
  state as of the database snapshot this file was built from.
- **`osm.way_members`/`osm.relation_members` are `null`, not an empty
  list, when they don’t apply** — a `null` `way_members` means “this
  isn’t a way”, not “this way has no members”. Only one of the two is
  ever non-`null` for a given row, matching `osm.type`. `way_members`
  is a way’s node references, in OpenStreetMap’s declared order.
  `relation_members` is a relation’s members, also in declared order,
  each with its own type (node/way/relation), ID, and role (e.g.
  “outer”, “inner”, or `null` if OpenStreetMap doesn’t record one).

## Geometry

`atp.geometry`/`osm.geometry` are standard [OGC Simple
Features](https://postgis.net/workshops/postgis-intro/geometries.html)
geometries — the same model QGIS, PostGIS, and most other GIS tools
already use. `atp.geometry` is whatever AllThePlaces’ own scrape
provides for that feature — nearly always a point, though a handful of
sources provide lines or polygons instead. `osm.geometry` is the
matched OpenStreetMap feature’s actual shape: a polygon for an area, a
line for a way that isn’t an area, a point for a node.

Rows are sorted along a Hilbert curve, for better compression and
faster spatial queries. At the moment, we compute this from the S2
cell ID of each geometry’s centroid, but we don’t want to commit to
that particular way of doing the sort, so the sort key itself isn’t
exposed as a column.

## Data provenance

Every file carries a [CycloneDX](https://cyclonedx.org/) document
embedded in its own metadata — CycloneDX being an established,
widely-used format for exactly this kind of “where did this data come
from” record. It answers questions like which AllThePlaces run and
which OpenStreetMap snapshot went into this file, and which version of
our pipeline produced it. It also records licensing information for
both inputs and the output.

```sh
duckdb -c "
SELECT
    json_extract_string(decode(value), '\$.metadata.tools.components[0].version') AS pipeline_version,
    json_extract_string(decode(value), '\$.components[0].version') AS alltheplaces_run,
    json_extract_string(decode(value), '\$.components[1].version') AS openstreetmap_snapshot
FROM parquet_kv_metadata('conflated.parquet')
WHERE key::VARCHAR = 'org.cyclonedx.bom';
"
```

```text
┌──────────────────┬──────────────────────┬────────────────────────┐
│ pipeline_version │   alltheplaces_run   │ openstreetmap_snapshot │
│     varchar      │       varchar        │         varchar        │
├──────────────────┼──────────────────────┼────────────────────────┤
│ 0.6.10           │ 2026-01-01T00:00:00Z │ 2026-01-27T08:11:02Z   │
└──────────────────┴──────────────────────┴────────────────────────┘
```

## A quick look

```sh
duckdb -c "
INSTALL spatial; LOAD spatial;
SELECT osm.type, osm.id, atp.fetched.spider, ST_GeometryType(osm.geometry) AS osm_shape
FROM read_parquet('conflated.parquet')
WHERE osm.id IS NOT NULL
LIMIT 3;
"
```

```text
┌─────────┬───────────┬────────────┬───────────────┐
│  type   │    id     │   spider   │   osm_shape   │
│ varchar │  uint64   │  varchar   │ geometry_type │
├─────────┼───────────┼────────────┼───────────────┤
│ way     │ 608979139 │ tchibo     │ POLYGON       │
│ way     │ 737021556 │ mediamarkt │ POLYGON       │
│ way     │ 737021557 │ denner_ch  │ POLYGON       │
└─────────┴───────────┴────────────┴───────────────┘
```
