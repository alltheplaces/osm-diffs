#!/usr/bin/env bash
# Monitors vm_stat on macOS, appending a log entry every 5 seconds.

# Resolved fresh every iteration, not once at the top: a one-shot PID
# goes stale as soon as osm-diffs isn't running yet when this script
# starts (or restarts), and every subsequent `ps -p` call then just
# fails with "Invalid process id" for the rest of the run -- which is
# exactly what happened to an earlier overnight log.
( while true; do
    printf '=== %s UTC ===\n' "$(date -u +"%Y-%m-%d %H:%M:%S")"
    pid=$(pgrep -f 'osm-diffs run' | head -1)
    if [ -n "$pid" ]; then
        ps -o pid,%cpu,%mem,rss,vsz -p "$pid"
    else
        echo "osm-diffs run: not currently running"
    fi
    vm_stat
    sleep 5
  done ) > macos_monitor.log 2>&1 &
disown
