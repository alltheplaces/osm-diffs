# Supply-chain security: concepts

This document explains the concepts behind the supply-chain security
practices used in this project’s release process (see
[`RELEASING.md`](RELEASING.md) for the actual, repo-specific steps). It’s
written for someone who can code but hasn’t necessarily done release
engineering before.

## Bill of Materials (BOM)

A Bill of Materials is a structured, machine-readable record of what went
into producing something — similar in spirit to the list of ingredients
on a food package. The idea is most commonly applied to software (a
*Software* Bill of Materials, SBOM: which libraries a program depends on,
which compiler and build tools were used, under which licenses), but
nothing about it is specific to software. The same idea applies to any
produced artifact: a BOM for a dataset could record which pipeline
version, which input files, and when it was produced.

A BOM lets anyone — a downstream user, a security team, an auditor —
answer questions like “does this contain a vulnerable version of library
X?” without having to rebuild the software or read its source.

## SBOM and CBOM

Our **S**oftware **B**ill of **M**aterials describes the container image
we publish: the Rust dependency graph of the `osm-diffs` binary, the
statically linked `tippecanoe` binary and the libraries baked into it,
build-environment details (compiler and OS versions), and licenses.

Alongside it, our SBOM includes a small **C**ryptographic **B**ill of
**M**aterials (CBOM): the same idea, but for cryptography instead of
dependencies — which TLS version, cipher suites, and crypto backend the
binary uses, for example when uploading its output to S3-compatible
storage. This matters because that’s exactly the kind of thing that
can go stale silently: nothing about a routine dependency update would
otherwise tell you that the set of cipher suites your binary supports has
changed.

Both are generated from real, current build state, not maintained by
hand — see [`scripts/sbom/README.md`](../scripts/sbom/README.md) for how.

## Minimal containers

`Containerfile` builds in two stages, and only the second one ships:
`FROM scratch`, containing nothing but the `osm-diffs` and `tippecanoe`
binaries. The entire build toolchain (Rust, a C compiler, `apk`, `git`,
...) stays behind in the discarded first stage. Both binaries are
statically linked against musl (Alpine’s C library) rather than
dynamically against glibc, so there isn’t even a shared C library in the
final image — let alone a shell or a package manager. The process also
runs as an unprivileged user, not `root`. If someone found a way to run
arbitrary code inside this container, there’s nothing there to run: no
`/bin/sh`, nothing to fetch a second-stage payload with, and no root
privileges to do more damage with even so. This is the same idea behind
[Distroless](https://github.com/GoogleContainerTools/distroless) images,
not something specific to `osm-diffs`.

## CycloneDX

We publish our SBOM (and CBOM) in [CycloneDX](https://cyclonedx.org/)
format, an open, machine-readable standard for this kind of document (see
the [CycloneDX Wikipedia article](https://en.wikipedia.org/wiki/CycloneDX)
for background). Using a standard format, rather than inventing our own,
means existing tools (vulnerability scanners, license auditors,
CI validators like `cyclonedx-cli`) can consume it without needing to
understand anything project-specific.

## Build provenance and attestations

Knowing what’s *in* an artifact (the SBOM) is only half the story —
you also want to know that it was actually *built* the way you expect,
by the CI pipeline you trust, from the source you expect, rather than
tampered with somewhere between the build and reaching you. That’s what a
build provenance attestation records: a cryptographically signed
statement, tied to the exact artifact digest, that says “this was built
by workflow W, from commit C, on GitHub-hosted infrastructure, triggered
by event E.”

This specifically targets
[SLSA Build Level 3](https://slsa.dev/spec/v1.2/build-track-basics#build-l3):
provenance that isn’t just signed, but generated somewhere the build
itself can’t influence or forge. That’s why `release.yml`’s `attest` job
runs as a separate job from `build`/`manifest` — the credentials used to
sign the provenance are never exposed to the steps that actually compile
arbitrary code, which is the part an attacker is more likely to reach.

We use GitHub’s native
[artifact attestations](https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds)
feature for this (backed by [Sigstore](https://www.sigstore.dev/)), which
signs and publishes both a build-provenance attestation and an
SBOM attestation for our container images, so anyone who pulls the image
can verify both what’s in it and that it was really built by our GitHub
Actions workflow — see [`RELEASING.md`](RELEASING.md) for exactly which
attestations get created and how to check them yourself.

One nuance worth knowing for a multi-architecture image like ours: the
per-architecture images each get their own SBOM and provenance
attestation (since their actual content, e.g. the compiled binaries,
genuinely differs by architecture), while the top-level multi-arch
manifest list gets a provenance attestation but not its own separate
SBOM — it has no software content of its own, it’s a pure routing
document pointing at the per-architecture images. This matches current
mainstream practice (see the research summarized on
[alltheplaces/osm-diffs#589](https://github.com/alltheplaces/osm-diffs/issues/589)
for where this might evolve).

## Immutable releases

Attestations prove a release was built correctly *once*. Immutability
protects that guarantee over time: once a release is published, GitHub
locks its git tag to that exact commit — it can’t be moved, and it can’t
be deleted while the release exists. Even if the release is deleted
outright, its tag name can never be reused for a new release. This closes
off a whole class of supply-chain attack where a previously-trusted,
already-verified tag gets quietly repointed at something else later.

## Why all of this, together

None of these pieces alone is enough. An SBOM without provenance tells
you what’s in an artifact but not whether to trust that it was built
honestly. Provenance without immutability can be quietly invalidated
later by moving the tag it refers to. Together, they form a chain a
downstream consumer can actually verify: *this exact tag* can’t have been
silently swapped (immutability), *this exact image* was built by our
workflow from our source (provenance), and *this exact image* contains
what it claims to (SBOM/CBOM).

## Is this overkill for a project like this?

Fair question. `osm-diffs` is open source, processes only publicly
available data, and doesn’t handle secrets. Our actual threat model is
modest compared to, say, a proprietary service handling user data or
credentials — nobody stands to gain much by compromising this pipeline.
So no, this level of supply-chain hardening isn’t strictly *necessary*
the way it would be for many other projects.

But it’s a one-time cost: once set up, it runs entirely automated, adding
no ongoing effort to any future release. Given that, doing it properly
is cheap insurance rather than a real trade-off against something else
we’d otherwise be spending that effort on.

It’s also a reasonable bet on where the industry is heading, not just
where it stands today: supply-chain attacks aren’t hypothetical anymore
(the [xz-utils backdoor](https://en.wikipedia.org/wiki/XZ_Utils_backdoor)
being the starkest recent example), and the regulation and tooling
responding to them — the practices this document describes among them —
are becoming baseline expectations rather than something only
security-sensitive projects bother with.
