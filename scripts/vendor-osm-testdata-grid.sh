#!/usr/bin/env sh
# scripts/vendor-osm-testdata-grid.sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Vendor the `grid/` subdirectory of https://github.com/osmcode/osm-testdata
# (public-domain OSM test fixtures) into tests/test_data/osm-testdata-grid/,
# pinned to a specific upstream commit. Re-run with a new commit SHA to pick
# up upstream updates.
#
# Usage:
#   ./scripts/vendor-osm-testdata-grid.sh <upstream-commit-sha>
set -eu
COMMIT="${1:?usage: vendor-osm-testdata-grid.sh <upstream-commit-sha>}"
DEST="tests/test_data/osm-testdata-grid"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

git clone --filter=blob:none --no-checkout https://github.com/osmcode/osm-testdata.git "$TMP"
git -C "$TMP" sparse-checkout set --no-cone grid
git -C "$TMP" checkout "$COMMIT"

rm -rf "$DEST"
mkdir -p "$DEST"
cp -R "$TMP/grid/." "$DEST/"

cat > "$DEST/VENDORED.md" <<EOF
Source:  https://github.com/osmcode/osm-testdata
Path:    grid/
Commit:  $COMMIT
Vendored on: $(date -u +%Y-%m-%d)
License: Public domain (per upstream README: "All files are released
         into the public domain.")
Used only for tests; not part of any shipped artifact.
EOF

echo "Vendored $COMMIT into $DEST"
