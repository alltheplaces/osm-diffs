#!/usr/bin/env sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Generate the CycloneDX Software Bill of Materials (SBOM) for the
# osm-diffs container image: one single file, describing both the
# osm-diffs binary (with its Rust dependency graph) and the statically
# linked tippecanoe binary that ships alongside it.
#
# Usage:
#   ./scripts/sbom/generate-sbom.sh [OUTPUT]
#
#   OUTPUT  path to write the generated SBOM
#           defaults to artifacts/sbom.cdx.json
#
# This is normally invoked automatically, once per architecture, from
# `Containerfile` while building the production container. It can also be
# run directly for development, e.g. on macOS: build-environment facts
# that are normally read via Alpine's `apk` (musl/sqlite/zlib versions
# used to link tippecanoe, the Alpine version itself, ...) are then not
# available, so placeholder values get inserted instead, and a warning is
# printed. Such a placeholder-filled SBOM is fine for checking structural
# or semantic validity, but MUST NOT be treated as an accurate SBOM for a
# production build; the output is marked as such in its own metadata
# (metadata.properties: "osm-diffs:sbom:devBuild").
#
# Note on the image digest: this script does not, and cannot, know the
# digest of the container image it is building -- that digest only
# exists once `podman build` has finished, which is after this script
# has already run. `.github/workflows/release.yml` patches the digest
# into the generated SBOM afterwards, with a one-line `jq` expression.
# See merge.jq for details.
#
# Requirements:
#   cargo, cargo-cyclonedx   (`cargo install cargo-cyclonedx`)
#   jq >= 1.6                (macOS: `brew install jq`  |  Alpine: `apk add jq`)

set -eu

IMAGE_NAME="alltheplaces/osm-diffs"

# ── locate project root and jq scripts ──────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
OUTPUT="${1:-${PROJECT_ROOT}/artifacts/sbom.cdx.json}"

# ── pre-flight checks ────────────────────────────────────────────────────────
command -v cargo >/dev/null 2>&1 || { echo "ERROR: cargo is not installed" >&2; exit 1; }
command -v jq    >/dev/null 2>&1 || { echo "ERROR: jq is not installed"    >&2; exit 1; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# ── detect build environment ─────────────────────────────────────────────────
# On the real build environment (Alpine Linux, used by Containerfile), `apk`
# tells us exactly which package versions went into the static tippecanoe
# binary. Anywhere else (e.g. a macOS development machine) we can't know
# those versions, so we insert placeholders and mark the SBOM as dev-only.
if command -v apk >/dev/null 2>&1; then
  DEV_BUILD="false"
  ALPINE_VERSION="$(grep "^VERSION_ID=" /etc/os-release | cut -d '=' -f 2)"
  APK_VERSION="$(apk --version | sed -E 's/.* ([0-9]+\.[0-9]+(\.[0-9]+(-r[0-9]+)?)?).*/\1/')"
  MUSL_VERSION="$(apk info musl            | head -1 | sed 's/musl-//;s/ .*//')"
  SQLITE_VERSION="$(apk info sqlite-static | head -1 | sed 's/sqlite-static-//;s/ .*//')"
  ZLIB_VERSION="$(apk info zlib-static     | head -1 | sed 's/zlib-static-//;s/ .*//')"
else
  DEV_BUILD="true"
  ALPINE_VERSION="dev-unknown"
  APK_VERSION="dev-unknown"
  MUSL_VERSION="dev-unknown"
  SQLITE_VERSION="dev-unknown"
  ZLIB_VERSION="dev-unknown"
  echo "WARNING: apk not found -- this does not look like the real Alpine" >&2
  echo "  build environment. Inserting placeholder values for the SBOM" >&2
  echo "  fields normally read via 'apk info'. The generated SBOM is" >&2
  echo "  only useful for development (e.g. checking structural or" >&2
  echo "  semantic validity); it MUST NOT be treated as accurate for a" >&2
  echo "  production build." >&2
fi

if command -v protoc >/dev/null 2>&1; then
  PROTOC_VERSION="$(protoc --version | awk '{print $2}')"   # "31.1" from "libprotoc 31.1"
else
  PROTOC_VERSION="dev-unknown"
fi

TIPPECANOE_VERSION="${TIPPECANOE_VERSION:-dev-unknown}"
BUILD_TIMESTAMP="${BUILD_TIMESTAMP:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"

# ── gather build-environment facts that are available everywhere ────────────
ARCH=$(uname -m)
case "$ARCH" in
    x86_64) ARCH="amd64" ;;
    arm64)  ARCH="aarch64" ;;
esac

