# Testing a branch on cloud machines

Two small tools: `cloud_test.py` runs the full `osm-diffs` pipeline
against a development branch on real Hetzner Cloud hardware, without
repeating the manual VM setup by hand each time -- create a VM, build
the branch's `Containerfile` natively on it, run the pipeline detached,
and pull back logs, all from one command. `analyze.py` then makes sense
of what got pulled back, without re-writing the same throwaway parsing
script every time.

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

## What gets collected, and why

Every signal here traces back to one specific design assumption worth
measuring, not just logging for its own sake: `OsmFeatureIndex`
(#655/#667) is a large, uncompressed structure that's `mmap`'d and
left to the OS page cache, deliberately *not* backed by an explicit
LRU decode cache. Whether that holds up under real memory pressure, on
real disk, isn't something you can determine by reading the code.

- **`vmstat.log`** -- `rss_file_bytes` climbing (in `pipeline.log`'s
  own memstats snapshots) is that assumption holding up: mmap'd pages
  resident, but cleanly reclaimable, not counted against anonymous
  memory. `wa` (iowait) climbing here instead is the opposite signal --
  page faults against the index turning out to be expensive, not cheap.
- **`disk.log`** -- external sorts spill temporary files and delete
  them again within a single step; a `df` snapshot after the fact would
  miss that, so this samples usage over time instead, per step.
- **`fio`** -- disk *latency under concurrency* turned out to matter
  more than raw throughput (one machine's volume had the highest raw
  IOPS of three, but degraded worst once anything was actually
  concurrent) -- a synthetic benchmark isolates that from whatever the
  pipeline itself happens to be doing at the time.
- **`sysinfo.txt`** -- kernel version, cgroup availability, and CPU
  model all turned out to change what the other numbers mean (e.g.
  `/sys/fs/cgroup/memory.current` not existing outside a container, so
  `pipeline.log`'s own `cgroup_*` fields read `None` throughout) --
  recorded once so nobody has to reconstruct it from memory later.

## Prerequisites

- The [`hcloud` CLI](https://github.com/hetznercloud/cli), installed and
  authenticated against the right project (`hcloud context list` should
  show an active context).
- An SSH key already uploaded to that Hetzner project (`hcloud ssh-key
  list`) -- its *name*, not the key file itself, is what you pass to
  this tool.
- Nothing else: both scripts only use the Python standard library (no
  `pip install` needed); `cloud_test.py` shells out to `hcloud`/`ssh`/
  `scp` for everything it does beyond that.

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

## Making sense of the logs

`analyze.py` is deliberately narrow -- it handles the mechanical,
always-useful parts, not an open-ended analysis framework, since the
actual question worth asking about a given run is usually specific to
that run and not something worth pre-guessing:

```console
$ ./analyze.py timeline logs/pr665-m1/pipeline.log
$ ./analyze.py vmstat-stats logs/pr665-m1/vmstat.log --step build_coverage
$ ./analyze.py disk-stats logs/pr665-m1/disk.log --step build_coverage
$ ./analyze.py compare logs/pr665-m1 logs/pr665-m2
```

`timeline` collapses repeated boilerplate lines (the same message
recurring 5+ times, e.g. per-feature warnings) into one summary line
each -- `--all` shows everything uncollapsed. `vmstat-stats`/
`disk-stats` take either an explicit `--from`/`--to` window or
`--step NAME`, which derives the window from that step's start/end in
the sibling `pipeline.log` automatically. See
[`test_data/`](test_data) for small example logs to try these against,
and as a reference for what each log actually looks like.

## Why build on the VM, not locally

Building the project's `Containerfile` locally on an Apple Silicon
Mac requires `podman build --platform linux/amd64`, which runs under
QEMU emulation -- slow enough in practice (~2 hours for a full build,
including one C++-compiler OOM-kill under emulation with the podman
VM's default memory) that it isn't worth it when a target x86_64 VM is
right there. `deploy` always builds natively on the remote machine
instead, the same way the three-machine PR 665 experiment ended up
doing it after discovering this the hard way.

It's also the right build for a different reason, not just speed:
building from the project's own `Containerfile` gets you the *exact*
same toolchain production uses -- same Rust version, same musl libc,
same compiler flags -- not just "close enough." That matters
specifically for this tool's purpose: a benchmark run against a
locally-built binary would leave you wondering whether a result is
real or an artifact of a different allocator/libc, exactly the kind of
noise you don't want when trying to draw conclusions from a branch's
behavior on real hardware.

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

- **Cost control is entirely manual.** `list` tells you what's running;
  nothing here auto-destroys an idle instance. If you forget a `destroy`,
  the meter keeps running.
- **One workdir volume per instance**, sized at creation time. Hetzner
  volume size limits are sometimes capped below the documented per-volume
  maximum by a project-level quota (we hit exactly this during the PR 665
  experiment) -- if `create` fails on `--volume-size`, that's the first
  thing to check, via a support ticket to raise the quota rather than
  anything this tool can work around.
