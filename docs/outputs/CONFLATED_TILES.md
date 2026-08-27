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

⚠️ **This file is for visualization only** — a debugging aid for
reviewing matching/conflation, not a data product. Its structure may
change at any time, or we may stop producing it altogether, without
notice. If you need something stable to build on, use
[`conflated.parquet`](CONFLATED_PARQUET.md) instead.

## Layers

Two layers, split by whether `conflate()` found an OpenStreetMap match
— but `matched`'s own shape changes with zoom, see below.

- **`matched`** — every `conflated.parquet` row with an `osm` side.
- **`unmatched`** — every row with no OpenStreetMap match. One feature
  per row at every zoom, geometry `atp_geometry` (wherever AllThePlaces
  placed it).

Every feature's coordinates are rounded to 1e-7 degrees (about 1 cm at
the equator) before being written out — the same precision
`edits.pmtiles`' features already use — so raw source-data precision
noise doesn't inflate the file for no visual benefit.

## `matched`'s two zoom ranges

Built as two separate PMTiles archives, merged with `tile-join`, so a
matched row looks different depending how far you're zoomed in:

- **z0–12 (overview)**: one feature per row, geometry `osm_geometry` —
  the actual matched OpenStreetMap shape (point, line, or polygon),
  since that's what a reviewer is meant to look at.
- **z13–16 (detail)**: up to three features per row —
  - the AllThePlaces point (`part: "atp"`),
  - the OpenStreetMap shape (`part: "osm"`),
  - a connector line between their centroids (`part: "link"`) —
    *unless* that offset is under 5 meters (too close to draw a
    meaningful line; see `MIN_CONNECTOR_LENGTH_METERS` in
    [`src/pipeline/conflated_tiles.rs`](../../src/pipeline/conflated_tiles.rs)
    for the exact, documented threshold).

  This split exists because a single Mapbox Vector Tile feature can
  only be one geometry type — "ATP point ∪ OSM shape ∪ connector line"
  can't be one feature — and because showing the connector at low zoom
  turned out to be actively harmful: tippecanoe's automatic zoom
  selection never terminates for a connector line whose ends are
  nearly coincident (a *good* match), since such a line never
  spatially separates from its own endpoint no matter how far you zoom
  in. Bounding the detail pass to a fixed zoom range side-steps that
  entirely — see
  [`docs/notes/2026-08-27-connector-line-zoom-runaway.md`](../notes/2026-08-27-connector-line-zoom-runaway.md)
  for the full investigation.

## Properties

- `spider` — which AllThePlaces spider produced this feature (its
  `atp.fetched.spider` in `conflated.parquet`).
- `atp:<tag>` — every tag from the row's `atp.tags` map, present on
  `unmatched` features, the z0–12 `matched` overview feature, and the
  z13–16 `atp`/`link` detail features.
- `osm:type`, `osm:id`, and `osm:<tag>` for every tag in `osm.tags` —
  present on the z0–12 `matched` overview feature and the z13–16
  `osm`/`link` detail features.
- `part` (`"atp"` | `"osm"` | `"link"`) — z13–16 detail features only,
  identifying which of the three a given feature is.

At z0–12 and on the `link` feature, tag properties are prefixed
(`atp:name` vs. `osm:name`) rather than merged, since the two sides can
and do disagree — that disagreement is often exactly what's worth
reviewing. The z13–16 `atp`/`osm` detail features carry only their own
side's tags (no `osm:*` on the `atp` feature or vice versa), so a tile
inspector shows unambiguously which end of a pair you're looking at.
