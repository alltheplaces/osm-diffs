#!/usr/bin/env python3
"""Spins up a Hetzner Cloud VM, builds osm-diffs from a given git branch,
and runs the full pipeline against it -- so testing a development branch
on real cloud hardware doesn't mean re-deriving the VM setup dance by
hand each time. See README.md in this directory for the full story,
including prerequisites and a worked example.

Every `hcloud`/`ssh`/`scp` command this runs is echoed to stderr first,
so a flag mismatch against your installed `hcloud` version should be
easy to spot and fix.
"""

import argparse
import json
import os
import shlex
import subprocess
import sys
import tempfile
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


def s3_endpoint(region):
    # Not a placeholder -- "your-objectstorage.com" is Hetzner's real,
    # literal domain for Object Storage.
    return f"https://{region}.your-objectstorage.com"


def s3_credentials():
    """Reads the test-bucket S3 credentials from the local environment --
    never passed as command-line arguments, so they never show up in
    `ps`/shell history on this machine either. Shared by `bucket
    create`/`bucket destroy` and containerized `start`'s S3 env-file
    construction, so all three fail with the same clear message rather
    than three slightly different ones."""
    access_key = os.environ.get("HETZNER_TEST_S3_ACCESS_KEY_ID")
    secret_key = os.environ.get("HETZNER_TEST_S3_ACCESS_KEY_SECRET")
    if not access_key or not secret_key:
        sys.exit(
            "HETZNER_TEST_S3_ACCESS_KEY_ID / HETZNER_TEST_S3_ACCESS_KEY_SECRET "
            "aren't set locally."
        )
    return access_key, secret_key


# `mc`'s own alias name for the Hetzner Object Storage account -- arbitrary,
# just needs to match between `mc alias set` and every later `mc` call.
MC_ALIAS = "containertest"


def mc_alias_set(region):
    """Not sh(): that echoes the full command line, which for `mc alias
    set` would print the secret key in plain text to stderr (terminal
    scrollback, a copy-pasted debugging session, ...). Echoes a redacted
    form instead, same audit-trail intent as sh() without the leak."""
    access_key, secret_key = s3_credentials()
    endpoint = s3_endpoint(region)
    cmd = ["mc", "alias", "set", MC_ALIAS, endpoint, access_key, secret_key]
    redacted = ["mc", "alias", "set", MC_ALIAS, endpoint, access_key, "***"]
    print(f"+ {' '.join(shlex.quote(c) for c in redacted)}", file=sys.stderr)
    subprocess.run(cmd, check=True)


def server_ip(name):
    info = hcloud_json(["server", "describe", name])
    return info["public_net"]["ipv4"]["ip"]


def workdir_for(name):
    """Derived, not stored: re-queries the volume each time rather than
    keeping a separate state file, so there's nothing that can drift out
    of sync with what Hetzner actually has."""
    vol = hcloud_json(["volume", "describe", f"{name}-data"])
    return f"/mnt/HC_Volume_{vol['id']}/workdir"


def label_flags(name, branch=None, image=None):
    labels = [f"{LABEL_KEY}=true", f"{LABEL_KEY}-owner={name}"]
    if branch:
        # Hetzner labels don't allow "/", which branch names often have.
        labels.append(f"{LABEL_KEY}-branch={branch.replace('/', '--')}")
    if image:
        # Same restriction as branch names, plus image refs also use ":"
        # for the tag (e.g. ghcr.io/alltheplaces/osm-diffs:v1.2.3).
        sanitized = image.replace("/", "--").replace(":", "--")
        labels.append(f"{LABEL_KEY}-image={sanitized}")
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
            *label_flags(args.name, args.branch, getattr(args, "image", None)),
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
            *label_flags(args.name, args.branch, getattr(args, "image", None)),
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
    # Both the build and pull paths end by extracting binaries out of the
    # locally tagged osm-diffs-test image, via the shared script.
    scp_to(ip, SCRIPT_DIR / "remote" / "extract-binaries.sh", f"{REMOTE_DIR}/extract-binaries.sh")
    ssh_cmd(ip, f"chmod +x {REMOTE_DIR}/extract-binaries.sh")

    if args.image:
        scp_to(ip, SCRIPT_DIR / "remote" / "pull.sh", f"{REMOTE_DIR}/pull.sh")
        ssh_cmd(ip, f"chmod +x {REMOTE_DIR}/pull.sh")
        ssh_cmd(ip, f"{REMOTE_DIR}/pull.sh {shlex.quote(args.image)}")
        print(f"\nPulled and installed on {args.name} ({ip}), image {args.image}.")
    else:
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
    containerized = getattr(args, "containerized", False)
    if containerized:
        # Checked before any Hetzner API calls -- a missing flag
        # shouldn't need working connectivity to report.
        if not args.mem_limit or not args.cpu_limit:
            sys.exit("--containerized requires --mem-limit and --cpu-limit")
        if args.bucket_name and not args.bucket_region:
            sys.exit("--bucket-name requires --bucket-region")

    ip = server_ip(args.name)
    workdir = workdir_for(args.name)

    # getattr, not args.clean: cmd_up() calls this reusing "up"'s own
    # argparse namespace, which has no --clean flag (only "start"'s
    # does) -- args.clean would raise AttributeError there.
    if getattr(args, "clean", False):
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

    if containerized:
        run_command = containerized_run_command(args, ip, workdir)
    else:
        run_command = f"/usr/local/bin/osm-diffs run --workdir {shlex.quote(workdir)}"

    ssh_cmd(
        ip,
        "systemd-run --unit=osm-diffs-run --collect "
        # Debian's systemd defaults new units to the same 1024
        # soft-limit open-file cap an interactive shell gets --
        # switching to systemd-run does NOT fix the EMFILE crash a
        # large external sort's spilled-chunk merge hit during the
        # PR 665 experiment (build_coverage, specifically). Raise it
        # explicitly rather than relying on ambient defaults that
        # already bit us once.
        "--property=LimitNOFILE=65536:65536 "
        f"--working-directory={REMOTE_DIR} -- "
        f"{run_command}",
    )
    mode = " (containerized)" if containerized else ""
    print(f"\nRunning on {args.name} ({ip}){mode}.")
    print(f"  workdir: {workdir}")
    print(f"\nCheck in with: cloud_test.py status --name {args.name}")


