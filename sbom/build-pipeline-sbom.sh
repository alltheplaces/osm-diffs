#!/usr/bin/env sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Generate and post-process a CycloneDX SBOM for diffed-places-pipeline
# so it passes the FOSSA NTIA validator and contains a Cryptographic
# Bill Of Materials (CBOM).
#
# Usage:
#   ./sbom/build-pipeline-sbom.sh [OUTPUT]
#
#   OUTPUT  path to write the fixed SBOM
#           defaults to osm-diffs.cdx.json in the project root
#
# Requirements
#   cargo, cargo-cyclonedx   (`cargo install cargo-cyclonedx`)
#   jq ≥ 1.6                 (macOS: `brew install jq`  |  Alpine: `apk add jq`)

set -eu

# ── constants ────────────────────────────────────────────────────────────────
BINARY_NAME="osm-diffs"

# ── locate project root ──────────────────────────────────────────────────────
# The script lives in <project-root>/sbom/, so the project root is one level
# up from the directory containing this file.  We resolve it here so the
# script works correctly regardless of the working directory when invoked
# (e.g. `./sbom/build-pipeline-sbom.sh` from the project root, or
# `../sbom/build-pipeline-sbom.sh` from a subdirectory).
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── argument handling ────────────────────────────────────────────────────────
OUTPUT="${1:-${PROJECT_ROOT}/${BINARY_NAME}.cdx.json}"

# ── pre-flight checks ────────────────────────────────────────────────────────
command -v cargo >/dev/null 2>&1 || { echo "ERROR: cargo is not installed" >&2; exit 1; }
command -v jq    >/dev/null 2>&1 || { echo "ERROR: jq is not installed"    >&2; exit 1; }

# ── generate raw SBOM ────────────────────────────────────────────────────────
RAW_FILE="${PROJECT_ROOT}/osm-diffs_bin.cdx.json"

# Clean up any leftover file from a previous failed run.
rm -f "$RAW_FILE"

cargo cyclonedx \
    --describe binaries \
    --format json \
    --manifest-path "${PROJECT_ROOT}/Cargo.toml" \
    --spec-version 1.5

if [ ! -f "$RAW_FILE" ]; then
    echo "ERROR: cargo cyclonedx did not produce expected file: $RAW_FILE" >&2
    exit 1
fi

