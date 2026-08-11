# Supply-chain security: concepts

The “software supply chain” is everything between source code and
something you can actually run: dependencies, build tools, the build
process itself. This document explains the security practices we use
around ours (see [`RELEASING.md`](RELEASING.md) for the actual,
repo-specific steps). It’s written for someone who can code but hasn’t
necessarily done release engineering before. None of it is original to
`osm-diffs` — everything here is standard practice, just explained in
one place.

## Containers

A container packages an application with everything it needs to run —
binaries, libraries, configuration — into one portable unit that
behaves the same way on any machine. It’s much lighter than a full
virtual machine, since it shares the host’s kernel instead of running
its own, while still keeping what’s inside isolated from everything
else on that host.

We publish `osm-diffs` as an [OCI image](https://opencontainers.org/) —
the open, vendor-neutral standard most container tooling implements
today — to a
[container registry](https://www.redhat.com/en/topics/cloud-native-apps/what-is-a-container-registry)
(GitHub’s, specifically): a place to store and share container images,
the way a package registry does for libraries.
[`Containerfile`](../Containerfile) is the
[recipe that builds ours](https://github.com/containers/container-libs/blob/main/common/docs/Containerfile.5.md):
a plain-text file listing the build steps, one instruction per line.

## Minimal containers

`Containerfile` builds in two stages, and only the second one ships:
`FROM scratch`, containing nothing but the `osm-diffs` and `tippecanoe`
binaries. The whole build toolchain (Rust, a C compiler, `apk`, `git`,
...) stays behind in the discarded first stage. Both binaries are
statically linked against [musl](https://en.wikipedia.org/wiki/Musl),
Alpine’s lightweight, security-focused C library
([musl.libc.org explains why](https://musl.libc.org/about.html)), so
there’s not even a shared C library in the final image — let alone a
shell or a package manager — and the process runs as an unprivileged
user, not `root`. If someone found a way to run arbitrary code inside
this container, there’s nothing there to run it with — see
[this explanation of container attack-surface reduction](https://www.minimus.io/post/container-image-attack-surface-reduction)
for why that matters.

## Multi-architecture containers

Our image is published for both `amd64` and `arm64`, mainly so a
developer can just pull it and run it locally to debug — including on
an Apple Silicon Mac, without hunting down an old Intel machine — even
though it’s meant to run as a weekly batch job in a datacenter. ARM is
also gaining ground in datacenters for its power efficiency, so
publishing both architectures now eases an eventual production move.

Mechanically, a multi-architecture image is just an
[image index](https://github.com/opencontainers/image-spec/blob/main/image-index.md)
(a manifest list): a small document pointing to one architecture-specific
image per platform. Pulling by tag automatically fetches the right one
for your machine.

## Bill of Materials (BOM)

A Bill of Materials is a structured, machine-readable record of what
went into producing something — like the ingredients list on a food
package. It’s most commonly applied to software (a *Software* Bill of
Materials, SBOM: which libraries a program depends on, which compiler
and build tools were used, under which licenses), but nothing about the
idea is software-specific; it applies to any produced artifact.

A BOM lets anyone — a downstream user, a security team, an auditor —
answer questions like “does this contain a vulnerable version of
library X?” without rebuilding the software or reading its source.

## SBOM and CBOM

Our SBOM describes the container image we publish: the Rust dependency
graph of the `osm-diffs` binary, the statically linked `tippecanoe`
binary and its libraries, build-environment details, and licenses.

Alongside it, our SBOM includes a small
[Cryptographic Bill of Materials](https://en.wikipedia.org/wiki/Cryptographic_bill_of_materials)
(CBOM): the same idea, but for cryptography — which TLS version, cipher
suites, and crypto backend the binary uses, for example when uploading
its output to S3-compatible storage.

Both are generated from real, current build state, not maintained by
hand — see [`scripts/sbom/README.md`](../scripts/sbom/README.md) for how.

## CycloneDX

We publish our SBOM (and CBOM) in [CycloneDX](https://cyclonedx.org/)
format, an open standard for this kind of document (see the
[CycloneDX Wikipedia article](https://en.wikipedia.org/wiki/CycloneDX)
for background). Using a standard format, rather than inventing our
own, means existing tools (vulnerability scanners, license auditors,
`cyclonedx-cli`) can consume it without knowing anything
project-specific.

## Build provenance and attestations

Knowing what’s *in* an artifact is only half the story — you also want
to know it was actually *built* the way you expect, rather than
tampered with somewhere between the build and reaching you. A build
provenance attestation records that: a cryptographically signed
statement, tied to the exact artifact digest, saying “this was built by
workflow W, from commit C, on GitHub-hosted infrastructure, triggered
by event E.”

We target
[SLSA Build Level 3](https://slsa.dev/spec/v1.2/build-track-basics#build-l3):
provenance that isn’t just signed, but generated somewhere the build
itself can’t influence or forge. That’s why `release.yml`’s `attest`
job runs separately from `build`/`manifest` — the signing credentials
are never exposed to the steps that compile arbitrary code, the part an
attacker is more likely to reach.

We use GitHub’s native
[artifact attestations](https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds)
feature for this, backed by [Sigstore](https://www.sigstore.dev/) (open
tooling for signing software without managing your own keys). It signs
and publishes both a provenance and an SBOM attestation per image, so
anyone who pulls it can verify what’s in it and that it was really
built by our workflow — see [`RELEASING.md`](RELEASING.md) for exactly
which attestations exist and how to check them.

One nuance for a multi-architecture image like ours: each
per-architecture image gets its own SBOM and provenance attestation,
while the top-level manifest list gets only provenance — it has no
software content of its own to describe. This matches current
mainstream practice; see
[alltheplaces/osm-diffs#589](https://github.com/alltheplaces/osm-diffs/issues/589)
for where it might evolve.

## Immutable releases

Attestations prove a release was built correctly *once*. Immutability
protects that guarantee over time: once published, GitHub locks a
release’s git tag to that exact commit — it can’t be moved or deleted
while the release exists, and even a deleted release’s tag name can
never be reused. That closes off a whole class of attack where a
previously-trusted, already-verified tag gets quietly repointed later.

## Why all of this, together

None of these pieces alone is enough. An SBOM without provenance tells
you what’s in an artifact but not whether it was built honestly.
Provenance without immutability can be quietly invalidated by moving
the tag it refers to. Together, they form a chain a downstream consumer
can verify: *this tag* can’t have been swapped (immutability), *this
image* was built by our workflow from our source (provenance), and
*this image* contains what it claims to (SBOM/CBOM).

## Is this overkill for a project like this?

Fair question. `osm-diffs` is open source, processes only public data,
and handles no secrets — our actual threat model is modest compared to
a service handling user data or credentials. So no, this
isn’t strictly *necessary* the way it would be for many other projects.

But it’s a one-time cost: once set up, it runs fully automated, adding
no effort to any future release. Cheap insurance, not a real trade-off
against other work.

It’s also about where things are heading, not just where they stand
today: supply-chain attacks aren’t hypothetical anymore (the
[xz-utils backdoor](https://en.wikipedia.org/wiki/XZ_Utils_backdoor)
being the starkest recent example), and what this document describes
is becoming a baseline expectation, not something only
security-sensitive projects bother with.
