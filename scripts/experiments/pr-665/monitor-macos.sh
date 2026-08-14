# Monitors vm_stat on macOS, appending a log entry very 5 seconds.

PID=$(pgrep -f 'osm-diffs run')
( while true; do
    printf '=== %s UTC ===\n' "$(date -u +"%Y-%m-%d %H:%M:%S")"
    ps -o pid,%cpu,%mem,rss,vsz -p "$PID"
    vm_stat
    sleep 5
  done ) > macos_monitor.log 2>&1 &
disown
