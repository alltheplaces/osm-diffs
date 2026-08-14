# Scripts

Source generators the Rust build depends on, release-engineering
scripts, and ad hoc tooling for testing development branches on real
hardware.

## Source generation

- [`generate_id_tagging_schema.py`](generate_id_tagging_schema.py):
  generates `src/pipeline/osm/generated.rs` from upstream
  [`id-tagging-schema`](https://github.com/openstreetmap/id-tagging-schema)
  data. Run via `uv run scripts/generate_id_tagging_schema.py`.
- [`vendor-osm-testdata-grid.sh`](vendor-osm-testdata-grid.sh): vendors
  the OSM test fixtures used by `tests/test_data/osm-testdata-grid/`
  from a pinned commit of
  [`osm-testdata`](https://github.com/osmcode/osm-testdata).

Both are run by hand when someone notices upstream has moved; nothing
here notifies you of a new release. Automating that is a low-priority,
deliberately deferred feature request, tracked in
[alltheplaces/osm-diffs#555](https://github.com/alltheplaces/osm-diffs/issues/555).

## Release engineering

See [`../docs/RELEASING.md`](../docs/RELEASING.md) for the full release
process these fit into.

- [`sbom/`](sbom/README.md): generates the Software Bill of Materials
  (SBOM) for the release container image.
- [`cut-release.sh`](cut-release.sh): cuts a new release (bumps
  `Cargo.toml`’s version via a PR, then creates the tagged GitHub Release
  that triggers `.github/workflows/release.yml`, then waits for and
  verifies the result). Run `./scripts/cut-release.sh vX.Y.Z` from an
  up-to-date `main`.
- [`verify-release.sh`](verify-release.sh): the verification step
  `cut-release.sh` runs at the end — also usable standalone, e.g. to
  double-check an older release. Run
  `./scripts/verify-release.sh vX.Y.Z`.

## Testing development branches

Unrelated to how `osm-diffs` actually ships to production — this is for
ad hoc validation of a branch before it lands.

- [`test-branch-on-hetzner/`](test-branch-on-hetzner/README.md): spins
  up a Hetzner Cloud VM, builds a given git branch on it, runs the
  pipeline against it, and pulls back logs — one command instead of
  repeating the manual setup by hand each time. See
  [alltheplaces/osm-diffs#667](https://github.com/alltheplaces/osm-diffs/issues/667)
  for why this exists.
- [`test-branch-on-macos/`](test-branch-on-macos/README.md): the same
  idea, much smaller — build and run the current checkout locally with
  a `vm_stat`/RSS monitor alongside it, for fast local iteration rather
  than matching production hardware/toolchain. See
  [alltheplaces/osm-diffs#669](https://github.com/alltheplaces/osm-diffs/issues/669).