JQ_PROGRAM=$(cat <<'JQEOF'
# Add a crates.io supplier to a component object if it lacks one.
def add_supplier:
  if .supplier == null or .supplier == {} then
    .supplier = {"name": "crates.io", "url": ["https://crates.io"]}
  else
    .
  end;

# Patch bom-ref of main application to read "osm-diffs-1.2.3"
# instead of "path+file:///Users/sascha/src/osm-diffs#osm-diffs".
( .metadata.component.name + "-" + .metadata.component.version ) as $root_ref |
.metadata.component."bom-ref" = $root_ref |
.dependencies[0].ref = $root_ref |

# Declare the root component's dependency on our two vendored, non-crate
# "data" components, so the dependency graph has a single root instead of
# three (the FOSSA NTIA validator flags orphaned components -- ones no
# other component depends on -- as extra roots).
.dependencies[0].dependsOn =
  ((.dependencies[0].dependsOn // []) + ["id-tagging-schema", "osm-testdata-grid"]) |

.bomFormat = "CycloneDX" |
.specVersion = "1.7" |
.metadata.lifecycles = [{phase: "build"}] |
.metadata.authors = [{name: "Sascha Brawer", email: "sascha@brawer.ch"}] |
.metadata.supplier = {
  name: "All The Places",
  url: ["https://github.com/alltheplaces/"]
} |
.metadata.tools = {
  components: [{
      type: "operating-system",
      name: "Alpine Linux",
      version: $ALPINE_VERSION,
      "bom-ref": "sbom-os",
      description: "Operating system on which this SBOM was built",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      name: "cargo-cyclonedx",
      "bom-ref": "cargo-cyclonedx",
      version: $CARGO_CYCLONEDX_VERSION,
      purl: "pkg:apk/alpine/cargo-cyclonedx@" + $CARGO_CYCLONEDX_VERSION + "?arch=" + $ARCH,
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      name: "jq",
      "bom-ref": "jq",
      version: $JQ_VERSION,
      purl: "pkg:apk/alpine/jq@" + $JQ_VERSION + "?arch=" + $ARCH,
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }
  ]
} |
.metadata.component.supplier = {name: "All The Places", url: ["https://github.com/alltheplaces/"]} |
.metadata.component.purl = "pkg:github/alltheplaces/osm-diffs@" + .metadata.component.version |
.metadata.component.licenses = [{expression: "MIT"}] |
.components |= [ .[] | add_supplier ] |

# Declare that we only use TLS 1.3, with the AWS BoringSSL fork.
.metadata.component.properties += [
  {name: "cdx:cbom:version",      value: "1.0"},
  {name: "crypto:tls:library",    value: "rustls"},
  {name: "crypto:tls:backend",    value: "aws-lc-rs"},
  {name: "crypto:tls:minVersion", value: "1.3"},
  {name: "crypto:tls:maxVersion", value: "1.3"}
] |
.formulation = [{
    "bom-ref": "build-formulation",
    components: [{
      type: "operating-system",
      name: "Alpine Linux",
      version: $ALPINE_VERSION,
      "bom-ref": "build-os",
      description: "Operating system for building binaries",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }, {
      type: "application",
      "bom-ref": "build-rustc",
      name: "rustc",
      version: $RUSTC_VERSION,
      purl: "pkg:generic/rust-lang/rustc@" + $RUSTC_VERSION,
      description: "Rust compiler",
      supplier: {
        name: "The Rust Project",
        url: ["https://www.rust-lang.org"]
      }
    }, {
      type: "application",
      "bom-ref": "build-protoc",
      name: "protoc",
      version: $PROTOC_VERSION,
      purl: "pkg:apk/alpine/protoc@" + $PROTOC_VERSION + "?arch=" + $ARCH,
      description: "Protocol Buffers compiler",
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }]
}] |
.components += [
  {
    "type": "data",
    "bom-ref": "id-tagging-schema",
    "name": "id-tagging-schema",
    "description": "OpenStreetMap tagging schema",
    "version": $ID_TAGGING_SCHEMA_VERSION,
    "purl": $ID_TAGGING_SCHEMA_PURL,
    "licenses": [{"license": {"id": $ID_TAGGING_SCHEMA_LICENSE}}],
    "externalReferences": [{
        "type": "vcs",
        "url": "https://github.com/openstreetmap/id-tagging-schema"
    }],
    "manufacturer": {
      "name": "iD Tagging Schema project (OpenStreetMap)",
      "url": [ "https://github.com/openstreetmap/id-tagging-schema" ]
    },
    "supplier": {
      "name": "iD Tagging Schema project (OpenStreetMap)",
      "url": [ "https://github.com/openstreetmap/id-tagging-schema" ]
    }
  },
  {
    "type": "data",
    "bom-ref": "osm-testdata-grid",
    "name": "osm-testdata-grid",
    "description": "OSM grid test fixtures, vendored for unit/integration tests only",
    "version": $OSM_TESTDATA_COMMIT,
    "scope": "excluded",
    "purl": "pkg:github/osmcode/osm-testdata@" + $OSM_TESTDATA_COMMIT + "#grid/data",
    "licenses": [{
      "license": {
        "name": "Public Domain",
        "acknowledgement": "declared"
      }
    }],
    "externalReferences": [{
      "type": "vcs",
      "url": "https://github.com/osmcode/osm-testdata/tree/" + $OSM_TESTDATA_COMMIT + "/grid/data"
    }],
    "manufacturer": {
      "name": "osmcode / Jochen Topf",
      "url": ["https://github.com/osmcode/osm-testdata"]
    },
    "supplier": {
      "name": "osmcode / Jochen Topf",
      "url": ["https://github.com/osmcode/osm-testdata"]
    }
  },
  {
    "type": "library",
    "bom-ref": "pkg/aws-lc",
    "name": "aws-lc",
    "author": "AWS Cryptography",
    "purl": "pkg:github/aws/aws-lc@v" + $AWS_LC_SYS_VERSION,
    "version": $AWS_LC_SYS_VERSION,
    "description":"AWS fork of BoringSSL; native crypto primitives beneath aws-lc-rs",
    "supplier": {
      "name": "AWS Cryptography",
      "url": ["https://github.com/aws/aws-lc"]
     },
     "licenses": [{"expression": "Apache-2.0 OR ISC" }],
     "externalReferences":[{
       "type": "certification-report",
       "url": "https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/4816",
       "comment": "FIPS 140-3 Level 1 — CMVP certificate #4816"
     }]
  },
  {
    "type": "cryptographic-asset",
    "bom-ref": "crypto/protocol/tls-1.3",
    "name": "TLS",
    "version": "1.3",
    "cpe": "cpe:2.3:a:ietf:tls:1.3:*:*:*:*:*:*:*",
    "supplier": {
      "name": "IETF",
      "url": ["https://www.rfc-editor.org/rfc/rfc8446"]
    },
    "cryptoProperties": {
      "assetType": "protocol",
      "protocolProperties": {
        "type": "tls",
        "version": "1.3",
        "cipherSuites": [
          {"name":"TLS_AES_256_GCM_SHA384"},
          { "name": "TLS_AES_128_GCM_SHA256" },
          { "name": "TLS_CHACHA20_POLY1305_SHA256" }
        ]
      }
    }
  }
] |
.dependencies += [
  {
    ref: (.components[] | select(.name == "aws-lc-sys") | ."bom-ref"),
    dependsOn: ["pkg/aws-lc"]
  }
] |

(.components[] | select(.name == "rustls") | ."bom-ref") as $rustls_ref |
(.dependencies[] | select(.ref == $rustls_ref)).dependsOn += ["crypto/protocol/tls-1.3"]

JQEOF
)

ARCH=$(uname -m)
case "$ARCH" in
    x86_64) ARCH="amd64" ;;
    arm64)  ARCH="aarch64" ;;
