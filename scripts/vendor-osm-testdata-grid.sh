#!/usr/bin/env sh
# scripts/vendor-osm-testdata-grid.sh
#
# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT
#
# Vendor the `grid/data/` subdirectory of
# https://github.com/osmcode/osm-testdata (public-domain OSM test fixtures)
# into tests/test_data/osm-testdata-grid/data/, pinned to a specific
# upstream commit. Re-run with a new commit SHA to pick up upstream
# updates.
#
# Only grid/data/ is vendored -- the actual test fixtures -- not the rest
# of grid/ (bin/ scripts, Makefile, grid.db, README.md, ...), which we
# don't need.
#
# Usage:
#   ./scripts/vendor-osm-testdata-grid.sh <upstream-commit-sha>
set -eu
COMMIT="${1:?usage: vendor-osm-testdata-grid.sh <upstream-commit-sha>}"
DEST="tests/test_data/osm-testdata-grid"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

git clone --filter=blob:none --no-checkout https://github.com/osmcode/osm-testdata.git "$TMP"
git -C "$TMP" sparse-checkout set --no-cone grid/data
git -C "$TMP" checkout "$COMMIT"

rm -rf "$DEST"
mkdir -p "$DEST/data"
cp -R "$TMP/grid/data/." "$DEST/data/"

cat > "$DEST/VENDORED.md" <<EOF
Source:  https://github.com/osmcode/osm-testdata
Path:    grid/data/
Commit:  $COMMIT
Vendored on: $(date -u +%Y-%m-%d)
License: Public domain (per upstream README: "All files are released
         into the public domain.")
Used only for tests; not part of any shipped artifact.
EOF

echo "Vendored $COMMIT into $DEST"
