# Cutting a release

This document is the practical how-to for cutting a release of
`osm-diffs`. For the concepts behind *why* the process looks like this
(SBOM, attestations, immutable releases, ...), see
[`supply-chain-security.md`](supply-chain-security.md).

## Quick start

```sh
./scripts/cut-release.sh vX.Y.Z
```

Run it from a clean, up-to-date `main` checkout. It handles everything
else: bumping the version, opening and merging the PR for that, tagging,
and publishing the release. See “What happens automatically” below for
the full sequence.

## Choosing the version number

This is the one genuine judgment call in the whole process; everything
else is mechanical. We follow [SemVer](https://semver.org/), driven by
the pipeline’s **output schema** — not by how much code changed:

- **major** — the output schema changed in a way that breaks existing
  clients (a field was removed or renamed, a type changed, ...)
- **minor** — the output schema evolved in a backward-compatible way
  (e.g. a new optional field was added)
- **patch** — bugfixes only, no schema changes

A release that touches a lot of internal code but doesn’t change what
clients read is still a patch release. A release that changes the output
schema in a breaking way is a major release even if the code diff is
tiny.

## What happens automatically, step by step

Once you run `cut-release.sh vX.Y.Z`:

1. **Preconditions are checked**: you’re on a clean, up-to-date `main`;
   `vX.Y.Z` doesn’t already exist; it’s actually newer than `Cargo.toml`’s
   current version; the latest CI run on `main`’s current commit passed.
   Any failure here stops immediately, before anything is pushed.
2. **A version-bump PR is opened**: `Cargo.toml`’s version (and
   `Cargo.lock`’s self-entry) is bumped on a new branch, and a PR titled
   “Bump version to X.Y.Z” is opened against `main`. `main` is protected
   (PR required, status checks required, merge queue), so this can’t be
   pushed directly.
3. **The script waits** for that PR to clear required checks and the
   merge queue, and actually land on `main`. This normally takes a few
   minutes (the required test job takes ~4-5 min; the merge queue has a
   minimum 3 min wait).
4. **The release is created**: a GitHub Release for `vX.Y.Z` is published
   at the new `main` HEAD, with auto-generated notes (categorized per
   [`.github/release.yml`](../.github/release.yml)’s label rules). This
   creates the underlying git tag as a side effect, and the release is
   immutable from this point on (see
   [`supply-chain-security.md`](supply-chain-security.md#immutable-releases)).
5. **That tag push triggers
   [`.github/workflows/release.yml`](../.github/workflows/release.yml)**,
   entirely independently of the script:
   - `verify-version`: re-checks the tag matches `Cargo.toml`’s version
     (a server-side safety net — this should never fail if you used the
     script, since the script only ever tags a version it just set).
   - `build` (once per architecture, amd64 and arm64): builds the
     container via `Containerfile`, which also generates the SBOM (see
     [`scripts/sbom/README.md`](../scripts/sbom/README.md)) and pushes
     each architecture’s image to `ghcr.io` by digest.
   - `manifest`: combines both architectures into a multi-arch manifest,
     tagged both `vX.Y.Z` and `latest`.
   - `attest`: publishes signed SBOM and build-provenance attestations
     for both per-architecture images, plus a build-provenance
     attestation for the manifest list.

Steps 1-4 usually take under 10 minutes; step 5 (the actual container
build) takes roughly another 15-20 minutes.

## Verifying a release

To double-check a release actually came out right, rather than just
trusting that the workflow went green:

```sh
# Cargo.toml on main should match the tag you just cut
git show origin/main:Cargo.toml | grep '^version'

# the release.yml run for your tag should show all jobs succeeding
gh run list --workflow=release.yml --limit 1

# attestations exist for the published image (note: gh attestation
# verify's default --predicate-type filter only shows build provenance;
# query the raw API to see the SBOM attestation too)
gh api "repos/alltheplaces/osm-diffs/attestations/sha256:<digest>" \
  | jq -r '.attestations[].bundle.dsseEnvelope.payload' \
  | while read -r p; do echo "$p" | base64 -d | jq -r .predicateType; done
# expect: https://slsa.dev/provenance/v1 and https://cyclonedx.org/bom
```

(This is exactly what was done to confirm v0.6.9, the first release cut
with this script — see the comment trail on
[alltheplaces/osm-diffs#562](https://github.com/alltheplaces/osm-diffs/pull/562)
for the full walkthrough, including how to find the per-architecture
digests via the release’s uploaded artifacts.)

## Rules

- **Always cut releases with `cut-release.sh`.** Never push a `v*` tag by
  hand. `release.yml` would still build and publish a container for it,
  but you’d have skipped the version-consistency checks, and immutability
  protections apply to tags that went through a real GitHub Release —
  not to a bare tag pushed directly.
- **Releases are immutable. If one’s bad, cut a new patch version and
  leave the bad one as-is.** You can’t fix a published release in place,
  and you can’t reuse or move its tag even if you delete it.
- **Anyone with write access can cut a release.** There’s no separate
  approval gate for this beyond the normal merge-queue checks — we don’t
  have enough people for dedicated release roles.

## If it goes wrong

- **A precondition check fails** (dirty tree, stale `main`, tag exists,
  version not newer, CI not green): nothing was pushed. Fix the
  underlying issue and re-run.
- **The bump PR fails its required checks**: auto-merge won’t complete,
  and the script eventually times out (~25 min) with the PR still open.
  Look at why CI failed on that PR, fix it, and either let auto-merge
  finish or close the PR and start over.
- **The bump PR merges, but the release itself fails to get created**
  (rare): `main` now has the version bump, but re-running the whole
  script will fail its “is this version newer” check, since `Cargo.toml`
  already matches. Just run the release-creation step directly instead:
  `gh release create vX.Y.Z --target main --generate-notes`.
- **The tag/release gets created, but `release.yml` then fails** (e.g. a
  build failure): the release is already immutable at this point, so
  there’s no “redo.” Fix whatever broke the build (or `main`), and cut a
  new patch version. The failed tag’s GitHub Release will just exist
  without a correspondingly published, attested container — that’s a
  known, accepted consequence of immutability, not a bug.

## Where things live

- [`scripts/cut-release.sh`](../scripts/cut-release.sh) — the script
  itself
- [`.github/workflows/release.yml`](../.github/workflows/release.yml) —
  build, SBOM, attest
- [`Containerfile`](../Containerfile) — how the container gets built
- [`.github/release.yml`](../.github/release.yml) — changelog
  categorization rules for auto-generated release notes
- [`scripts/sbom/README.md`](../scripts/sbom/README.md) — how the SBOM
  itself is generated

## Known gaps, not yet in place

- **Production deployment isn’t wired up yet.** This process ends at “a
  correctly built, SBOM’d, attested container sits in `ghcr.io`” — what
  happens after that, to actually run this in production, doesn’t exist
  yet and isn’t covered here.
- A few low-priority, deliberately-deferred items are tracked separately
  and don’t block anything: automated freshness checks for vendored
  dependencies
  ([#555](https://github.com/alltheplaces/osm-diffs/issues/555)), moving
  `cargo-cyclonedx` off Alpine’s edge repo once it’s available in stable
  ([#556](https://github.com/alltheplaces/osm-diffs/issues/556)),
  embedding the release version into the pipeline’s actual output data,
  not just its logs
  ([#588](https://github.com/alltheplaces/osm-diffs/issues/588)), and
  watching for an emerging standard on index-level SBOMs for multi-arch
  images ([#589](https://github.com/alltheplaces/osm-diffs/issues/589)).
