#!/usr/bin/env sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Generate and post-process a CycloneDX SBOM for diffed-places-pipeline
# so it passes the FOSSA NTIA validator.
#
# Steps:
#   1. Runs `cargo cyclonedx` to produce a raw SBOM.
#   2. Fixes multiple roots: removes all non-binary workspace-member components
#      from .components[] and collapses the .dependencies[] roots so that only
#      the named binary is a root node; former extra-roots' children are merged
#      into that root's dependsOn.
#   3. Fixes missing supplier: every dependency component lacking a supplier
#      gets { "name": "crates.io", "url": ["https://crates.io"] }.
#      The root application component always gets ROOT_SUPPLIER_NAME / _URL.
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

# Supplier injected into the root application component (metadata.component).
# Edit these two values to match your organisation.
ROOT_SUPPLIER_NAME="Diffed Places"
ROOT_SUPPLIER_URL="https://github.com/diffed-places/"

# Supplier applied to every third-party crate that is missing one.
CRATES_IO_SUPPLIER='{"name":"crates.io","url":["https://crates.io"]}'

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
    .supplier = $cratesio
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
# metadata (the root) always gets the project supplier, overwriting whatever
# cargo-cyclonedx may have left blank.
.metadata.supplier = $rootsupplier |

# all entries in .components[] get the crates.io supplier if absent.
.components |= [ .[] | add_supplier ]

JQEOF
)

# ── post-process and write output ────────────────────────────────────────────
jq \
  --argjson cratesio     "$CRATES_IO_SUPPLIER" \
  --argjson rootsupplier "{\"name\":\"$ROOT_SUPPLIER_NAME\",\"url\":[\"$ROOT_SUPPLIER_URL\"]}" \
  --arg     binary       "$BINARY_NAME" \
  "$JQ_PROGRAM" \
  "$RAW_FILE" > "$OUTPUT"

rm -f "$RAW_FILE"

echo "Written: $OUTPUT"
