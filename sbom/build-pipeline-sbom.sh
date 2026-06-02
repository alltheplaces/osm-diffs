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


# ── jq program ───────────────────────────────────────────────────────────────
#
# Written as a series of named steps via `as` bindings so each transformation
# is easy to follow and test independently.
#
JQ_PROGRAM=$(cat <<'JQEOF'
# ── helpers ──────────────────────────────────────────────────────────────────

# Add a crates.io supplier to a component object if it lacks one.
def add_supplier:
  if .supplier == null or .supplier == {} then
    .supplier = {"name": "crates.io", "url": ["https://crates.io"]}
  else
    .
  end;

# ── step 1: identify the single root ─────────────────────────────────────────
# cargo-cyclonedx stores the primary component in .metadata.component.
# Its bom-ref is what we want as the sole dependency graph root.
# If for some reason it is absent, fall back to the first component whose
# name matches the binary name.
(
  if .metadata.component != null then
    .metadata.component."bom-ref"
  else
    (.components[] | select(.name == $binary) | ."bom-ref") // null
  end
) as $root_ref |

# ── step 2: collect bom-refs that are "extra roots" ──────────────────────────
# Extra roots are .dependencies[] entries whose ref is NOT $root_ref AND
# that are not already a dependee of any other component — i.e. they appear
# as a top-level key in .dependencies but not inside any .dependsOn list.
([ .dependencies[].ref ] | sort | unique) as $all_dep_refs |
([ .dependencies[].dependsOn[]? ] | sort | unique) as $all_dependees |
(
  [ $all_dep_refs[] |
    select(. != $root_ref) |
    select(. as $r | $all_dependees | contains([$r]) | not)
  ]
) as $extra_roots |

# ── step 3: remove extra-root components from .components[] ──────────────────
# cargo-cyclonedx emits one component per Cargo target (bin, lib, example…).
# Extra roots are workspace members, not third-party crates.  They don't
# belong in the FOSSA dependency list.
.components |= [
  .[] | select(."bom-ref" as $r | ($extra_roots | contains([$r])) | not)
] |

# ── step 4: rebuild .dependencies so there is exactly one root ───────────────
# a. Remove entries for the now-deleted extra-root components.
# b. Merge their dependsOn lists into the real root's dependsOn.
# c. Remove any dependsOn references to the extra roots themselves.
([ .dependencies[] |
   select(.ref as $r | ($extra_roots | contains([$r])) | not) |
   .dependsOn = [ .dependsOn[]? |
                  select(. as $d | ($extra_roots | contains([$d])) | not) ]
] ) as $pruned_deps |

( [ $pruned_deps[] |
    select(.ref != $root_ref) |
    .dependsOn[]?
  ] | sort | unique
) as $extra_root_children |

# Build the final dependencies array
.dependencies = [
  $pruned_deps[] |
  if .ref == $root_ref then
    # Merge former extra-root children into this node's dependsOn (deduplicated)
    .dependsOn = ([ .dependsOn[]?, $extra_root_children[] ] | sort | unique)
  else
    .
  end
] |

# ── step 5: add supplier to every component missing one ──────────────────────
.metadata.supplier = {"name": "Diffed Places", "url": "https://github.com/diffed-places/"} |
.metadata.component.supplier = {"name": "Diffed Places", "url": "https://github.com/diffed-places/"} |
.metadata.component.purl = "pkg:github/diffed-places/pipeline@" + .metadata.component.version |
.metadata.component.licenses = [{expression: "MIT"}] |
.components |= [ .[] | add_supplier ] |

# ── step 6: declare that we only use TLS 1.3, with the AWS BoringSSL fork ────
.specVersion = "1.6" |
.metadata.component.properties += [
  {"name": "cdx:cbom:version",      "value": "1.0"},
  {"name": "crypto:tls:library",    "value": "rustls"},
  {"name": "crypto:tls:backend",    "value": "aws-lc-rs"},
  {"name": "crypto:tls:minVersion", "value": "1.3"},
  {"name": "crypto:tls:maxVersion", "value": "1.3"}
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
  "$JQ_PROGRAM" \
  "$RAW_FILE" > "$OUTPUT"

rm -f "$RAW_FILE"

echo "Written: $OUTPUT"
