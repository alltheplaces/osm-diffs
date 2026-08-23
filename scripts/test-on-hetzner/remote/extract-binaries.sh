#!/usr/bin/env bash
# Extracts /app/osm-diffs and /usr/local/bin/tippecanoe out of the
# locally tagged osm-diffs-test image into /usr/local/bin -- shared by
# build.sh (after building from source) and pull.sh (after pulling an
# already-built image), so bare-mode `start` works the same either way.
set -euo pipefail

# Remove any leftover "extract" container from a previous deploy first --
# podman create fails with "name already in use" otherwise.
podman rm -f extract >/dev/null 2>&1 || true
podman create --name extract osm-diffs-test
# Both go to /usr/local/bin, not somewhere repo-relative: osm-diffs
# invokes tippecanoe via `Command::new("tippecanoe")`, a bare PATH
# lookup with no hardcoded location (src/pipeline/tiles.rs), so
# tippecanoe specifically must resolve on PATH for any branch that
# reaches render_tiles.
podman cp extract:/app/osm-diffs /usr/local/bin/osm-diffs
podman cp extract:/usr/local/bin/tippecanoe /usr/local/bin/tippecanoe
podman rm -f extract >/dev/null

chmod +x /usr/local/bin/osm-diffs /usr/local/bin/tippecanoe
/usr/local/bin/osm-diffs --version
