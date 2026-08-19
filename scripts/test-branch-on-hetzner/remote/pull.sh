#!/usr/bin/env bash
# Pushed to the remote VM and run there by `cloud_test.py deploy --image`.
# Pulls an already-built image -- e.g. a released osm-diffs container
# from ghcr.io -- instead of building one from source, then extracts
# binaries the same way build.sh does after a local build, via the
# shared extract-binaries.sh, so bare-mode `start` works unchanged
# regardless of whether the image was built here or pulled.
set -euo pipefail

image="$1"

# Not preinstalled on the base Debian cloud image.
apt-get update -qq
apt-get install -y -qq podman >/dev/null

podman pull "$image"
# extract-binaries.sh (and containerized `start`) both expect this fixed
# local name, regardless of what registry/tag the image actually came
# from -- same convention build.sh already uses for a locally built image.
podman tag "$image" osm-diffs-test

"$(dirname "$0")/extract-binaries.sh"
