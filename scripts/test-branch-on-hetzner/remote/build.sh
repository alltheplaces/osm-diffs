#!/usr/bin/env bash
# Pushed to the remote VM and run there by `cloud_test.py deploy`. Clones
# (or updates) the given branch and builds osm-diffs + tippecanoe via the
# project's own Containerfile -- natively, no QEMU emulation. Building
# this same Containerfile locally on an Apple Silicon dev machine forces
# --platform linux/amd64, which is slow enough (~2 hours, including one
# OOM-kill from the C++ compiler under emulation) that it's worth always
# building directly on the target x86_64 VM instead.
set -euo pipefail

repo="$1"
branch="$2"
dir="$3"

# Neither is preinstalled on the base Debian cloud image.
apt-get update -qq
apt-get install -y -qq git podman >/dev/null

if [ ! -d "$dir/.git" ]; then
    git clone --branch "$branch" "$repo" "$dir"
else
    git -C "$dir" fetch origin "$branch"
    git -C "$dir" checkout "$branch"
    git -C "$dir" reset --hard "origin/$branch"
fi

cd "$dir"
podman build -t osm-diffs-test \
    --build-arg BUILD_TIMESTAMP="$(date -u +"%Y-%m-%dT%H:%M:%SZ")" \
    -f Containerfile .

"$(dirname "$0")/extract-binaries.sh"
