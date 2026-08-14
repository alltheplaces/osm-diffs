#!/usr/bin/env python3
"""Spins up a Hetzner Cloud VM, builds osm-diffs from a given git branch,
and runs the full pipeline against it -- so testing a development branch
on real cloud hardware doesn't mean re-deriving the VM setup dance by
hand each time. See README.md in this directory for the full story,
including prerequisites and a worked example.

Not tested end to end against the real Hetzner API from the environment
this was written in (no credentials there) -- treat the first real
`up` as a shakedown of the exact `hcloud` flags, not a guarantee. Every
`hcloud`/`ssh`/`scp` command this runs is echoed to stderr first, so a
flag mismatch against your installed `hcloud` version should be easy to
spot and fix.
"""

import argparse
import json
import shlex
import subprocess
import sys
import time
from pathlib import Path

DEFAULT_IMAGE = "debian-13"
DEFAULT_LOCATION = "hel1"
DEFAULT_TYPE = "cpx32"
DEFAULT_VOLUME_SIZE = 400
DEFAULT_REPO = "https://github.com/alltheplaces/osm-diffs.git"
LABEL_KEY = "osm-diffs-test"
REMOTE_DIR = "/root/osm-diffs"
SCRIPT_DIR = Path(__file__).resolve().parent
LOGS_DIR = SCRIPT_DIR / "logs"

PLANET_FILES = ("planet-latest.osm.pbf", "planet-latest.osm.pbf.meta.json")


def sh(cmd, **kwargs):
    """Runs a command, echoing it first -- so failures (and the exact
    command that caused them) are easy to see and debug without having
    to reproduce them by hand."""
    print(f"+ {' '.join(shlex.quote(c) for c in cmd)}", file=sys.stderr)
    return subprocess.run(cmd, check=True, **kwargs)


def hcloud_json(args):
    cmd = ["hcloud", *args, "-o", "json"]
    print(f"+ {' '.join(shlex.quote(c) for c in cmd)}", file=sys.stderr)
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        print(result.stderr, file=sys.stderr, end="")
        raise subprocess.CalledProcessError(result.returncode, cmd, result.stdout, result.stderr)
    return json.loads(result.stdout)


# Hetzner reuses IP addresses across separate VM lifecycles -- a freshly
# created server can easily get an IP that `known_hosts` already has an
# entry for, from some earlier (now-destroyed) instance, with a
# different host key. `StrictHostKeyChecking=accept-new` only accepts
# genuinely *new* hosts; it still correctly (and, here, unhelpfully)
# refuses a *changed* key for an IP already in known_hosts, which just
# hangs until wait_for_ssh's timeout gives up. Since every host this
# script talks to is one it just created via an authenticated Hetzner
# API call moments earlier -- not an arbitrary untrusted address -- we
# skip host key verification entirely rather than repeatedly hitting
# this across the tool's whole reason for existing (create/destroy VMs
# on repeat).
SSH_OPTS = ["-o", "UserKnownHostsFile=/dev/null", "-o", "StrictHostKeyChecking=no"]


def ssh_cmd(ip, remote_cmd, **kwargs):
    return sh(["ssh", *SSH_OPTS, f"root@{ip}", remote_cmd], **kwargs)


def scp_to(ip, local, remote):
    sh(["scp", *SSH_OPTS, str(local), f"root@{ip}:{remote}"])


def scp_from(ip, remote, local):
    sh(["scp", *SSH_OPTS, f"root@{ip}:{remote}", str(local)])


def wait_for_ssh(ip, timeout=180):
    print(f"waiting for SSH on {ip} ...", file=sys.stderr)
    deadline = time.time() + timeout
    while time.time() < deadline:
        result = subprocess.run(
            ["ssh", *SSH_OPTS, "-o", "ConnectTimeout=5", f"root@{ip}", "true"],
            capture_output=True,
        )
        if result.returncode == 0:
            print("SSH is up.", file=sys.stderr)
            return
        time.sleep(5)
    raise TimeoutError(f"SSH on {ip} did not come up within {timeout}s")


def server_ip(name):
    info = hcloud_json(["server", "describe", name])
    return info["public_net"]["ipv4"]["ip"]


