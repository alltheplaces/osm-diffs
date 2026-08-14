#!/usr/bin/env bash
# Builds a local OCI container, extracts the binaries into ./osm-diffs and ./tippecanoe.
# Assumes a Debian/Ubuntu host (apt-get). Run from the root of the project tree.
set -euo pipefail

apt-get install -y podman
podman build -t osm-diffs-test --build-arg BUILD_TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ") -f Containerfile .

# Remove any leftover "extract" container from a previous run first --
# podman create fails with "name already in use" otherwise, so without
# this a second run of this script would fail immediately.
podman rm -f extract >/dev/null 2>&1 || true
podman create --name extract osm-diffs-test
podman cp extract:/app/osm-diffs ./osm-diffs
podman cp extract:/usr/local/bin/tippecanoe ./tippecanoe
podman rm -f extract >/dev/null
