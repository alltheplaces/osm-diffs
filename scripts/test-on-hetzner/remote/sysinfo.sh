#!/usr/bin/env bash
# Collects environment facts relevant to interpreting pipeline results --
# not anything this tool controls, but things that turned out to matter
# during the PR 665 cloud experiment (#667): VMs landing on different
# kernel patch levels, a replacement VM coming up on Ubuntu where Debian
# was intended (only caught by comparing `uname -a` across machines),
# and /sys/fs/cgroup/memory.current not existing at all outside a
# container (so the cgroup_* fields in pipeline.log's memstats read
# None -- worth knowing going in, not discovering by surprise).
#
# Not `set -e`: an individual command failing (e.g. no cgroup files
# present) shouldn't stop the rest from being collected.
set -uo pipefail

echo "=== date ==="
date -u
echo "=== uname -a ==="
uname -a
echo "=== /etc/os-release ==="
cat /etc/os-release
echo "=== nproc ==="
nproc
echo "=== /proc/cpuinfo model name ==="
grep "model name" /proc/cpuinfo | head -1
echo "=== free -h ==="
free -h
echo "=== swapon --show ==="
swapon --show
echo "=== lsblk ==="
lsblk
echo "=== df -h ==="
df -h
echo "=== cgroup memory ==="
cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.max 2>&1
echo "=== Hetzner instance metadata ==="
# Specific keys only, not the whole metadata document: that also
# includes a huge base64-encoded cloud-init vendor_data blob, pure
# noise for this purpose.
for key in instance-id hostname region availability-zone; do
    printf '%s: ' "$key"
    curl -s --max-time 5 "http://169.254.169.254/hetzner/v1/metadata/$key"
    echo
done
