# Output files

This directory documents the files this project produces for public
consumption — what’s in them, and how to read them. (For how the
pipeline itself is built and tested, see the parent [`docs/`](../)
directory instead — those pages are for people working on the
pipeline’s code, not for people using its output.)

- [`CONFLATED_PARQUET.md`](CONFLATED_PARQUET.md) — `conflated.parquet`,
  pairing AllThePlaces features with their matching OpenStreetMap
  features.

## Downloading the current output

The pipeline isn’t running in production yet (see
[`docs/TECHNICAL_DESIGN.md`](../TECHNICAL_DESIGN.md#status)), so
there’s no fixed, permanent place to fetch its output from. For now,
though, the latest `conflated.parquet` is publicly downloadable at
<https://cdn.diffed-places.org/conflated.parquet>, and the matching
PMTiles archive can be inspected visually, right in the browser, at
<https://pmtiles.io/#url=https://cdn.diffed-places.org/edits.pmtiles&inspectFeatures=true>.

**This URL is temporary.** Once the pipeline moves to production,
output will be published at a different, hopefully permanent
location, and this one will stop being updated. Don’t build anything
that depends on `cdn.diffed-places.org` staying around.