def containerized_run_command(args, ip, workdir):
    """Builds the `podman run ...` command line for containerized `start`,
    doing whatever synchronous prep it needs first -- fetching a regional
    extract, pushing an S3 credentials env file, fixing up ownership for
    the container's non-root user -- before the detached systemd-run unit
    that will actually execute it."""
    if args.regional_extract:
        print(f"Fetching regional extract {args.regional_extract} ...", file=sys.stderr)
        scp_to(
            ip,
            SCRIPT_DIR / "remote" / "fetch_test_extract.sh",
            f"{REMOTE_DIR}/fetch_test_extract.sh",
        )
        ssh_cmd(ip, f"chmod +x {REMOTE_DIR}/fetch_test_extract.sh")
        ssh_cmd(
            ip,
            f"{REMOTE_DIR}/fetch_test_extract.sh "
            f"{shlex.quote(args.regional_extract)} {shlex.quote(workdir)}",
        )

    env_flag = ""
    if args.bucket_name:
        access_key, secret_key = s3_credentials()
        env_content = (
            f"S3_ENDPOINT={s3_endpoint(args.bucket_region)}\n"
            f"S3_BUCKET={args.bucket_name}\n"
            f"S3_REGION={args.bucket_region}\n"
            f"S3_ACCESS_KEY_ID={access_key}\n"
            f"S3_ACCESS_KEY_SECRET={secret_key}\n"
        )
        with tempfile.NamedTemporaryFile("w", suffix=".env", delete=False) as f:
            f.write(env_content)
            local_env_path = f.name
        try:
            # A file, not command-line flags: keeps the S3 secret out of
            # `ps`/shell history on the remote host.
            scp_to(ip, local_env_path, f"{REMOTE_DIR}/s3.env")
        finally:
            Path(local_env_path).unlink()
        ssh_cmd(ip, f"chmod 600 {REMOTE_DIR}/s3.env")
        env_flag = f"--env-file {REMOTE_DIR}/s3.env "

    # The image runs as non-root (USER 1000 in the Containerfile); the
    # workdir volume is root-owned on the host (created by `create`), so
    # the container would otherwise hit EACCES writing into it.
    ssh_cmd(ip, f"chown -R 1000:1000 {shlex.quote(workdir)}")

    run_id_flag = f" --run_id {shlex.quote(args.run_id)}" if args.run_id else ""
    return (
        # --read-only: the pipeline should only ever write into /workdir
        # (the mounted volume), never into the container's own
        # filesystem -- an early bug wrote outside it and went unnoticed
        # for a while (see #728). The mounted volume stays writable
        # regardless of --read-only, since it's a separate mount point
        # from the container's root filesystem.
        "podman run --rm --read-only "
        f"--memory={shlex.quote(args.mem_limit)} --cpus={shlex.quote(args.cpu_limit)} "
        f"-v {shlex.quote(workdir)}:/workdir {env_flag}"
        f"osm-diffs-test run --workdir /workdir{run_id_flag}"
    )


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


def cmd_bucket_create(args):
    mc_alias_set(args.region)
    sh(["mc", "mb", f"{MC_ALIAS}/{args.name}"])
    print(f"\nBucket {args.name} created in {args.region}.")
    print(f"  S3_ENDPOINT: {s3_endpoint(args.region)}")
    print(f"  S3_BUCKET:   {args.name}")
    print(
        f"\nNext: cloud_test.py start --name <server> --containerized ... "
        f"--bucket-name {args.name} --bucket-region {args.region}"
    )
    print(f"When done: cloud_test.py bucket destroy --name {args.name} --region {args.region}")


