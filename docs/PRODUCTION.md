# Running `osm-diffs` in production

`osm-diffs` doesn’t run anywhere permanent yet — see
[`TECHNICAL_DESIGN.md`](TECHNICAL_DESIGN.md)’s “Status” section. This
document isn’t a description of an existing deployment; it’s the
operational knowledge gathered from testing on Hetzner Cloud
([`scripts/test-on-hetzner`](../scripts/test-on-hetzner/README.md),
[#711](https://github.com/alltheplaces/osm-diffs/issues/711),
[#722](https://github.com/alltheplaces/osm-diffs/issues/722)), written
down now so whoever sets up the actual deployment doesn’t have to
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
never looser. **8GB is the recommended minimum** in production: the
first config in the sweep with no measurable difference from a
generous limit.

**Disk**: peak usage during a full-planet run was **~172GB**, settling
to **~143GB** once `import_osm` finishes and its temporary files are
cleaned up — consistent across two independent runs on different
hardware (this sweep, and the earlier
[#665](https://github.com/alltheplaces/osm-diffs/issues/665) shakedown,
which saw ~174GB peak / ~148GB settled). **220–250GB** gives reasonable
headroom over that peak without being wildly oversized; the 400GB
default `scripts/test-on-hetzner/cloud_test.py` uses for testing is
itself generous margin, not a sizing recommendation.

## Container invocation

The published image (`ghcr.io/alltheplaces/osm-diffs:vX.Y.Z` — pin an
exact tag; see
[the releases page](https://github.com/alltheplaces/osm-diffs/releases)
for the current version and its notes) runs as:

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
- `--memory`/`--cpus`: the actual cgroup limits this document’s sizing
  guidance is about — without them, `pipeline.log`’s own `cgroup_*`
  memstats fields read `None` (see
  [`LOGGING.md`](LOGGING.md)), and nothing here has been validated
  running unconstrained.
- `--run_id`: whatever identifier the scheduling system assigns this
  run (a Kubernetes Job name, a cron invocation ID, …). It becomes
  `formulation[].workflows[].uid` in the output’s embedded provenance
  BOM (see
  [`outputs/CONFLATED_PARQUET.md`](outputs/CONFLATED_PARQUET.md)), keys
  the run’s archived `pipeline.log`, and pins the workdir against a
  mismatched restart. Anything outside `[A-Za-z0-9._-]` is normalised
  for those uses (see [`LOGGING.md`](LOGGING.md)); pass a clean value
  and it’s used verbatim. Optional; empty if omitted.

## Required configuration

Uploads go to **two** independent S3 destinations, each with its own
five environment variables, read once at startup (see
[`src/pipeline/upload.rs`](../src/pipeline/upload.rs)):

| Prefix | Holds | Intended backing store |
|---|---|---|
| `PUBLIC_S3_*` | `conflated.parquet`, the PMTiles archives — the public downloads | a Bunny.net S3 instance, so objects replicate to the CDN edge |
| `INTERNAL_S3_*` | `logs/<run-id>.log`, and future intermediates | in-datacenter object storage (e.g. Hetzner Object Storage); never needs CDN replication |

For each prefix:

- `<prefix>_ENDPOINT` — also that bucket's on/off switch: unset it
  entirely to disable those uploads (e.g. a local/dry-run invocation, or
  a run that only needs one of the two buckets) rather than passing
  empty values.
- `<prefix>_BUCKET`, `<prefix>_REGION`, `<prefix>_ACCESS_KEY_ID`,
  `<prefix>_ACCESS_KEY_SECRET` — required once `<prefix>_ENDPOINT` is
  set; genuine S3-compatible buckets for the actual output, never the
  ephemeral per-run test buckets `scripts/test-on-hetzner` creates for
  its own testing.

`*_ACCESS_KEY_ID`/`*_ACCESS_KEY_SECRET` are live credentials — handle
them as secrets, not plain configuration. Keep them out of a command
line (visible to anyone who can `ps` the host, and easy to leak into
shell history or CI logs) and out of any manifest committed to a repo.
The `podman run --env-file` invocation above already does the
minimum right thing for a manual/systemd invocation — the file’s
contents never appear as process arguments. Whatever eventually
schedules this in production should do the equivalent for its own
environment: a Kubernetes `Secret` referenced via `secretKeyRef` (not
a literal value in the `Job`/`CronJob` manifest), or the analogous
mechanism for whatever scheduler ends up being used.

## Scheduling

Nothing here runs on a schedule yet. The design intent
([`TECHNICAL_DESIGN.md`](TECHNICAL_DESIGN.md#objective)) is a weekly
cadence, matching how often AllThePlaces itself publishes a fresh dump
— whatever wires this up (a Kubernetes `CronJob`, a plain cron job
somewhere, a scheduled GitHub Actions workflow) is future work, not
something this repository provides today.

## Serving the outputs (CDN)

Nothing serves the pipeline’s outputs to the public yet. The intent is
to put them behind the same [Bunny](https://bunny.net) CDN that
[`osmviews`](https://github.com/brawer/osmviews) uses, sharing one
account and one OpenTofu config
([`brawer/production`](https://github.com/brawer/production), `bunny/`).
The CDN and its edge-cache rules for `osmdiffs` are **already
provisioned** there — currently against a staging hostname
(`osmdiffs.dandelis.ch`, to be moved to a permanent one later) — so this
section is about the shape the pipeline’s uploads must take, not infra
still to build.

### The `/data/` contract

Bunny serves objects straight from the storage bucket and cannot attach
per-object response headers, so caching is driven entirely by URL
patterns in
[`bunny/cdn.tf`](https://github.com/brawer/production/blob/main/bunny/cdn.tf).
Two rules apply under `https://<host>/data/`:

- **`data/datapackage.json` — 60 s TTL.** The one object allowed to
  change at a stable URL: a
  [Frictionless Data Package](https://datapackage.org/) descriptor,
  overwritten in place every run, listing that run’s files with byte
  sizes and SHA-256 hashes. It is the update-check target — a client
  GETs ~1 KB and compares `version`. 60 s (a little more across cache
  tiers) is then the whole propagation delay of a new run; **the
  pipeline never purges the CDN** and never needs a Bunny account
  credential.

- **everything else under `data/` — long TTL**
  (`local.immutable_max_age` in `cdn.tf`: 600 s today, moving to 30
  days). Matched as a *negative* rule — “under `data/` and not
  `datapackage.json`” — so new file types need no config change.

That negative match is the load-bearing invariant:

> **`data/datapackage.json` is the only object the pipeline may
> overwrite. Every other published file must carry an immutable URL** —
> a date or content hash in the name (`conflated-<date>.parquet`,
> `conflated-<date>.pmtiles`, the dated CycloneDX BOM, …).

Reusing a name for changed bytes would serve stale data from cache for
up to the long TTL, recoverable only with a manual purge. Upload the
dated files first and `datapackage.json` last, so “the manifest lists
file X” always implies X is already there.

Pruning old runs to save storage is fine — a deleted dated object just
starts returning 404, and the CDN caches that 404 correctly. Replacing
a published object is not.

### Still open

- Whether `osmdiffs` adopts a `datapackage.json` manifest at all, or a
  lighter scheme — tracked with the `osmviews` work in
  [brawer/osmviews#110](https://github.com/brawer/osmviews/issues/110).
- CORS headers on `/data/*`, needed if a browser reads the outputs
  directly —
  [brawer/production#10](https://github.com/brawer/production/issues/10).
- The dedicated public bucket now exists (`PUBLIC_S3_*`, above,
  separate from the internal `INTERNAL_S3_*` one). Still missing: the
  `data/` key prefix and the immutable (dated/hashed) object names the
  CDN contract requires — the pipeline still writes flat, mutable keys
  (`conflated.parquet`, …).

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
  larger instance chosen for genuine margin beyond this document’s
  bare sizing numbers.

This is Hetzner-specific pricing, from the same provider
`scripts/test-on-hetzner` tests against — an actual production
deployment might land on different infrastructure entirely; treat the
euro figures as an order-of-magnitude anchor, not a quote.
