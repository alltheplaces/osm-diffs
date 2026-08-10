# Scripts

Scripts used to generate some Rust sources in this repository, plus
release-engineering scripts that aren’t part of the Rust build itself.
See [`../docs/releasing.md`](../docs/releasing.md) for the full release
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
