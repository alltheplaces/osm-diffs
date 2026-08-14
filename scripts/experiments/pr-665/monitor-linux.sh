#!/usr/bin/env bash
# Runs vmstat and a df-sampling loop side by side, both UTC-timestamped so
# they line up with pipeline.log. Meant to be identical across all test
# VMs -- deploy this one file, run it once per machine inside its own
# screen session (so it survives disconnects independently of the
# pipeline run itself):
#
#   screen -S monitor
#   WORKDIR=/mnt/HC_Volume_.../workdir ./monitor.sh
#   <Ctrl-A D>
#
# Writes vmstat.log and disk.log into the current directory (not
# $WORKDIR itself, to avoid the monitoring logs' own tiny writes
# perturbing the disk numbers being measured).
set -euo pipefail

: "${WORKDIR:?Set WORKDIR before running this script}"

TZ=UTC vmstat -t 5 > vmstat.log &
vmstat_pid=$!
trap 'kill "$vmstat_pid" 2>/dev/null' EXIT

# df, not du: du would recursively walk the whole workdir on every
# sample, which is expensive and would itself compete for the disk I/O
# being measured. df just reads filesystem-level accounting, effectively
# free -- correct here since $WORKDIR lives on its own dedicated volume,
# so filesystem-used == workdir-used.
while true; do
    printf '%s ' "$(date -u +"%Y-%m-%d %H:%M:%S")"
    df -B1 --output=used,avail "$WORKDIR" | tail -1
    sleep 5
done > disk.log
