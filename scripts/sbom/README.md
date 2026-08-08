# SBOM generation

This directory holds the scripts that generate the Software Bill of
Materials (SBOM) for the `osm-diffs` container image.

## What's an SBOM, and why do we have one?

A Software Bill of Materials is a structured, machine-readable inventory of
everything that went into building a piece of software: which libraries it
depends on, which compiler and build tools were used, under which licenses
the various pieces are published, and so on -- similar in spirit to the
list of ingredients on a food package. It lets anyone (a downstream user, a
security team, an auditor) answer questions like "does this container
contain a vulnerable version of library X?" without having to rebuild the
software or read its source.

Alongside it, our SBOM includes a small Cryptographic Bill of Materials
(CBOM): the same idea, but for cryptography instead of dependencies --
which TLS version, cipher suites and crypto backend we use. See the
`crypto/protocol/tls-1.3` entry in [`pipeline.jq`](pipeline.jq).

We publish our SBOM in [CycloneDX](https://cyclonedx.org/) format, an
open standard for this kind of document (see the
[CycloneDX Wikipedia article](https://en.wikipedia.org/wiki/CycloneDX) for
background). For every release, the generated SBOM gets attached to our
container image on GitHub Container Registry as a signed
[attestation](https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds),
so anyone who pulls the image can verify both what's in it and that it was
really built by our GitHub Actions workflow.

## When and how it runs

The SBOM gets generated automatically whenever a release container is
built: after a git tag such as `v1.2.3` is pushed, GitHub Actions runs
[`.github/workflows/release.yml`](../../.github/workflows/release.yml),
which invokes `podman build` on [`Containerfile`](../../Containerfile) to
build the container image. As one of the last steps of that build,
`Containerfile` runs [`generate-sbom.sh`](generate-sbom.sh), which writes a
single `sbom.cdx.json` file into a directory that's mounted from the host
(so we can read it without baking it into the shipped image itself).

One thing `generate-sbom.sh` cannot know at that point is the digest of
the finished container image -- that digest only exists once `podman
build` has completed, which is after the SBOM was already written. So
`release.yml` patches the digest in afterwards, with a one-line `jq`
expression; see the comment in [`merge.jq`](merge.jq) for why that's safe.

`.github/workflows/test-container.yml` runs the same `Containerfile` build
(without publishing anything) on every pull request that touches
`Containerfile` or this directory, and validates the resulting SBOM with
[`cyclonedx-cli`](https://github.com/CycloneDX/cyclonedx-cli).

## How it works

`generate-sbom.sh` gathers information about the build environment (Alpine
Linux version, compiler versions, library versions, ...) using standard
tools such as `apk`, `sed`, `awk` and `grep`. It then:

1. Runs `cargo cyclonedx` to get the Rust dependency graph for the
   `osm-diffs` binary, in CycloneDX 1.5 format (the newest that
   `cargo cyclonedx` currently supports).
2. Pipes that through [`pipeline.jq`](pipeline.jq), which upgrades it to
   CycloneDX 1.7 and enriches it with build-environment metadata,
   supplier/license info, and the vendored "data" components
   (`id-tagging-schema`, `osm-testdata-grid`) that `cargo cyclonedx` has no
   way to see.
3. Builds a CycloneDX fragment for the statically linked `tippecanoe`
   binary from scratch, with [`tippecanoe.jq`](tippecanoe.jq).
4. Combines both fragments into the final, single SBOM for the container,
   with [`merge.jq`](merge.jq).

None of the `.jq` files are invoked directly; `generate-sbom.sh` always
passes the gathered facts to them as `--arg`/`--slurpfile` parameters, so
none of the shell script's data ever needs to be patched into the `jq`
source itself.

### Running it outside of the release build

You can run `generate-sbom.sh` directly, e.g. on a macOS development
machine, to sanity-check its output:

```sh
./scripts/sbom/generate-sbom.sh /tmp/sbom.cdx.json
```

Some of the facts it normally reads via Alpine's `apk` (the exact musl,
sqlite and zlib versions statically linked into `tippecanoe`, the Alpine
version itself) aren't available outside of that build environment. In
that case, the script inserts placeholder values, prints a warning, and
marks the SBOM as `osm-diffs:sbom:devBuild` in its metadata properties.
Such an SBOM is useful for checking structural or semantic validity (e.g.
with `cyclonedx-cli validate`), but must not be treated as an accurate
description of a production build.
