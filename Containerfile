# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# This file is used in continuous integration to automatically
# build containers, invoked from .github/workflows/release.yml.
# As a developer, you do not need to build production containers.
# However, here’s how to test a change to this file:
#
#     mkdir artifacts
#     podman build -t test-container                                    \
#         --build-arg BUILD_TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")  \
#         --volume $(pwd)/artifacts:/artifacts                          \
#         -f Containerfile .
#     podman run -t test-container --help


# ----------------------------------------------------------------------------
#  Stage 1.1: Setup
# ----------------------------------------------------------------------------

FROM rust:1.98.0-alpine3.23 AS builder

ARG BUILD_TIMESTAMP
ARG IMAGE_NAME=alltheplaces/osm-diffs
ARG TIPPECANOE_VERSION=2.79.0
# Commit that the "2.79.0" tag pointed to as of 2026-08-08. Pinned by
# commit, not by tag/branch name: git tags are mutable references that
# can be force-moved (accidentally or maliciously) after the fact, while
# fetching by commit SHA is content-addressed and can't silently drift.
# Update alongside TIPPECANOE_VERSION when bumping to a newer release.
ARG TIPPECANOE_COMMIT=68ab8dcc229f95b8b25877697d5e8d66783af503

# TODO(#556): Take cargo-cyclonedx from stable Alpine Linux (not edge)
# once Alpine 3.24 has been released.
RUN echo "@edge https://dl-cdn.alpinelinux.org/alpine/edge/community" >> /etc/apk/repositories && \
    apk update && \
    apk add \
      bash \
      build-base \
      cargo-cyclonedx@edge \
      cmake \
      git \
      jq \
      protoc \
      sqlite-static \
      sqlite-dev \
      zlib-static \
      zlib-dev


# ----------------------------------------------------------------------------
#  Stage 1.2: Build statically linked tippecanoe binary
# ----------------------------------------------------------------------------
#
# `make install` also installs tile-join -- a sibling binary from this
# same source tree/commit, used by pipeline::tiles::join_tiles to merge
# conflated.pmtiles' overview and detail passes (see
# pipeline::conflated_tiles' module doc comment). Not a second
# dependency to track: same repo, same commit, same static-link
# treatment, same license.

WORKDIR /build/tippecanoe

RUN git init -q . && \
    git remote add origin https://github.com/felt/tippecanoe.git && \
    git fetch --depth 1 origin "${TIPPECANOE_COMMIT}" && \
    git checkout -q FETCH_HEAD

RUN make -j"$(nproc)" \
        PREFIX=/usr/local \
        LDFLAGS="-static -static-libgcc -static-libstdc++" && \
    make install PREFIX=/usr/local && \
    strip --strip-all /usr/local/bin/tippecanoe /usr/local/bin/tile-join

# Sanity-check: confirm both binaries are truly statically linked
RUN for bin in tippecanoe tile-join; do \
        readelf -d "/usr/local/bin/$bin" 2>&1 | grep -q NEEDED \
        && (echo "✗ dynamic deps detected in $bin" && exit 1) \
        || echo "✓ no dynamic library dependencies in $bin"; \
    done


# ----------------------------------------------------------------------------
#  Stage 1.3: Build and test osm-diffs binary
# ----------------------------------------------------------------------------

WORKDIR /usr/osm-diffs

COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml .
COPY scripts/sbom scripts/sbom
COPY src src
COPY tests tests

RUN cargo build --release --locked
RUN cargo test --release --locked


# ----------------------------------------------------------------------------
#  Stage 1.4: Generate the SBOM for the container
# ----------------------------------------------------------------------------
#
# Runs after both binaries (tippecanoe and osm-diffs) have been built, so a
# single script invocation can see the facts about both of them.

RUN sh scripts/sbom/generate-sbom.sh /artifacts/sbom.cdx.json


# ----------------------------------------------------------------------------
#  Stage 2: Package binaries into a scratch container
# ----------------------------------------------------------------------------

FROM scratch

ARG BUILD_TIMESTAMP
ARG VCS_REF
ARG VCS_URL

COPY --from=builder /usr/local/bin/tippecanoe /usr/local/bin/tippecanoe
COPY --from=builder /usr/local/bin/tile-join /usr/local/bin/tile-join
    
COPY --from=builder --chown=1000:1000  \
    /usr/osm-diffs/target/release/osm-diffs  \
    /app/osm-diffs

USER 1000

ENTRYPOINT ["/app/osm-diffs"]

LABEL  \
    org.opencontainers.image.authors="Sascha Brawer <sascha@brawer.ch>"  \
    org.opencontainers.image.created=$BUILD_TIMESTAMP  \
    org.opencontainers.image.description="Data pipeline for alltheplaces/osm-diffs"  \
    org.opencontainers.image.licenses="MIT"  \
    org.opencontainers.image.revision=$VCS_REF  \
    org.opencontainers.image.source=$VCS_URL  \
    org.opencontainers.image.vendor="alltheplaces.xyz"