esac

ALPINE_VERSION="$(grep "^VERSION_ID=" /etc/os-release | cut -d '=' -f 2)"
APK_VERSION="$(apk --version | sed -E 's/.* ([0-9]+\.[0-9]+(\.[0-9]+(-r[0-9]+)?)?).*/\1/')"
AWS_LC_SYS_VERSION="$(grep -A1 'name = "aws-lc-sys"' Cargo.lock | grep version | sed -n 's/.*version = "//;s/"//p')"
CARGO_CYCLONEDX_VERSION="$(cargo cyclonedx --version | cut -d ' ' -f 2)"
ID_TAGGING_SCHEMA_LICENSE="$(sed -n 's/.*released under the \(.*\) license.*/\1/p' src/pipeline/osm/id_tagging_schema.rs)"
ID_TAGGING_SCHEMA_PURL="$(grep pkg: src/pipeline/osm/id_tagging_schema.rs | awk '{print $NF}')"
ID_TAGGING_SCHEMA_VERSION="$(echo $ID_TAGGING_SCHEMA_PURL | sed 's/.*@//')"
JQ_VERSION="$(jq --version | sed -n 's/jq-//p')"
OSM_TESTDATA_COMMIT="$(sed -n 's/^Commit:  *//p' "${PROJECT_ROOT}/tests/test_data/osm-testdata-grid/VENDORED.md")"
PROTOC_VERSION="$(protoc --version | awk '{print $2}')"   # "31.1" from "libprotoc 31.1"
RUSTC_VERSION="$(rustc --version --verbose | awk '/^release:/{print $2}')"

# ── post-process and write output ────────────────────────────────────────────
jq \
  --arg binary                    "${BINARY_NAME}" \
  --arg ALPINE_VERSION            "${ALPINE_VERSION}" \
  --arg ARCH                      "${ARCH}" \
  --arg AWS_LC_SYS_VERSION        "${AWS_LC_SYS_VERSION}" \
  --arg CARGO_CYCLONEDX_VERSION   "${CARGO_CYCLONEDX_VERSION}" \
  --arg ID_TAGGING_SCHEMA_LICENSE "${ID_TAGGING_SCHEMA_LICENSE}" \
  --arg ID_TAGGING_SCHEMA_PURL    "${ID_TAGGING_SCHEMA_PURL}" \
  --arg ID_TAGGING_SCHEMA_VERSION "${ID_TAGGING_SCHEMA_LICENSE}" \
  --arg JQ_VERSION                "${JQ_VERSION}" \
  --arg OSM_TESTDATA_COMMIT       "${OSM_TESTDATA_COMMIT}" \
  --arg PROTOC_VERSION            "${PROTOC_VERSION}" \
  --arg RUSTC_VERSION             "${RUSTC_VERSION}" \
  "$JQ_PROGRAM" \
  "$RAW_FILE" > "$OUTPUT"

rm -f "$RAW_FILE"

echo "Written: $OUTPUT"
