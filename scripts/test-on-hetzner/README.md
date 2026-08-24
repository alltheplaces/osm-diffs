# Testing on cloud machines

Two small tools: `cloud_test.py` runs the full `osm-diffs` pipeline on
real Hetzner Cloud hardware, without repeating the manual VM setup by
hand each time -- create a VM, either build a development branch's
`Containerfile` natively on it or pull an already-built image (e.g. a
released `ghcr.io` container), run the pipeline detached, and pull back
logs, all from one command. `analyze.py` then makes sense of what got
pulled back, without re-writing the same throwaway parsing script every
time.

Runs either bare (the pipeline binary extracted straight onto the VM)
or, via `start --containerized`, inside `podman run --memory=/--cpus=`
for real cgroup memory/CPU accounting -- see
[#722](https://github.com/alltheplaces/osm-diffs/issues/722) for why
that distinction matters (a bare VM run can never populate
`pipeline.log`'s own `cgroup_*` fields; a containerized one can).

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
- [`uv`](https://docs.astral.sh/uv/) -- run `uv sync` once from
  `scripts/` to install `cloud_test.py`'s real dependencies (`boto3`
  for the S3 test-bucket lifecycle, `duckdb` for `validate`). `hcloud`/
  `ssh`/`scp`/`podman` are still shelled out to directly, not wrapped
  in a library -- what you see echoed to stderr is exactly what runs.
- `HETZNER_TEST_S3_ACCESS_KEY_ID`/`HETZNER_TEST_S3_ACCESS_KEY_SECRET` in
  your own environment, only if using `--bucket-name` (see
  "Containerized runs, regional extracts" below).

Changing `cloud_test.py`? Run `uv run pytest` from `scripts/` first --
`tests/test_cloud_test.py` covers the pure logic (label sanitization,
S3/bucket calls, command construction) without touching real
Hetzner/S3, and is also enforced in CI (`test-scripts.yml`).

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
| `up` | Create the server + volume, deploy, start the pipeline. |
| `create` | Server + a formatted, attached, automounted data volume. Also collects `sysinfo` and runs `fio` once, automatically. Prints the exact `destroy` command needed to remove what it just created. |
| `deploy` | `--branch NAME`: clone/update the given branch on an existing server and build it via the project's `Containerfile`, natively (see below for why that matters). `--image REF`: pull an already-built image instead (e.g. `ghcr.io/alltheplaces/osm-diffs:v1.2.3`) -- either way, binaries get extracted the same way, so bare-mode `start` works unchanged regardless of which path was used. |
| `start` | Launch `osm-diffs run` and the vmstat/disk monitor, both detached via `systemd-run`. `--clean` clears the workdir first but keeps `planet-latest.osm.pbf`/its metadata sidecar, so re-running doesn't re-download the ~94GB planet file. `--containerized --mem-limit SIZE --cpu-limit N` runs `podman run` against the image `deploy` produced instead of the bare extracted binary, for real cgroup accounting -- see below. |
| `status` | `systemctl status` for the run, plus `df` and the last few `pipeline.log` lines. |
| `fio` | Random-read benchmark of the attached volume (the same command used by hand throughout the PR 665 experiment -- see `#667`). Re-runnable anytime, e.g. to check whether a result was a one-off blip. |
| `sysinfo` | OS/kernel version, CPU model, memory, swap, disk layout, cgroup limits -- environment facts that turned out to matter for interpreting results but aren't anything this tool controls. |
| `logs` | Downloads `pipeline.log`, `vmstat.log`, `disk.log`, `sysinfo.txt`, `dmesg.log` to `logs/<name>/`. |
| `stop` | Stops the pipeline + monitor, leaves the VM (and its disk contents) alone. |
| `destroy` | Deletes the server and volume. Asks for confirmation unless `--yes`. Prints a rough cost estimate for the run just torn down (unit price × actual lifetime, from Hetzner's own `/v1/pricing`) -- an estimate, not the invoiced figure; Object Storage/traffic aren't included. |
| `bucket create`/`bucket destroy` | Ephemeral S3 test bucket lifecycle -- see "Containerized runs" below. |
| `validate` | Hard pass/fail checks against a run's `conflated.parquet` + downloaded logs -- see "Validation checks" below. |
| `list` | Lists every instance this tool created (via a Hetzner label), so nothing gets forgotten and left running. `--bucket-region LOC` also lists live test buckets in that Object Storage region. |

Run `./cloud_test.py <command> --help` for the full flag list; defaults
are `cpx32` / `hel1` / a 400GB volume, all overridable.

## Containerized runs, regional extracts

```console
$ ./cloud_test.py up --name reg1 --ssh-key my-key \
    --image ghcr.io/alltheplaces/osm-diffs:v1.2.3 \
    --containerized --mem-limit 4g --cpu-limit 2 \
    --regional-extract europe/switzerland
```

- `--containerized --mem-limit SIZE --cpu-limit N` runs the pipeline via
  `podman run --memory=<SIZE> --cpus=<N>` instead of the bare extracted
  binary. This is the whole point for anything measuring memory/CPU
  behavior: a bare-VM run's `cgroup_current_bytes` is normally still
  populated (systemd puts every service unit into its own cgroup even
  outside a container), but `cgroup_max_bytes` reads `None` -- there's
  no configured limit to report -- so it can't validate a
  memory-pressure design against an actual limit, only a containerized
  run (with a real `podman --memory`) can. See
  [`src/pipeline/memstats.rs`](../../src/pipeline/memstats.rs)'s own
  doc comment for the full nuance. `--mem-limit`/`--cpu-limit` are
  required together with `--containerized`.
- `--regional-extract REGION` (a Geofabrik path fragment, e.g.
  `europe/switzerland`) downloads that extract straight to
  `planet-latest.osm.pbf` before starting the container, skipping the
  planet download entirely -- useful for a quick functional smoke test
  of the tool/pipeline itself. Since the planet now downloads over
  plain HTTPS instead of BitTorrent (~28 minutes for the full ~94GB
  file, not the ~4h48m the old BitTorrent path took), skipping the
  download buys much less than it used to, and a small extract creates
  no real memory pressure to observe in the first place -- so prefer
  omitting `--regional-extract` for anything actually measuring
  `--mem-limit` behavior; only worth it for a region meaningfully
  bigger than CI's own tiny `zugerland.osm.pbf` fixture (40 KB)
  regardless. Omit it to exercise the real download path against the
  full planet, which is now the recommended default even for
  comparing several `--mem-limit`/`--cpu-limit`/`--type`
  combinations, e.g. for #711.
- `--bucket-name NAME --bucket-region LOC` uploads output to an
  ephemeral S3-compatible test bucket, reading credentials from
  `HETZNER_TEST_S3_ACCESS_KEY_ID`/`HETZNER_TEST_S3_ACCESS_KEY_SECRET` in
  your own environment (never passed as command-line arguments, to keep
  them out of `ps`/shell history on the remote host). Omit it and the
  container just doesn't upload anywhere, same as the pipeline's own
  `S3_ENDPOINT`-unset behavior.
- `--run-id ID` is passed straight through as `osm-diffs run --run_id
  ID`, embedded into the output's provenance BOM.

Recommended: keep all of this pointed at a Hetzner project (and S3
credentials) dedicated to testing, separate from anything
production-related -- testing shouldn't touch production, since
something always goes wrong during testing.

## Validation checks

```console
$ ./cloud_test.py validate \
    --bucket-name osm-diffs-container-test-1 --bucket-region fsn1 \
    --pipeline-log logs/reg1/pipeline.log \
    --mem-limit 4g --expect-pipeline-version 0.8.0 --min-atp-features 100000
```

`validate` runs a fixed set of **hard** checks -- invariants that can't
legitimately vary run-to-run, so a failure means something's actually
broken, not just that today's data looks different from yesterday's:

- Output is non-empty, and its schema matches
  [`docs/outputs/CONFLATED_PARQUET.md`](../../docs/outputs/CONFLATED_PARQUET.md)
  (struct field names, not full types -- e.g. this is what would have
  caught `changeset` if #731 had only updated one of the writer/doc).
- `atp`/`atp_geometry` and `osm`/`osm_geometry` are null exactly
  together, and every `osm_geometry` is a valid geometry.
- The embedded provenance BOM is present, passes
  [`cyclonedx-cli validate`](https://github.com/CycloneDX/cyclonedx-cli)
  (the same tool `test-container.yml` already uses), and -- if
  `--expect-pipeline-version` is given -- its `pipeline_version` matches.
- The `conflate` step reached its `phase="end"` log record with no
  `ERROR` records logged after `phase="start"`.
- `pipeline.log`'s `cgroup_current_bytes`/`cgroup_max_bytes` are both
  populated on that record. `cgroup_max_bytes` specifically is the
  proof: it only reads non-`None` when a real memory limit was
  configured (i.e. `podman --memory`), unlike `cgroup_current_bytes`,
  which is normally populated even on a bare VM -- and, if
  `--mem-limit` is given, `check_cgroup_signal` also checks that
  `cgroup_max_bytes` matches it.
- No OOM-kill signature in the downloaded `dmesg.log` (an OOM-killed
  step's own log record is simply missing, not an error entry -- this
  is the check that actually catches that case).
- AllThePlaces' geometry count (`import_atp`'s tally) is at least
  `--min-atp-features`, if given -- skipped (not silently passed)
  without it, since there's no universal floor that makes sense across
  every run.

`validate` also prints a set of **advisory** checks -- content-shaped
signals that are expected to drift as real data and matching logic
evolve, so they're never grounds for a fixed pass/fail, only reported
for a human to eyeball:

- Conflation match rate -- skipped (not silently passed) if
  `--regional-extract` is given, since AllThePlaces is worldwide and a
  regional OSM extract will show ~0% match outside its region by
  design, not by defect.
- `rss_file_bytes` vs `rss_anon_bytes`/`rss_shmem_bytes` at peak, from
  the periodic `conflate.match: progress` snapshots logged during
  matching -- the mmap/page-cache design's own signal (#711); large
  `rss_shmem_bytes` is flagged as a likely tmpfs-workdir
  misconfiguration worth a look.
- Any 85%-of-cgroup-limit `WARN` the pipeline already self-logs.
- Disk headroom, from the downloaded `disk.log`.
- Wall-clock timings per step, from `pipeline.log`'s own
  `elapsed_seconds` fields.
- OSM geometry count (matched features only) from `conflate.write`'s
  tally.

`--bucket-name`/`--bucket-region` point `validate` at the same test
bucket `start --containerized --bucket-name ...` uploaded to;
`--pipeline-log` (and, if not alongside it, `--dmesg-log`) point at a
local directory `logs` already downloaded to; `--regional-extract`
should match whatever `start` was given, if anything. Standalone-runnable
against any past run this way, even after its VM has been `destroy`ed.
Exits non-zero if any hard check fails -- advisory checks never affect
the exit code.

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
