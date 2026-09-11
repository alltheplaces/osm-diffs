#!/usr/bin/env bash
# Extracts /app/osmdiffs, /usr/local/bin/tippecanoe, and
# /usr/local/bin/tile-join out of the locally tagged osmdiffs-test
# image into /usr/local/bin -- shared by build.sh (after building from
# source) and pull.sh (after pulling an already-built image), so
# bare-mode `start` works the same either way.
set -euo pipefail

# Remove any leftover "extract" container from a previous deploy first --
# podman create fails with "name already in use" otherwise.
podman rm -f extract >/dev/null 2>&1 || true
podman create --name extract osmdiffs-test
# All three go to /usr/local/bin, not somewhere repo-relative: osmdiffs
# invokes tippecanoe and tile-join via `Command::new(...)`, a bare PATH
# lookup with no hardcoded location (src/pipeline/tiles.rs), so both
# specifically must resolve on PATH for any branch that reaches
# render_tiles/join_tiles.
podman cp extract:/app/osmdiffs /usr/local/bin/osmdiffs
podman cp extract:/usr/local/bin/tippecanoe /usr/local/bin/tippecanoe
podman cp extract:/usr/local/bin/tile-join /usr/local/bin/tile-join
podman rm -f extract >/dev/null

chmod +x /usr/local/bin/osmdiffs /usr/local/bin/tippecanoe /usr/local/bin/tile-join
/usr/local/bin/osmdiffs --version
