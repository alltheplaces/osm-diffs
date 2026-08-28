# Connector-line visualization: a zoom-runaway gotcha, and the fix

Investigation note for [#775](https://github.com/alltheplaces/osm-diffs/issues/775)
(visualize the ATP↔OSM offset in `conflated.pmtiles` with a connector
line). A prototype, its measurements, and six real sample renders are
published as an artifact:
<https://claude.ai/code/artifact/0bd35079-99d3-4608-b2e8-a968cb53160c>.
This note captures the same findings as plain text, for anyone without
access to that link.

## The idea

For a matched row, draw the ATP point, the real OSM shape, and a line
connecting their centroids — instead of just picking one geometry, the
way `conflated.pmtiles` v1 (#709) does. Since a Mapbox Vector Tile
feature can only be one geometry type, this means three separate
features per matched row (`part: "atp" | "osm" | "link"`), not one.

## First measurement: expensive, but why wasn't obvious

An unrestricted build (`tippecanoe -zg --extend-zooms-if-still-dropping`,
the same flags `render_conflated_tiles` already uses today) against a
real full-planet `conflated.parquet` (1,744,015 rows, 730,512 matched):

| | v1 (shipped) | Connector line, unrestricted |
|---|---:|---:|
| `render_conflated_tiles` wall time | 1m48s | 19m55s |
| tippecanoe peak RSS | 1.87 GB | 2.86 GB |
| `conflated.pmtiles` size | 768 MB | 5.4 GB |

RAM only grew 1.5× — never the risk. Wall time and output size grew
11× and 7×, wildly disproportionate to the 3× feature count.

## Root cause: found by reading the PMTiles header, not by guessing

The first hypothesis (low-zoom feature density triggering
`--drop-densest-as-needed`'s retry loop) turned out to be wrong.
Checking the actual PMTiles headers instead of speculating:

```python
with open("conflated.pmtiles", "rb") as f:
    header = f.read(127)
print(header[100], header[101])  # min_zoom, max_zoom
```

| Build | min_zoom | max_zoom |
|---|---:|---:|
| v1 (shipped) | 0 | 12 |
| Connector line, unrestricted | 0 | **19** |

`-zg --extend-zooms-if-still-dropping` keeps adding zoom levels until
every crowded feature has separated into its own tile — which works
when zooming in genuinely spreads features apart. It doesn't terminate
when a connector line's two ends are nearly coincident (a *good*
match!), because that line never spatially separates from its own
endpoint no matter how far you zoom. Tippecanoe kept climbing, chasing
a separation that can't happen, past z19 before an unrelated internal
limit stopped it.

Confirmed directly: killing an in-progress `-Z13`-only run (the `-Z`
floor alone doesn't fix this — it only avoids the low-zoom cost, not
the runaway) showed it had already reached **z20** before being
terminated.

## The fix: bounded zoom range, split build, `tile-join`

Two independent fixes, both applied:

1. **Never auto-detect zoom for the detail layer.** Replace
   `-zg --extend-zooms-if-still-dropping` with an explicit, bounded
   range (`-Z<min> -z<max>`) for any tippecanoe build that includes
   connector-line features.
2. **Skip near-zero-length connectors at the source** (not yet
   measured standalone, but should reduce how often the pathological
   case even arises): compute the haversine distance between the ATP
   point and OSM centroid: below a small threshold, don't emit the
   `link` feature at all — there's nothing useful to draw, and it's
   exactly the degenerate geometry that caused the runaway.

Building on that: `tippecanoe` also ships `tile-join`, its own
documented tool for exactly the "coarse overview vs. high-zoom detail"
split (see `man tippecanoe`'s "Show countries at low zoom levels but
states at higher zoom levels" example, `-z3`/`-Z4` + `tile-join`).
Applied here:

- **Overview** = the already-shipped `conflated.pmtiles` (single
  feature per row, z0–12) — reused unmodified, no rebuild needed.
- **Detail** = the 3-feature dataset, *matched rows only* (unmatched
  rows are already a single point at every zoom, so they don't belong
  in the detail pass at all), built with a hard `-Z13 -z16` range.
- Merged with `tile-join` (which warns about the differing max zoom
  between inputs — expected, not an error).

| Step | Wall time | Peak RSS | Output |
|---|---:|---:|---:|
| Detail pass (`-Z13 -z16`, matched only) | 2m11s | 2.87 GB | 1.1 GB |
| `tile-join` merge | 3m09s | 2.37 GB | 1.8 GB (final) |
| **Total added over shipped #709** | **5m20s** | ≤ 2.87 GB | **+1.1 GB** |

vs. 19m55s / +4.6 GB for the unrestricted, runaway build. Every step
individually stays well inside the production container's 12 GB
`--mem-limit`.

## Shipping this for real

Decisions made when turning this into pipeline code (tracked in the
PR that follows this note):

- `atp` features carry only `atp:*` tags, `osm` features only `osm:*`
  — today's prototype put both tag sets on every feature, which made
  it hard to tell which end of a pair you were looking at in a tile
  inspector. The `link` feature keeps both.
- The z13/z16 zoom split and the minimum-connector-length threshold
  each become a single, well-documented constant in
  `src/pipeline/conflated_tiles.rs` — not scattered magic numbers.
- `tile-join` is a sibling binary from the exact same
  `felt/tippecanoe` build (`Containerfile`) already produces
  `tippecanoe` from, at the same pinned commit — shipping it means one
  more `COPY` line in the container stage, plus a small, mechanical
  second component in `scripts/sbom/tippecanoe.jq` (same version/
  supplier/license/musl-sqlite-zlib dependencies as the existing
  `tippecanoe` entry), not new supply-chain investigation.

## Open, not yet answered

- Is z16 the right ceiling, or should it go deeper? Picked as a first
  guess from what "individual buildings become legible" roughly
  implies, not a measured optimum.
- What connector-length threshold is right? Real observed offsets in
  this run ranged from ~0.2 m (a coincident match) to the matcher's
  own ~400 m search-radius cap; a few meters (comfortably above GPS/
  geocoding noise, well below "meaningfully offset") is the working
  assumption.
