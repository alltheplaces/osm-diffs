# Builds a local OCI container, extracts the binaries into ./osm-diffs and ./tippecanoe.

apt-get install -y podman
podman build -t osm-diffs-test --build-arg BUILD_TIMESTAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ") -f Containerfile .
podman create --name extract osm-diffs-test
podman cp extract:/app/osm-diffs ./osm-diffs
podman cp extract:/usr/local/bin/tippecanoe ./tippecanoe