def workdir_for(name):
    """Derived, not stored: re-queries the volume each time rather than
    keeping a separate state file, so there's nothing that can drift out
    of sync with what Hetzner actually has."""
    vol = hcloud_json(["volume", "describe", f"{name}-data"])
    return f"/mnt/HC_Volume_{vol['id']}/workdir"


def label_flags(name, branch=None):
    labels = [f"{LABEL_KEY}=true", f"{LABEL_KEY}-owner={name}"]
    if branch:
        # Hetzner labels don't allow "/", which branch names often have.
        labels.append(f"{LABEL_KEY}-branch={branch.replace('/', '--')}")
    flags = []
    for label in labels:
        flags += ["--label", label]
    return flags


def cmd_create(args):
    print(f"Creating server {args.name} ({args.type}, {args.location}) ...", file=sys.stderr)
    sh(
        [
            "hcloud",
            "server",
            "create",
            "--name",
            args.name,
            "--type",
            args.type,
            "--image",
            DEFAULT_IMAGE,
            "--location",
            args.location,
            "--ssh-key",
            args.ssh_key,
            *label_flags(args.name, args.branch),
        ]
    )

    ip = server_ip(args.name)
    print(f"Server {args.name} is up at {ip}", file=sys.stderr)

    volume_name = f"{args.name}-data"
    print(
        f"Creating {args.volume_size}GB volume {volume_name}, "
        "attaching + formatting ...",
        file=sys.stderr,
    )
    hcloud_json(
        [
            "volume",
            "create",
            "--name",
            volume_name,
            "--size",
            str(args.volume_size),
            # --server and --location are mutually exclusive: attaching
            # to a server already implies the volume's location.
            "--server",
            args.name,
            "--format",
            "ext4",
            "--automount",
            *label_flags(args.name, args.branch),
        ]
    )
    workdir = workdir_for(args.name)

    wait_for_ssh(ip)
    # --automount wires up /etc/fstab for the *next* boot; on the
    # already-running server it just created, give it a nudge now.
    ssh_cmd(ip, "mount -a")
    ssh_cmd(ip, f"mkdir -p {shlex.quote(workdir)}")

    run_sysinfo(ip)
    run_fio(ip, workdir)

    print(f"\n{args.name} ready.")
    print(f"  IP:      {ip}")
    print(f"  workdir: {workdir}")
    print(f"\nNext: cloud_test.py deploy --name {args.name} --branch <branch>")


def run_sysinfo(ip):
    """Collects environment facts worth having on record before drawing
    conclusions from a run -- see remote/sysinfo.sh for why each of
    these turned out to matter during the PR 665 cloud experiment.
    Written to REMOTE_DIR/sysinfo.txt (fetched by `logs`), and also
    streamed to stdout here so it's visible immediately."""
    print("Collecting system info ...", file=sys.stderr)
    ssh_cmd(ip, f"mkdir -p {REMOTE_DIR}")
    scp_to(ip, SCRIPT_DIR / "remote" / "sysinfo.sh", f"{REMOTE_DIR}/sysinfo.sh")
    ssh_cmd(ip, f"bash {REMOTE_DIR}/sysinfo.sh | tee {REMOTE_DIR}/sysinfo.txt")


def cmd_sysinfo(args):
    ip = server_ip(args.name)
    run_sysinfo(ip)


def run_fio(ip, workdir):
    """The same benchmark run by hand throughout the PR 665 cloud
    experiment (see #667) -- it's what caught a machine's attached
    volume being meaningfully slower than its siblings, and a separate
    machine's volume degrading badly under concurrent load despite
    having the highest raw IOPS ceiling of the three. --ioengine=libaio
    (not the default psync) so --iodepth/--numjobs actually take effect,
    rather than silently capping concurrency at 1.
    """
    print("Benchmarking the volume with fio ...", file=sys.stderr)
    ssh_cmd(ip, "which fio >/dev/null 2>&1 || (apt-get update -qq && apt-get install -y -qq fio)")
    test_file = shlex.quote(f"{workdir}/fio-test")
    ssh_cmd(
        ip,
        f"fio --name=randread --filename={test_file} --size=1G --bs=4k "
        "--rw=randread --direct=1 --ioengine=libaio --iodepth=32 "
        f"--numjobs=$(nproc) --runtime=20 --time_based --group_reporting; "
        f"rm -f {test_file}",
    )