AWS_LC_SYS_VERSION="$(grep -A1 'name = "aws-lc-sys"' "${PROJECT_ROOT}/Cargo.lock" | grep version | sed -n 's/.*version = "//;s/"//p')"
CARGO_CYCLONEDX_VERSION="$(cargo cyclonedx --version | cut -d ' ' -f 2)"
ID_TAGGING_SCHEMA_LICENSE="$(sed -n 's/.*released under the \(.*\) license.*/\1/p' "${PROJECT_ROOT}/src/pipeline/osm/id_tagging_schema.rs")"
ID_TAGGING_SCHEMA_PURL="$(grep pkg: "${PROJECT_ROOT}/src/pipeline/osm/id_tagging_schema.rs" | awk '{print $NF}')"
ID_TAGGING_SCHEMA_VERSION="$(echo "$ID_TAGGING_SCHEMA_PURL" | sed 's/.*@//')"
JQ_VERSION="$(jq --version | sed -n 's/jq-//p')"
OSM_TESTDATA_COMMIT="$(sed -n 's/^Commit:  *//p' "${PROJECT_ROOT}/tests/test_data/osm-testdata-grid/VENDORED.md")"
RUSTC_VERSION="$(rustc --version --verbose | awk '/^release:/{print $2}')"

if command -v uuidgen > /dev/null 2>&1; then
  SERIAL="urn:uuid:$(uuidgen | tr '[:upper:]' '[:lower:]')"
elif [ -r /proc/sys/kernel/random/uuid ]; then
  SERIAL="urn:uuid:$(cat /proc/sys/kernel/random/uuid)"
else
  echo "ERROR: cannot generate UUID -- install uuidgen (apk add util-linux)" >&2
  exit 1
fi

# ── build the osm-diffs (pipeline) SBOM fragment ─────────────────────────────
# cargo cyclonedx always writes next to Cargo.toml, named after the binary
# target; move it into our workdir once it's done so it can't be mistaken
# for a second, independently meaningful SBOM file.
RAW_PIPELINE="${PROJECT_ROOT}/osm-diffs_bin.cdx.json"
rm -f "$RAW_PIPELINE"  # clean up any leftover file from a previous failed run

cargo cyclonedx \
    --describe binaries \
    --format json \
    --manifest-path "${PROJECT_ROOT}/Cargo.toml" \
    --spec-version 1.5

if [ ! -f "$RAW_PIPELINE" ]; then
    echo "ERROR: cargo cyclonedx did not produce expected file: $RAW_PIPELINE" >&2
    exit 1
fi
mv "$RAW_PIPELINE" "${WORKDIR}/pipeline-raw.cdx.json"
RAW_PIPELINE="${WORKDIR}/pipeline-raw.cdx.json"

jq \
  --arg ALPINE_VERSION            "${ALPINE_VERSION}" \
  --arg ARCH                      "${ARCH}" \
  --arg AWS_LC_SYS_VERSION        "${AWS_LC_SYS_VERSION}" \
  --arg CARGO_CYCLONEDX_VERSION   "${CARGO_CYCLONEDX_VERSION}" \
  --arg ID_TAGGING_SCHEMA_LICENSE "${ID_TAGGING_SCHEMA_LICENSE}" \
  --arg ID_TAGGING_SCHEMA_PURL    "${ID_TAGGING_SCHEMA_PURL}" \
  --arg ID_TAGGING_SCHEMA_VERSION "${ID_TAGGING_SCHEMA_VERSION}" \
  --arg JQ_VERSION                "${JQ_VERSION}" \
  --arg OSM_TESTDATA_COMMIT       "${OSM_TESTDATA_COMMIT}" \
  --arg PROTOC_VERSION            "${PROTOC_VERSION}" \
  --arg RUSTC_VERSION             "${RUSTC_VERSION}" \
  --arg DEV_BUILD                 "${DEV_BUILD}" \
  -f "${SCRIPT_DIR}/pipeline.jq" \
  "$RAW_PIPELINE" > "${WORKDIR}/pipeline.cdx.json"

# ── build the tippecanoe SBOM fragment ───────────────────────────────────────
jq -n \
  --arg ARCH               "${ARCH}" \
  --arg TIPPECANOE_VERSION "${TIPPECANOE_VERSION}" \
  --arg ALPINE_VERSION     "${ALPINE_VERSION}" \
  --arg APK_VERSION        "${APK_VERSION}" \
  --arg JQ_VERSION         "${JQ_VERSION}" \
  --arg MUSL_VERSION       "${MUSL_VERSION}" \
  --arg SQLITE_VERSION     "${SQLITE_VERSION}" \
  --arg ZLIB_VERSION       "${ZLIB_VERSION}" \
  --arg DEV_BUILD          "${DEV_BUILD}" \
  -f "${SCRIPT_DIR}/tippecanoe.jq" \
  > "${WORKDIR}/tippecanoe.cdx.json"

# ── assemble the final, single SBOM ──────────────────────────────────────────
mkdir -p "$(dirname "$OUTPUT")"
jq -n \
  --arg serial     "$SERIAL" \
  --arg image      "$IMAGE_NAME" \
  --arg timestamp  "$BUILD_TIMESTAMP" \
  --slurpfile pipeline   "${WORKDIR}/pipeline.cdx.json" \
  --slurpfile tippecanoe "${WORKDIR}/tippecanoe.cdx.json" \
  -f "${SCRIPT_DIR}/merge.jq" \
  > "$OUTPUT"

echo "Written: $OUTPUT"
