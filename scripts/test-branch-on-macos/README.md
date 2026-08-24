# Quick local iteration

Build the current checkout, run the pipeline against a workdir, and
monitor `vm_stat`/RSS alongside it -- so testing a change doesn't mean
re-typing the same monitoring loop by hand each time.

Much smaller than [`../test-on-hetzner/`](../test-on-hetzner)
on purpose: no VM lifecycle to manage, no branch to clone -- everything
runs on the machine you're already on, against whatever's currently
checked out. It also doesn't build via the project's `Containerfile`
the way that tool does: plain `cargo build --release` here, not
matching production's exact toolchain, because the point is fast
turnaround on a local edit-run loop, not comparable-to-production
numbers. Use `test-on-hetzner` when the toolchain match or real
hardware actually matters.

## Usage

```console
$ ./test_macos.py run --workdir /tmp/osm-diffs-workdir
+ cargo build --release
   ...
Starting monitor -> /tmp/osm-diffs-workdir/macos_monitor.log
+ .../target/release/osm-diffs run --workdir /tmp/osm-diffs-workdir
   ...

pipeline.log: /tmp/osm-diffs-workdir/pipeline.log
monitor log:  /tmp/osm-diffs-workdir/macos_monitor.log
```

- `--skip-build` reuses the existing `target/release/osm-diffs` as-is,
  for re-running against the same binary without waiting on a rebuild.
- `--clean` clears the workdir first but keeps `planet-latest.osm.pbf`/
  its metadata sidecar, so re-running doesn't re-download the ~94GB
  planet file.
- `./test_macos.py build` runs just the `cargo build --release` step on
  its own.

Ctrl-C (or the pipeline exiting on its own, successfully or not) always
stops the monitor and prints where both logs ended up -- there's no
separate stop step to remember.

## Why the monitor re-resolves the PID every iteration

An earlier, ad hoc version of this script (used during the PR 665
experiment) resolved `osm-diffs`'s PID once at the top and never again.
When the process wasn't running yet at that exact moment -- or restarted
during the run -- every subsequent `ps -p` call just failed with
"Invalid process id" for the rest of the run, silently producing a
useless log. `monitor.sh` re-resolves it via `pgrep` on every 5-second
tick instead.

`scripts/test-on-hetzner/analyze.py` can be pointed at
`pipeline.log` from a local run here too -- the log format is identical
either way.