def cmd_fio(args):
    ip = server_ip(args.name)
    workdir = workdir_for(args.name)
    run_fio(ip, workdir)


def cmd_deploy(args):
    ip = server_ip(args.name)
    ssh_cmd(ip, f"mkdir -p {REMOTE_DIR}")
    scp_to(ip, SCRIPT_DIR / "remote" / "build.sh", f"{REMOTE_DIR}/build.sh")
    ssh_cmd(ip, f"chmod +x {REMOTE_DIR}/build.sh")
    remote_checkout = f"{REMOTE_DIR}/osm-diffs-src"
    ssh_cmd(
        ip,
        f"{REMOTE_DIR}/build.sh {shlex.quote(args.repo)} "
        f"{shlex.quote(args.branch)} {shlex.quote(remote_checkout)}",
    )
    print(f"\nBuilt and installed on {args.name} ({ip}), branch {args.branch}.")
    print(f"Next: cloud_test.py start --name {args.name}")


def cmd_start(args):
    ip = server_ip(args.name)
    workdir = workdir_for(args.name)

    if args.clean:
        print(f"Clearing {workdir}, keeping the planet download ...", file=sys.stderr)
        keep = " ".join(f"! -name {shlex.quote(f)}" for f in PLANET_FILES)
        ssh_cmd(ip, f"find {shlex.quote(workdir)} -mindepth 1 {keep} -delete")

    scp_to(ip, SCRIPT_DIR / "remote" / "monitor.sh", f"{REMOTE_DIR}/monitor.sh")
    ssh_cmd(ip, f"chmod +x {REMOTE_DIR}/monitor.sh")

    # systemd-run, not screen/nohup: gives a detached process that
    # survives the SSH session ending, plus `systemctl status/stop` for
    # free, without needing a terminal multiplexer at all.
    ssh_cmd(
        ip,
        "systemd-run --unit=osm-diffs-monitor --collect "
        f"--working-directory={REMOTE_DIR} --setenv=WORKDIR={workdir} -- "
        f"/bin/bash {REMOTE_DIR}/monitor.sh",
    )
    ssh_cmd(
        ip,
        "systemd-run --unit=osm-diffs-run --collect "
        f"--working-directory={REMOTE_DIR} -- "
        f"/usr/local/bin/osm-diffs run --workdir {shlex.quote(workdir)}",
    )
    print(f"\nRunning on {args.name} ({ip}).")
    print(f"  workdir: {workdir}")
    print(f"\nCheck in with: cloud_test.py status --name {args.name}")


def cmd_status(args):
    ip = server_ip(args.name)
    workdir = workdir_for(args.name)
    ssh_cmd(ip, "systemctl status osm-diffs-run --no-pager -l || true")
    ssh_cmd(ip, f"df -h {shlex.quote(workdir)}")
    ssh_cmd(ip, f"tail -5 {shlex.quote(workdir)}/pipeline.log 2>/dev/null || true")


def cmd_logs(args):
    ip = server_ip(args.name)
    workdir = workdir_for(args.name)
    dest = Path(args.dest) if args.dest else LOGS_DIR / args.name
    dest.mkdir(parents=True, exist_ok=True)
    scp_from(ip, f"{workdir}/pipeline.log", dest / "pipeline.log")
    scp_from(ip, f"{REMOTE_DIR}/vmstat.log", dest / "vmstat.log")
    scp_from(ip, f"{REMOTE_DIR}/disk.log", dest / "disk.log")
    scp_from(ip, f"{REMOTE_DIR}/sysinfo.txt", dest / "sysinfo.txt")
    print(f"\nLogs saved to {dest}")


def cmd_stop(args):
    ip = server_ip(args.name)
    ssh_cmd(ip, "systemctl stop osm-diffs-run osm-diffs-monitor || true")


def cmd_destroy(args):
    if not args.yes:
        reply = input(f"Delete server and volume for {args.name!r}? [y/N] ")
        if reply.strip().lower() != "y":
            print("Aborted.", file=sys.stderr)
            return
    subprocess.run(["hcloud", "volume", "detach", f"{args.name}-data"])
    subprocess.run(["hcloud", "volume", "delete", f"{args.name}-data"])
    subprocess.run(["hcloud", "server", "delete", args.name])


