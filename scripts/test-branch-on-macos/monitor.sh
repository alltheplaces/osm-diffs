#!/usr/bin/env bash
# Monitors vm_stat and osm-diffs's own RSS, appending a log entry every
# 5 seconds, UTC-timestamped to match pipeline.log. Started/stopped by
# test_macos.py, not meant to be run standalone (though nothing stops
# you from doing that too).
#
# Resolves the PID fresh every iteration, not once at the top: a
# one-shot PID goes stale as soon as osm-diffs isn't running yet when
# this starts (or has restarted since) -- every subsequent `ps -p` call
# would then just fail with "Invalid process id" for the rest of the
# run, exactly what happened to an earlier ad hoc version of this
# script during the PR 665 experiment.
set -uo pipefail

while true; do
    printf '=== %s UTC ===\n' "$(date -u +"%Y-%m-%d %H:%M:%S")"
    pid=$(pgrep -f 'osm-diffs run' | head -1)
    if [ -n "$pid" ]; then
        ps -o pid,%cpu,%mem,rss,vsz -p "$pid"
    else
        echo "osm-diffs run: not currently running"
    fi
    vm_stat
    sleep 5
done
