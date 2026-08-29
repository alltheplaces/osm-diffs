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

Two layers, split by whether `conflate()` found an OpenStreetMap match:

- **`matched`** — every `conflated.parquet` row with an `osm` side.
- **`unmatched`** — every row with no OpenStreetMap match.

Each is built as two PMTiles archives — a coarse **overview** (z0–12)
and a fine **detail** (z13–16) — merged with `tile-join`, so what a
row looks like depends on how far you're zoomed in (see below). A
feature's coordinates are rounded to 1e-7 degrees (about 1 cm at the
equator) before being written out — the same precision `edits.pmtiles`
uses — so raw source-data precision noise doesn't inflate the file.

## The two zoom ranges

### z0–12: the overview

**One deliberately minimal feature per row — no tags, no `fid`.** At z0
a single tile has to hold the entire planet, and every per-feature byte
counts. Tags push the tile so far past tippecanoe's size limit that
`--drop-densest-as-needed` throws away all but a few hundred features
worldwide; even a per-feature `fid` (unique, so it defeats the vector
tile's columnar value dedup) costs ~20× the low-zoom density (measured
against a full planet: full tags → ~370 features visible at z0; `+fid`
→ ~7k; overview as it stands → ~40k, a workable world map).

Geometry is `osm_geometry` (the matched OpenStreetMap shape — point,
line, or polygon) for `matched` rows, `atp_geometry` for `unmatched`.
Properties: `spider`, `matched`, and — `matched` rows only — `osm:type`
/ `osm:id`. To see a row's tags, click through to the detail layer
(below): `matched` rows join on `osm:type`+`osm:id`, `unmatched` rows
on nearest `part: "atp"` feature of the same `spider`.

### z13–16: the detail

**Full-tag features**, where a tile covers a few km and the tag
payload costs nothing.

- A **`matched`** row yields up to three features:
  - the AllThePlaces point (`part: "atp"`),
  - the OpenStreetMap shape (`part: "osm"`),
  - a connector line between their centroids (`part: "link"`) —
    *unless* that offset is under 5 meters (too close to draw a
    meaningful line; see `MIN_CONNECTOR_LENGTH_METERS` in
    [`src/pipeline/conflated_tiles.rs`](../../src/pipeline/conflated_tiles.rs)
    for the exact, documented threshold).
- An **`unmatched`** row yields one feature: its AllThePlaces point
  (`part: "atp"`), with the full `atp:*` tag set. `conflated.parquet`
  carries no stable id for the ATP side, so a viewer that clicked the
  (id-less) overview feature finds this one by taking the nearest
  `part: "atp"` feature of the same `spider` to the click.

The matched split into separate `atp`/`osm`/`link` features exists
because a single Mapbox Vector Tile feature can only be one geometry
type — "ATP point ∪ OSM shape ∪ connector line" can't be one feature —
and because showing the connector at low zoom turned out to be actively
harmful: tippecanoe's automatic zoom selection never terminates for a
connector line whose ends are nearly coincident (a *good* match), since
such a line never spatially separates from its own endpoint no matter
how far you zoom in. Bounding the detail pass to a fixed zoom range
side-steps that entirely — see
[`docs/notes/2026-08-27-connector-line-zoom-runaway.md`](../notes/2026-08-27-connector-line-zoom-runaway.md)
for the full investigation.

## Properties

- `fid` — the row's ordinal in `conflated.parquet` for this pipeline
  run. On **z13–16 detail features only** (not the overview — see
  above), where it groups a matched row's up-to-three features and
  cross-references `conflated.parquet`. Stable within a run only (both
  archives are always rebuilt together); not a cross-run identifier.
- `spider` — which AllThePlaces spider produced this feature (its
  `atp.fetched.spider` in `conflated.parquet`). On every feature.
- `matched` (`true` | `false`) — overview features only; the detail
  layer conveys the same thing through its layer name and `part`.
- `osm:type`, `osm:id` — the matched OpenStreetMap object. On the
  `matched` overview feature (where they double as the join key to the
  detail layer) and the z13–16 `osm`/`link` detail features.
- `osm:<tag>` for every tag in `osm.tags` — z13–16 `osm`/`link` detail
  features only.
- `atp:<tag>` for every tag in `atp.tags` — z13–16 `atp`/`link` detail
  features only.
- `part` (`"atp"` | `"osm"` | `"link"`) — z13–16 detail features only,
  identifying which of the (up to) three a given feature is.

On the `link` feature, tag properties are prefixed (`atp:name` vs.
`osm:name`) rather than merged, since the two sides can and do disagree
— that disagreement is often exactly what's worth reviewing. The
`atp`/`osm` detail features carry only their own side's tags (no
`osm:*` on the `atp` feature or vice versa), so a tile inspector shows
unambiguously which end of a pair you're looking at.