def cmd_list(_args):
    servers = hcloud_json(["server", "list", "-l", f"{LABEL_KEY}=true"])
    if not servers:
        print("No osm-diffs-test instances running.")
        return
    for s in servers:
        labels = s.get("labels", {})
        branch = labels.get(f"{LABEL_KEY}-branch", "?").replace("--", "/")
        ip = s["public_net"]["ipv4"]["ip"]
        print(f"{s['name']:20} {ip:16} branch={branch}")


def cmd_up(args):
    cmd_create(args)
    cmd_deploy(args)
    cmd_start(args)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    def add_common(p, needs_ssh_key=False, needs_branch=False):
        p.add_argument("--name", required=True, help="Server/volume name, e.g. pr665-m1")
        if needs_ssh_key:
            p.add_argument(
                "--ssh-key", required=True, help="Name of an SSH key already in the Hetzner project"
            )
        if needs_branch:
            p.add_argument("--branch", required=True, help="git branch/ref to build")

    p_up = sub.add_parser("up", help="create + deploy + start, in one shot")
    add_common(p_up, needs_ssh_key=True, needs_branch=True)
    p_up.add_argument("--type", default=DEFAULT_TYPE)
    p_up.add_argument("--location", default=DEFAULT_LOCATION)
    p_up.add_argument("--volume-size", type=int, default=DEFAULT_VOLUME_SIZE)
    p_up.add_argument("--repo", default=DEFAULT_REPO)
    p_up.set_defaults(func=cmd_up)

    p_create = sub.add_parser("create", help="provision the server + volume only")
    add_common(p_create, needs_ssh_key=True)
    p_create.add_argument("--branch", help="just for the label, doesn't build anything")
    p_create.add_argument("--type", default=DEFAULT_TYPE)
    p_create.add_argument("--location", default=DEFAULT_LOCATION)
    p_create.add_argument("--volume-size", type=int, default=DEFAULT_VOLUME_SIZE)
    p_create.set_defaults(func=cmd_create)

    p_deploy = sub.add_parser("deploy", help="build + install a branch on an existing server")
    add_common(p_deploy, needs_branch=True)
    p_deploy.add_argument("--repo", default=DEFAULT_REPO)
    p_deploy.set_defaults(func=cmd_deploy)

    p_start = sub.add_parser("start", help="run the pipeline + monitoring, detached")
    add_common(p_start)
    p_start.add_argument(
        "--clean",
        action="store_true",
        help="clear the workdir first, keeping the planet download",
    )
    p_start.set_defaults(func=cmd_start)

    p_status = sub.add_parser("status", help="is it still running, how full is the disk")
    add_common(p_status)
    p_status.set_defaults(func=cmd_status)

    p_fio = sub.add_parser(
        "fio", help="benchmark the attached volume (also run once automatically by create)"
    )
    add_common(p_fio)
    p_fio.set_defaults(func=cmd_fio)

    p_sysinfo = sub.add_parser(
        "sysinfo", help="OS/kernel/CPU/memory/disk facts (also run once automatically by create)"
    )
    add_common(p_sysinfo)
    p_sysinfo.set_defaults(func=cmd_sysinfo)

    p_logs = sub.add_parser(
        "logs", help="download pipeline.log/vmstat.log/disk.log/sysinfo.txt"
    )
    add_common(p_logs)
    p_logs.add_argument("--dest", help="local directory (default: ./logs/<name>)")
    p_logs.set_defaults(func=cmd_logs)

    p_stop = sub.add_parser("stop", help="stop the pipeline + monitoring (leaves the VM up)")
    add_common(p_stop)
    p_stop.set_defaults(func=cmd_stop)

    p_destroy = sub.add_parser("destroy", help="delete the server and volume")
    add_common(p_destroy)
    p_destroy.add_argument("--yes", action="store_true", help="skip the confirmation prompt")
    p_destroy.set_defaults(func=cmd_destroy)

    p_list = sub.add_parser("list", help="list all osm-diffs-test instances")
    p_list.set_defaults(func=cmd_list)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
