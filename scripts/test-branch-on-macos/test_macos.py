#!/usr/bin/env python3
"""Quick local iteration: build the current checkout, run the pipeline
against a workdir, and monitor vm_stat/RSS alongside it -- so testing a
change doesn't mean re-typing the same vm_stat loop by hand each time.

See scripts/test-branch-on-hetzner/ for the equivalent against real
cloud hardware; this is deliberately much smaller -- no VM lifecycle to
manage, no branch to clone, since the whole point is fast iteration on
whatever's currently checked out on this machine. It also doesn't build
via the project's Containerfile the way that tool does: plain `cargo
build --release` here, not matching production's exact toolchain,
because the point is fast turnaround, not comparable-to-production
numbers.

Usage:
  test_macos.py build
  test_macos.py run --workdir DIR [--clean] [--skip-build]
"""

import argparse
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
BINARY = REPO_ROOT / "target" / "release" / "osm-diffs"
PLANET_FILES = ("planet-latest.osm.pbf", "planet-latest.osm.pbf.meta.json")


def sh(cmd, **kwargs):
    print(f"+ {' '.join(shlex.quote(c) for c in cmd)}", file=sys.stderr)
    return subprocess.run(cmd, check=True, **kwargs)


def cmd_build(args):
    sh(["cargo", "build", "--release"], cwd=REPO_ROOT)


def cmd_run(args):
    workdir = Path(args.workdir).resolve()
    workdir.mkdir(parents=True, exist_ok=True)

    if args.clean:
        print(f"Clearing {workdir}, keeping the planet download ...", file=sys.stderr)
        for entry in workdir.iterdir():
            if entry.name in PLANET_FILES:
                continue
            if entry.is_dir():
                shutil.rmtree(entry)
            else:
                entry.unlink()

    if not args.skip_build:
        cmd_build(args)

    if not BINARY.exists():
        sys.exit(f"{BINARY} not found -- run `build` first, or drop --skip-build")

    monitor_log = workdir / "macos_monitor.log"
    print(f"Starting monitor -> {monitor_log}", file=sys.stderr)
    with open(monitor_log, "w") as log_file:
        monitor = subprocess.Popen(
            ["bash", str(SCRIPT_DIR / "monitor.sh")],
            stdout=log_file,
            stderr=subprocess.STDOUT,
        )

        exit_code = 0
        try:
            print(f"+ {BINARY} run --workdir {workdir}", file=sys.stderr)
            result = subprocess.run([str(BINARY), "run", "--workdir", str(workdir)])
            exit_code = result.returncode
        except KeyboardInterrupt:
            print("\nInterrupted.", file=sys.stderr)
            exit_code = 130
        finally:
            monitor.terminate()
            try:
                monitor.wait(timeout=5)
            except subprocess.TimeoutExpired:
                monitor.kill()

    print(f"\npipeline.log: {workdir / 'pipeline.log'}")
    print(f"monitor log:  {monitor_log}")
    sys.exit(exit_code)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_build = sub.add_parser("build", help="cargo build --release")
    p_build.set_defaults(func=cmd_build)

    p_run = sub.add_parser("run", help="build (unless --skip-build), run, and monitor")
    p_run.add_argument("--workdir", required=True)
    p_run.add_argument(
        "--clean", action="store_true", help="clear the workdir first, keeping the planet download"
    )
    p_run.add_argument(
        "--skip-build", action="store_true", help="use the existing release binary as-is"
    )
    p_run.set_defaults(func=cmd_run)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
