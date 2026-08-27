# `conflated.pmtiles`

*Looking to download this file rather than read about its format? See
[downloading the current output](README.md#downloading-the-current-output).*

Visualizes [`conflated.parquet`](CONFLATED_PARQUET.md) itself — every
[AllThePlaces](https://alltheplaces.xyz/) feature, matched to
[OpenStreetMap](https://www.openstreetmap.org/) or not — so the
*matching* step can be reviewed on its own, independent of whatever
[`edits.pmtiles`](../TECHNICAL_DESIGN.md) separately proposes changing.
It's a [PMTiles](https://protomaps.com/docs/pmtiles) archive, built with
[Tippecanoe](https://github.com/felt/tippecanoe) the same way
`edits.pmtiles` is — see [`pmtiles.io`](https://pmtiles.io) for how to
open one in a browser, no server required.

## Layers

Two layers, split by whether `conflate()` found an OpenStreetMap match:

- **`matched`** — one feature per `conflated.parquet` row with an `osm`
  side. Geometry is `osm_geometry`: the actual matched OpenStreetMap
  shape (point, line, or polygon), since that's what a reviewer is
  meant to look at.
- **`unmatched`** — one feature per row with no OpenStreetMap match.
  Geometry is `atp_geometry`: wherever AllThePlaces placed it.

Every feature's coordinates are rounded to 1e-7 degrees (about 1 cm at
the equator) before being written out — the same precision
`edits.pmtiles`' features already use — so raw source-data precision
noise doesn't inflate the file for no visual benefit.

## Properties

- `spider` — which AllThePlaces spider produced this feature (its
  `atp.fetched.spider` in `conflated.parquet`).
- `atp:<tag>` — every tag from the row's `atp.tags` map, one property
  per tag.
- `osm:type`, `osm:id`, and `osm:<tag>` for every tag in `osm.tags` —
  present only on `matched` features.

Tag properties are prefixed (`atp:name` vs. `osm:name`) rather than
merged, since the two sides can and do disagree — that disagreement is
often exactly what's worth reviewing.

There's no match-confidence score yet — see
[`CONFLATED_PARQUET.md`](CONFLATED_PARQUET.md) for why `conflate()`
doesn't persist one today.

## What this isn't (yet)

A matched feature shows only the OpenStreetMap side, not a visual link
back to where AllThePlaces placed it — see
[#775](https://github.com/alltheplaces/osm-diffs/issues/775) for that
idea (a connector line between the two), left for later since a vector
tile feature can only be one geometry type, so it needs its own design
and its own memory/size measurement, not a quick addition here.
