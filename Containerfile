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

FROM rust:1.96.0-alpine3.23 AS builder

ARG BUILD_TIMESTAMP
ARG TIPPECANOE_VERSION=2.79.0

# TODO: Take cargo-cyclonedx from stable Alpine Linux (not edge)
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
      sqlite-static \
      sqlite-dev \
      zlib-static \
      zlib-dev


# ----------------------------------------------------------------------------
#  Stage 1.2: Build statically linked tippecanoe binary
# ----------------------------------------------------------------------------

WORKDIR /build/tippecanoe

RUN git clone --depth 1 --branch ${TIPPECANOE_VERSION} \
    https://github.com/felt/tippecanoe.git /build/tippecanoe

RUN make -j"$(nproc)" \
        PREFIX=/usr/local \
        LDFLAGS="-static -static-libgcc -static-libstdc++" && \
    make install PREFIX=/usr/local && \
    strip --strip-all /usr/local/bin/tippecanoe

# Sanity-check: confirm the binary is truly statically linked
RUN readelf -d /usr/local/bin/tippecanoe 2>&1 | grep -q NEEDED \
    && (echo "✗ dynamic deps detected" && exit 1) \
    || echo "✓ no dynamic library dependencies"

COPY sbom/build_tippecanoe_sbom.sh .
RUN sh build_tippecanoe_sbom.sh >/artifacts/tippecanoe.cdx.json


# ----------------------------------------------------------------------------
#  Stage 1.3: Build and test osm-diffs binary
# ----------------------------------------------------------------------------

WORKDIR /usr/osm-diffs

COPY Cargo.toml Cargo.lock .
COPY sbom sbom
COPY src src
COPY tests tests

RUN cargo build --release --locked
RUN cargo test --release --locked
RUN sh sbom/build-pipeline-sbom.sh /artifacts/pipeline.cdx.json


# ----------------------------------------------------------------------------
#  Stage 2: Package binaries into a scratch container
# ----------------------------------------------------------------------------

FROM scratch

ARG BUILD_TIMESTAMP
ARG VCS_REF
ARG VCS_URL

COPY --from=builder /usr/local/bin/tippecanoe /usr/local/bin/tippecanoe
    
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
