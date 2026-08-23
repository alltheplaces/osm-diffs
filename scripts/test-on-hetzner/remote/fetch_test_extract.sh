#!/usr/bin/env bash
# Pushed to the remote VM and run there by `cloud_test.py start
# --containerized --regional-extract`. Downloads a Geofabrik regional
# extract straight to the workdir's planet-latest.osm.pbf, skipping the
# multi-hour BitTorrent download entirely -- import_osm's fetch_planet()
# computes its own .meta.json sidecar for whatever valid .pbf shows up
# (src/pipeline/osm/fetch.rs), so no sidecar needs to be faked here.
set -euo pipefail

region="$1"
workdir="$2"

curl -fsSL "https://download.geofabrik.de/${region}-latest.osm.pbf" \
    -o "${workdir}/planet-latest.osm.pbf"