def cmd_bucket_destroy(args):
    if not args.yes:
        reply = input(f"Delete bucket {args.name!r} in {args.region!r} (and everything in it)? [y/N] ")
        if reply.strip().lower() != "y":
            print("Aborted.", file=sys.stderr)
            return
    mc_alias_set(args.region)
    subprocess.run(["mc", "rm", "--recursive", "--force", f"{MC_ALIAS}/{args.name}"])
    subprocess.run(["mc", "rb", f"{MC_ALIAS}/{args.name}"])


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

    def add_common(p, needs_ssh_key=False):
        p.add_argument("--name", required=True, help="Server/volume name, e.g. pr665-m1")
        if needs_ssh_key:
            p.add_argument(
                "--ssh-key", required=True, help="Name of an SSH key already in the Hetzner project"
            )

    def add_deploy_target(p):
        """--branch (build from source) or --image (pull an existing
        image), mutually exclusive, one required."""
        group = p.add_mutually_exclusive_group(required=True)
        group.add_argument("--branch", help="git branch/ref to build")
        group.add_argument(
            "--image",
            help="pull this image instead of building (e.g. ghcr.io/alltheplaces/osm-diffs:v1.2.3)",
        )
        p.add_argument("--repo", default=DEFAULT_REPO, help="ignored with --image")

    def add_containerized(p):
        """--containerized and the flags that only make sense with it."""
        p.add_argument(
            "--containerized",
            action="store_true",
            help="run via `podman run --memory=/--cpus=` for real cgroup accounting, "
            "instead of the bare extracted binary",
        )
        p.add_argument("--mem-limit", help="e.g. 4g -- required with --containerized")
        p.add_argument("--cpu-limit", help="e.g. 2 -- required with --containerized")
        p.add_argument(
            "--regional-extract",
            help="Geofabrik path fragment (e.g. europe/switzerland), instead of the "
            "full planet -- --containerized only",
        )
        p.add_argument("--run-id", help="embedded into the provenance BOM -- --containerized only")
        p.add_argument(
            "--bucket-name", help="ephemeral S3 test bucket to upload to -- --containerized only"
        )
        p.add_argument("--bucket-region", help="required with --bucket-name")

    p_up = sub.add_parser("up", help="create + deploy + start, in one shot")
    add_common(p_up, needs_ssh_key=True)
    add_deploy_target(p_up)
    p_up.add_argument("--type", default=DEFAULT_TYPE)
    p_up.add_argument("--location", default=DEFAULT_LOCATION)
    p_up.add_argument("--volume-size", type=int, default=DEFAULT_VOLUME_SIZE)
    add_containerized(p_up)
    p_up.set_defaults(func=cmd_up)

    p_create = sub.add_parser("create", help="provision the server + volume only")
    add_common(p_create, needs_ssh_key=True)
    p_create.add_argument("--branch", help="just for the label, doesn't build anything")
    p_create.add_argument("--type", default=DEFAULT_TYPE)
    p_create.add_argument("--location", default=DEFAULT_LOCATION)
    p_create.add_argument("--volume-size", type=int, default=DEFAULT_VOLUME_SIZE)
    p_create.set_defaults(func=cmd_create)

    p_deploy = sub.add_parser(
        "deploy", help="build from a branch, or pull an existing image, onto an existing server"
    )
    add_common(p_deploy)
    add_deploy_target(p_deploy)
    p_deploy.set_defaults(func=cmd_deploy)

    p_start = sub.add_parser("start", help="run the pipeline + monitoring, detached")
    add_common(p_start)
    p_start.add_argument(
        "--clean",
        action="store_true",
        help="clear the workdir first, keeping the planet download",
    )
    add_containerized(p_start)
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

    p_bucket = sub.add_parser("bucket", help="ephemeral S3 test bucket lifecycle")
    bucket_sub = p_bucket.add_subparsers(dest="bucket_command", required=True)

    p_bucket_create = bucket_sub.add_parser("create", help="create a test bucket")
    p_bucket_create.add_argument("--name", required=True, help="Bucket name")
    p_bucket_create.add_argument(
        "--region", required=True, help="Hetzner Object Storage region, e.g. fsn1"
    )
    p_bucket_create.set_defaults(func=cmd_bucket_create)

    p_bucket_destroy = bucket_sub.add_parser(
        "destroy", help="delete a test bucket and everything in it"
    )
    p_bucket_destroy.add_argument("--name", required=True, help="Bucket name")
    p_bucket_destroy.add_argument(
        "--region", required=True, help="Hetzner Object Storage region, e.g. fsn1"
    )
    p_bucket_destroy.add_argument("--yes", action="store_true", help="skip the confirmation prompt")
    p_bucket_destroy.set_defaults(func=cmd_bucket_destroy)

    p_list = sub.add_parser("list", help="list all osm-diffs-test instances")
    p_list.set_defaults(func=cmd_list)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
