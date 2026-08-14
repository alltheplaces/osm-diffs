# Downloads logs from the test machines (three Debian VMs in Hetzner's Helsinki datacenter)
# to our local workstation. Run this from the root of the project tree.

MACHINE_1=65.21.53.183
MACHINE_2=46.62.202.62
MACHINE_3=62.238.98.183

mkdir -p scripts/experiments/pr-665/logs

for i in 1 2 3; do
    MACHINE=$(eval echo \$MACHINE_$i)

    PIPELINE_LOG=`eval echo root@${MACHINE}:/mnt/*/workdir/pipeline.log`
    VMSTAT_LOG=`eval echo root@${MACHINE}:/root/osm-diffs/vmstat.log`
    DEST=`eval echo logs/machine-\$MACHINE_$i`
    
    scp "${VMSTAT_LOG}" "${DEST}/vmstat.log"
    scp "${PIPELINE_LOG}" "${DEST}/pipeline.log"
done
