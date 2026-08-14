#!/usr/bin/env bash
# Downloads logs from the test machines (three Debian VMs in Hetzner's Helsinki datacenter)
# to our local workstation. Run this from the root of the project tree.
#
# Deliberately no `set -e`: one unreachable/not-yet-started machine
# shouldn't abort downloading logs from the other two.

MACHINE_1=65.21.53.183
MACHINE_2=46.62.202.62
MACHINE_3=62.238.98.183

for i in 1 2 3; do
    MACHINE=$(eval echo \$MACHINE_$i)
    DEST="scripts/experiments/pr-665/logs/machine-$i"
    mkdir -p "$DEST"

    PIPELINE_LOG=`eval echo root@${MACHINE}:/mnt/*/workdir/pipeline.log`
    VMSTAT_LOG=`eval echo root@${MACHINE}:/root/osm-diffs/vmstat.log`
    DISK_LOG=`eval echo root@${MACHINE}:/root/osm-diffs/disk.log`

    scp "${VMSTAT_LOG}" "${DEST}/vmstat.log"
    scp "${DISK_LOG}" "${DEST}/disk.log"
    scp "${PIPELINE_LOG}" "${DEST}/pipeline.log"
done
