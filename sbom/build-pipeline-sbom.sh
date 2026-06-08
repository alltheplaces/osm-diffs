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
#           defaults to diffed-places-pipeline.cdx.json in the project root
#
# Requirements
#   cargo, cargo-cyclonedx   (`cargo install cargo-cyclonedx`)
#   jq ≥ 1.6                 (macOS: `brew install jq`  |  Alpine: `apk add jq`)

set -eu

# ── constants ────────────────────────────────────────────────────────────────
BINARY_NAME="diffed-places-pipeline"

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
RAW_FILE="${PROJECT_ROOT}/diffed-places-pipeline_bin.cdx.json"

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

# Patch bom-ref of main application to read "diffed-places-pipeline-1.2.3"
# instead of "path+file:///Users/sascha/src/pipeline#diffed-places-pipeline".
( .metadata.component.name + "-" + .metadata.component.version ) as $root_ref |
.metadata.component."bom-ref" = $root_ref |
.dependencies[0].ref = $root_ref |

.bomFormat = "CycloneDX" |
.specVersion = "1.7" |
.metadata.lifecycles = [{phase: "build"}] |
.metadata.authors = [{name: "Sascha Brawer", email: "sascha@brawer.ch"}] |
.metadata.supplier = {name: "Diffed Places", url: "https://github.com/diffed-places/"} |
.metadata.tools = {
  "components": .metadata.tools + [{
      name: "jq",
      version: $jq_version,
      supplier: {
        name: "Alpine Linux",
        url: ["https://alpinelinux.org"]
      }
    }
  ]
} |
.metadata.component.supplier = {name: "Diffed Places", url: "https://github.com/diffed-places/"} |
.metadata.component.purl = "pkg:github/diffed-places/pipeline@" + .metadata.component.version |
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
.components += [
  {
    "type": "library",
    "bom-ref": "pkg/aws-lc",
    "name": "aws-lc",
    "author": "AWS Cryptography",
    "purl": "pkg:github/aws/aws-lc@v" + $aws_lc_sys_version,
    "version": $aws_lc_sys_version,
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

# ── post-process and write output ────────────────────────────────────────────
jq \
  --arg     binary             "$BINARY_NAME" \
  --arg     aws_lc_sys_version "$(grep -A1 'name = "aws-lc-sys"' Cargo.lock | grep version | sed -n 's/.*version = "//;s/"//p')" \
  --arg     jq_version         "$(jq --version | sed -n 's/jq-//p')" \
  "$JQ_PROGRAM" \
  "$RAW_FILE" > "$OUTPUT"

rm -f "$RAW_FILE"

echo "Written: $OUTPUT"
