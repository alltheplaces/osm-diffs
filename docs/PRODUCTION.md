# Running `osm-diffs` in production

`osm-diffs` doesn’t run anywhere permanent yet — see
[`TECHNICAL_DESIGN.md`](TECHNICAL_DESIGN.md)’s “Status” section. This
document isn’t a description of an existing deployment; it’s the
operational knowledge gathered from real testing on Hetzner Cloud
([`scripts/test-on-hetzner`](../scripts/test-on-hetzner/README.md),
[#711](https://github.com/alltheplaces/osm-diffs/issues/711),
[#722](https://github.com/alltheplaces/osm-diffs/issues/722)), written
down now so whoever sets up the real thing doesn’t have to
re-establish it from scratch.

## Hardware sizing

**CPU**: every number below comes from a fixed `--cpus=6` (a Hetzner
`cpx42`) — CPU count itself hasn’t been swept independently, so “how
many CPUs” is still an open question, not a documented recommendation.

**Memory**: a `--mem-limit` sweep of the released `v0.8.2` container
against the full OpenStreetMap planet, same CPU count throughout,
gave:

| `--mem-limit` | `import_osm` | `conflate` | Total |
|---|---|---|---|
| 12g | 2h24m12s | 10m37s | 2h43m2s |
| 8g | 2h24m24s | 10m18s | 2h43m20s |
| 6g | 2h27m53s | 10m17s | 2h46m28s |
| 4g | 4h02m46s | 10m36s | 4h21m36s |

**6GB is the measured floor** on this CPU count: 8g and 6g are
indistinguishable from a comfortable 12g baseline; 4g still completes
correctly (identical row counts, all `validate` hard checks pass — the
memory pressure costs wall-clock time, not correctness) but takes
~1h40m longer, entirely inside `import_osm`’s node-coordinate
resolution step, not `conflate` — see
[`TECHNICAL_DESIGN.md`](TECHNICAL_DESIGN.md#why-conflate-doesnt-need-its-own-cache)
for why those two steps behave so differently under memory pressure.

**Don’t provision at the measured floor.** 6GB is where the sweep
happened to land this time; it isn’t a target with margin built in, and
the planet only grows over time — a floor measured today gets tighter,
never looser. **8GB is the recommended minimum** for real use: the
first config in the sweep with no measurable difference from a
generous limit.

**Disk**: peak usage during a full-planet run was **~172GB**, settling
to **~143GB** once `import_osm` finishes and its temporary files are
cleaned up — consistent across two independent runs on different
hardware (this sweep, and the earlier
[#665](https://github.com/alltheplaces/osm-diffs/issues/665) shakedown,
which saw ~174GB peak / ~148GB settled). **220-250GB** gives reasonable
headroom over that peak without being wildly oversized; the 400GB
default `scripts/test-on-hetzner/cloud_test.py` uses for testing is
itself generous margin, not a sizing recommendation.

## Container invocation

The published image (`ghcr.io/alltheplaces/osm-diffs:vX.Y.Z` — pin an
exact, immutable tag; see
[`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md#immutable-releases))
runs as:

```sh
podman run --rm --read-only \
  --memory=8g --cpus=6 \
  -v /path/to/workdir:/workdir \
  --env-file s3.env \
  ghcr.io/alltheplaces/osm-diffs:vX.Y.Z \
  run --workdir /workdir --run_id "$RUN_ID"
```

- `--read-only`: the image ships nothing that needs a writable root
  filesystem — see
  [`SUPPLY_CHAIN_SECURITY.md`](SUPPLY_CHAIN_SECURITY.md#minimal-containers).
  Everything the pipeline writes goes to `/workdir`, the one mounted
  volume.
- `--memory`/`--cpus`: the real cgroup limits this document’s sizing
  guidance is about — without them, `pipeline.log`’s own `cgroup_*`
  memstats fields read `None` (see
  [`LOGGING.md`](LOGGING.md)), and nothing here has been validated
  running unconstrained.
- `--run_id`: becomes `formulation[].workflows[].uid` in the output’s
  embedded provenance BOM (see
  [`outputs/CONFLATED_PARQUET.md`](outputs/CONFLATED_PARQUET.md)) —
  whatever identifier the scheduling system assigns this run (a
  Kubernetes Job name, a cron invocation ID, …). Optional; empty if
  omitted.

## Required configuration

Five environment variables, read once at startup (see
[`src/pipeline/upload.rs`](../src/pipeline/upload.rs)):

- `S3_ENDPOINT` — also the on/off switch: unset it entirely to disable
  uploads (e.g. for a local/dry-run invocation) rather than passing
  empty values.
- `S3_BUCKET`, `S3_REGION`, `S3_ACCESS_KEY_ID`, `S3_ACCESS_KEY_SECRET`
  — required once `S3_ENDPOINT` is set; a real S3-compatible bucket for
  the actual output (`conflated.parquet`, `edits.pmtiles`,
  `logs/<run-id>.log`), never the ephemeral per-run test buckets
  `scripts/test-on-hetzner` creates for its own testing.

## Scheduling

Nothing here runs on a schedule yet. The design intent
([`TECHNICAL_DESIGN.md`](TECHNICAL_DESIGN.md#objective)) is a weekly
cadence, matching how often AllThePlaces itself publishes a fresh dump
— whatever wires this up (a Kubernetes `CronJob`, a plain cron job
somewhere, a scheduled GitHub Actions workflow) is future work, not
something this repository provides today.

## What to watch

Everything below comes straight out of `pipeline.log` (see
[`LOGGING.md`](LOGGING.md)) — no separate monitoring system needed to
get started:

- **OOM**: exit code 137, or an OOM-kill signature in `dmesg`/
  `journalctl -k` — the definitive failure signal. A killed step’s own
  “end” log record is simply missing, not an error entry, so don’t
  rely on `pipeline.log` alone to notice this; check the container’s
  own exit status too.
- **Approaching the limit**: a `WARN` fires automatically once
  `cgroup_current_bytes` crosses 85% of `cgroup_max_bytes`, at any
  step — an early signal ahead of an actual kill.
- **The page-cache design holding up**: during `conflate.match`
  specifically, `rss_file_bytes` should dominate `rss_bytes` (reclaimable
  cache, not heap) — if that ratio drops, something’s changed about the
  access pattern this design depends on.
- **Step timings drifting**: `import_osm`’s own sub-steps
  (`import_osm.fetch`/`.open`/`.prune`/`.assemble`/`.index`, logged
  individually as of
  [#761](https://github.com/alltheplaces/osm-diffs/pull/761)) are worth
  watching over time as the planet grows — a slow drift is expected;
  a sudden jump on an otherwise-unchanged config is worth investigating
  the way this document’s own `--mem-limit` sweep did.

## Cost

Real Hetzner Cloud pricing as of 2026-08 (`cpx42`, `fsn1`/`hel1`,
excluding VAT): **€0.1114/hour**. A weekly full-planet run at the
recommended 8GB config takes ~2h43m, so:

- **~€0.30 per run** in compute, plus a few cents of volume cost for
  its few-hour lifetime (volumes bill per GB-month;
  `€0.0572`/GB-month, negligible unless kept around persistently
  between runs).
- **~€1.30/month** at a weekly cadence — compute only. Not included:
  S3-compatible storage for the actual output, egress/traffic, or any
  larger instance chosen for real margin beyond this document’s bare
  sizing numbers.

This is Hetzner-specific pricing, from the same provider
`scripts/test-on-hetzner` tests against — a real production deployment
might land on different infrastructure entirely; treat the euro
figures as an order-of-magnitude anchor, not a quote.
