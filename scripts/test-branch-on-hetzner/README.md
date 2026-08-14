# Testing a branch on cloud machines

A small tool for running the full `osm-diffs` pipeline against a
development branch on real Hetzner Cloud hardware, without repeating the
manual VM setup by hand each time: create a VM, build the branch's
`Containerfile` natively on it, run the pipeline detached, and pull back
logs -- all from one command.

This exists because staged rollouts of large pipeline changes (see e.g.
[#655](https://github.com/alltheplaces/osm-diffs/issues/655)) call for
repeating this exact dance on cloud hardware at multiple points, not
just once -- and doing it by hand each time is slow and error-prone (in
one evening we hit: an accidentally-wrong OS image, a default file
descriptor limit too low for a large external sort, a Hetzner volume
size quota, and a container build that silently targeted the wrong CPU
architecture). Automating the setup doesn't remove the need to *look at*
the results carefully -- it just removes the repetitive, error-prone
parts of getting there.

**Not a general-purpose deployment tool.** This has nothing to do with
how `osm-diffs` actually gets deployed to production (see
`../../docs/RELEASING.md` for that) -- it only exists to make ad hoc
experiments on cloud hardware repeatable.

## Prerequisites

- The [`hcloud` CLI](https://github.com/hetznercloud/cli), installed and
  authenticated against the right project (`hcloud context list` should
  show an active context).
- An SSH key already uploaded to that Hetzner project (`hcloud ssh-key
  list`) -- its *name*, not the key file itself, is what you pass to
  this tool.
- Nothing else: `cloud_test.py` only uses the Python standard library
  (no `pip install` needed), and shells out to `hcloud`/`ssh`/`scp` for
  everything else.

## Quick start

```console
$ ./cloud_test.py up --name pr665-m1 --branch experiment/skip-old-osm-path --ssh-key my-key
...
Running on pr665-m1 (1.2.3.4).
  workdir: /mnt/HC_Volume_.../workdir

$ ./cloud_test.py status --name pr665-m1
...
$ ./cloud_test.py logs --name pr665-m1
Logs saved to logs/pr665-m1

$ ./cloud_test.py destroy --name pr665-m1
Delete server and volume for 'pr665-m1'? [y/N] y
```

`up` is `create` + `deploy` + `start` in sequence; run them separately
if you want to, say, redeploy a newer commit of the same branch onto an
already-provisioned machine (`deploy` + `start`, skipping `create`), or
restart a run with a clean workdir without tearing down the VM
(`start --clean`).

## Commands

| Command | What it does |
|---|---|
| `up` | Create the server + volume, build the branch, start the pipeline. |
| `create` | Server + a formatted, attached, automounted data volume. Also collects `sysinfo` and runs `fio` once, automatically. |
| `deploy` | Clone/update the given branch on an existing server and build it via the project's `Containerfile`, natively (see below for why that matters). |
| `start` | Launch `osm-diffs run` and the vmstat/disk monitor, both detached via `systemd-run`. `--clean` clears the workdir first but keeps `planet-latest.osm.pbf`/its metadata sidecar, so re-running doesn't re-fetch the planet over BitTorrent. |
| `status` | `systemctl status` for the run, plus `df` and the last few `pipeline.log` lines. |
| `fio` | Random-read benchmark of the attached volume (the same command used by hand throughout the PR 665 experiment -- see `#667`). Re-runnable anytime, e.g. to check whether a result was a one-off blip. |
| `sysinfo` | OS/kernel version, CPU model, memory, swap, disk layout, cgroup limits -- environment facts that turned out to matter for interpreting results but aren't anything this tool controls. |
| `logs` | Downloads `pipeline.log`, `vmstat.log`, `disk.log`, `sysinfo.txt` to `logs/<name>/`. |
| `stop` | Stops the pipeline + monitor, leaves the VM (and its disk contents) alone. |
| `destroy` | Deletes the server and volume. Asks for confirmation unless `--yes`. |
| `list` | Lists every instance this tool created (via a Hetzner label), so nothing gets forgotten and left running. |

Run `./cloud_test.py <command> --help` for the full flag list; defaults
are `cpx32` / `hel1` / a 400GB volume, all overridable.

## Why build on the VM, not locally

Building the project's `Containerfile` locally on an Apple Silicon
Mac requires `podman build --platform linux/amd64`, which runs under
QEMU emulation -- slow enough in practice (~2 hours for a full build,
including one C++-compiler OOM-kill under emulation with the podman
VM's default memory) that it isn't worth it when a target x86_64 VM is
right there. `deploy` always builds natively on the remote machine
instead, the same way the three-machine PR 665 experiment ended up
doing it after discovering this the hard way.

## No `screen`, no `nohup`

`start` uses `systemd-run --unit=... --collect` to launch both the
pipeline and the monitoring script as detached transient units. That
survives the SSH session ending the same way `screen`/`nohup` would,
but gives `systemctl status`/`stop` for free and doesn't need a
terminal multiplexer session per machine that a human has to remember
to attach/detach.

## State

There's no separate state file tracking which instances exist --
`list`/`destroy` query Hetzner directly (servers/volumes tagged with the
`osm-diffs-test` label), and `workdir_for()` re-derives the mounted
volume's path from `hcloud volume describe` each time rather than
caching it. Nothing here can drift out of sync with what Hetzner
actually has, at the cost of an extra API round-trip per command --
cheap, and worth it.

## Known gaps / not attempted here

- **Not tested against the real Hetzner API** from the environment this
  was written in (no credentials there). Treat the first real `up` as a
  shakedown of the exact `hcloud` flag names for your installed CLI
  version, not a guarantee -- every command this tool runs is echoed to
  stderr first specifically to make that debugging easy.
- **Cost control is entirely manual.** `list` tells you what's running;
  nothing here auto-destroys an idle instance. If you forget a `destroy`,
  the meter keeps running.
- **One workdir volume per instance**, sized at creation time. Hetzner
  volume size limits are sometimes capped below the documented per-volume
  maximum by a project-level quota (we hit exactly this during the PR 665
  experiment) -- if `create` fails on `--volume-size`, that's the first
  thing to check, via a support ticket to raise the quota rather than
  anything this tool can work around.
- **`macos_monitor.sh`-equivalent for a local dev-machine run** isn't
  part of this tool -- it's specifically for cloud VMs. See
  `../experiments/pr-665/monitor-macos.sh` for the macOS-side monitor
  used during local full-planet testing.
