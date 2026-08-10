# Scripts

Scripts used to generate some Rust sources in this repository, plus
release-engineering scripts that aren't part of the Rust build itself.

- [`sbom/`](sbom/README.md): generates the Software Bill of Materials
  (SBOM) for the release container image.
- [`cut-release.sh`](cut-release.sh): cuts a new release (bumps
  `Cargo.toml`'s version via a PR, then creates the tagged GitHub Release
  that triggers `.github/workflows/release.yml`). Run
  `./scripts/cut-release.sh vX.Y.Z` from an up-to-date `main`.
