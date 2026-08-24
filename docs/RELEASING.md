# Cutting a release

This document is the practical how-to for cutting a release of
`osm-diffs`. For the concepts behind *why* the process looks like this
(SBOM, attestations, immutable releases, ...), see
[`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md).

## Quick start

```sh
./scripts/cut-release.sh vX.Y.Z
```

Run it from a clean, up-to-date `main` checkout. It handles everything
else: bumping the version, opening and merging the PR for that, tagging,
and publishing the release. See “What happens automatically” below for
the full sequence — but read “Choosing the version number” first, it’s
the one part of this that actually needs a careful decision, not just
running a command.

## Choosing the version number

This is the one genuine judgment call in the whole process; everything
else is mechanical. We follow [SemVer](https://semver.org/), driven by
the pipeline’s **output schema** — not by how much code changed.

This is a different question than what [SemVer](https://semver.org/)
usually answers. Programmers normally think of SemVer in terms of API
compatibility for code that *links against* a library. Nobody links
against `osm-diffs`:
downstream clients only ever consume the *data* it produces. So the
question to ask isn’t “did the code change in a breaking way,” it’s “does
this change what a client reading our output has to handle differently”:

- **major** — the output schema changed in a way that breaks existing
  clients (a field was removed or renamed, a type changed, ...)
- **minor** — the output schema evolved in a backward-compatible way
  (e.g. a new optional field was added)
- **patch** — bugfixes only, no schema changes

A release that touches a lot of internal code but doesn’t change what
clients read is still a patch release. A release that changes the output
schema in a breaking way is a major release even if the code diff is
tiny.

**Before 1.0.0, a schema-breaking release bumps *minor*, not *major*.**
SemVer’s own spec is explicit that this is fine: [§4](https://semver.org/#spec-item-4)
says a `0.y.z` major version is for initial development, where “anything
MAY change at any time” and the public interface “SHOULD NOT be
considered stable” — SemVer deliberately leaves how `0.y.z` itself
increments up to the project. We use the common convention of treating
`0.MINOR.PATCH` the way `MAJOR.MINOR.PATCH` works post-1.0: a
schema-breaking release bumps `MINOR` (not `MAJOR`, which stays `0`),
anything else bumps `PATCH`. This isn’t just a convention we picked —
it’s the same rule `cargo`/crates.io itself uses for `0.x` dependency
resolution (a `^0.2.0` requirement excludes `0.3.0`, treating that
minor-version bump as the breaking one), so it’s already how our own
build tooling reasons about pre-1.0 versions. We’ll move to `MAJOR`
bumps for breaking changes once there’s an actual 1.0.0 to break
compatibility with — i.e. once this pipeline has real downstream
consumers depending on schema stability, not before.

`cut-release.sh` gives you one piece of help with this call, not a
replacement for it: it scans merged PR titles since the last tag for a
Conventional Commits `!` marker (see
[`CONTRIBUTING.md`](CONTRIBUTING.md#pr-titles-conventional-commits) — on
this project `!` means *output-schema-breaking*, not API-breaking) and
prints what it finds before asking you to confirm the version. Treat it
as a “did you forget something” prompt: false positives and false
negatives are both possible, since a PR’s type is chosen by whoever wrote
the title, not by inspecting the schema diff.

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
   minutes (the required test job takes ~4–5 min; the merge queue has a
   minimum 3 min wait).
4. **The release is created**: a GitHub Release for `vX.Y.Z` is published
   at the new `main` HEAD, with auto-generated notes (categorized per
   [`.github/release.yml`](../.github/release.yml)’s label rules). This
   creates the underlying git tag as a side effect, and the release is
   immutable from this point on (see
   [`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md#immutable-releases)).
5. **That tag push triggers
   [`.github/workflows/release.yml`](../.github/workflows/release.yml)**,
   entirely independently of the script. `release.yml` itself is just a
   thin caller: it hands off to
   [`.github/workflows/release-build.yml`](../.github/workflows/release-build.yml)
   as a reusable workflow — a separate file with its own identity is what
   gets this to SLSA Build Level 3 rather than Level 2 (see
   [`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md#build-provenance-and-attestations)).
   The called workflow runs:
   - `verify-version`: re-checks the tag matches `Cargo.toml`’s version
     (a server-side safety net — this should never fail if you used the
     script, since the script only ever tags a version it just set).
   - `build` (once per architecture, amd64 and arm64): builds the
     container via [`Containerfile`](../Containerfile), which also
     generates the SBOM (see
     [`scripts/sbom/README.md`](../scripts/sbom/README.md)) and pushes
     each architecture’s image to `ghcr.io` by digest.
   - `manifest`: combines both architectures into a multi-arch manifest,
     tagged both `vX.Y.Z` and `latest`.
   - `attest`: publishes signed SBOM and build-provenance attestations
     for both per-architecture images, plus a build-provenance
     attestation for the manifest list.
6. **The script waits for that workflow to finish**, then runs
   [`verify-release.sh`](../scripts/verify-release.sh) to confirm it
   actually came out right — not just that it reported success, but that
   both a build-provenance and an SBOM attestation genuinely exist for
   both architectures (see “Verifying a release” below).

Steps 1–4 usually take under 10 minutes; steps 5–6 (the actual container
build, plus verification) take roughly another 20–25 minutes.

## Verifying a release

This happens automatically as the last step of `cut-release.sh` — you
don’t need to do anything extra. It’s implemented as a separate script,
[`verify-release.sh`](../scripts/verify-release.sh), specifically so it
can also be run standalone at any later time, e.g. if `cut-release.sh`
didn’t get to finish (the machine running it crashed, the connection
dropped, ...), or to double-check an older release:

```sh
./scripts/verify-release.sh vX.Y.Z
```

It waits for (or, if already finished, immediately checks)
`release.yml`’s run for that tag, then confirms both a build-provenance
and an SBOM attestation exist for each per-architecture image — the same
two checks described in
[`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md#build-provenance-and-attestations),
done for real rather than assumed. This is exactly what was done by hand
to confirm v0.6.9, the first release cut with `cut-release.sh` — see the
comment trail on
[alltheplaces/osm-diffs#562](https://github.com/alltheplaces/osm-diffs/pull/562)
for that walkthrough, which is what `verify-release.sh` automates.

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
- **`cut-release.sh` is interrupted, or times out, while waiting on
  `release.yml`** (~20–25 min; a crashed machine, a dropped connection,
  or a truly stuck workflow): the release itself is unaffected — it was
  already created before this wait began. Just run
  `./scripts/verify-release.sh vX.Y.Z` on its own once you’re ready to
  check on it again; it picks up wherever the run currently stands.

## Where things live

- [`scripts/cut-release.sh`](../scripts/cut-release.sh) — the script
  itself
- [`scripts/verify-release.sh`](../scripts/verify-release.sh) — the
  verification step `cut-release.sh` runs at the end (also usable
  standalone)
- [`.github/workflows/release.yml`](../.github/workflows/release.yml) —
  triggers on a pushed tag, calls `release-build.yml`
- [`.github/workflows/release-build.yml`](../.github/workflows/release-build.yml) —
  build, SBOM, attest
- [`Containerfile`](../Containerfile) — how the container gets built
- [`.github/release.yml`](../.github/release.yml) — changelog
  categorization rules for auto-generated release notes
- [`scripts/sbom/README.md`](../scripts/sbom/README.md) — how the SBOM
  itself is generated

## Known gaps, not yet in place

- **Production deployment isn’t wired up yet.** This process ends at “a
  correctly built, SBOM’d, attested container sits in `ghcr.io`” — what
  happens after that, to actually run this in production (scheduling,
  where it runs), doesn’t exist yet. What it should take to get there
  once it does — hardware sizing, required configuration, what to
  monitor — is written down in
  [`PRODUCTION.md`](PRODUCTION.md), from real testing rather than
  guesswork.
- A few low-priority, deliberately-deferred items are tracked separately
  and don’t block anything: automated freshness checks for vendored
  dependencies
  ([#555](https://github.com/alltheplaces/osm-diffs/issues/555)), moving
  `cargo-cyclonedx` off Alpine’s edge repo once it’s available in stable
  ([#556](https://github.com/alltheplaces/osm-diffs/issues/556)), and
  watching for an emerging standard on index-level SBOMs for multi-arch
  images ([#589](https://github.com/alltheplaces/osm-diffs/issues/589)).
