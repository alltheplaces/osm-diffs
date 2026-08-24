# Logging

`osm-diffs run` writes `pipeline.log` to its `--workdir`, one JSON
object per line ([JSON Lines](https://jsonlines.org/)) —
machine-readable rather than free-form text, so a log can be grepped,
`jq`’d, or loaded into whatever analysis tool you like without first
having to parse a human-oriented formatter’s line-wrapping and colors.
Every record has:

- `timestamp` — RFC 3339, UTC.
- `level` — `INFO`, `WARN`, `ERROR`, …
- `target` — which module logged it (e.g. `osm_diffs::pipeline::conflate`).
- `message` — the human-readable text.
- `fields` — present only on records that attach structured data
  (numbers, an id, …) instead of just interpolating it into the
  message text, e.g. a pipeline step’s elapsed time and memory
  snapshot. Omitted entirely on records that don’t have any, so plain
  log lines stay plain.

See [`src/pipeline/logging.rs`](../src/pipeline/logging.rs) for the
implementation, and
[`src/pipeline/mod.rs`](../src/pipeline/mod.rs)’s `log_snapshot` for an
example of a structured record (step name, phase, elapsed time, RSS/
cgroup memory snapshot — logged at the start and end of every pipeline
step). A step can log its own internal sub-steps the same way, under a
dotted name (e.g. `import_osm.fetch`, `import_osm.assemble`) — not a
second logging mechanism, the exact same `run_step` helper, just called
again from inside a step for finer timing resolution than that step’s
own start/end pair alone would give. `import_osm` does this for its
fetch/open/prune/assemble/index-build phases; see
[`src/pipeline/osm/mod.rs`](../src/pipeline/osm/mod.rs).

## Where weekly-run logs end up

Every pipeline run uploads its `pipeline.log` to S3 storage at
`logs/<run-id>.log`, where `<run-id>` is the run’s start timestamp.
That timestamp is effectively each production run’s own ID — for
traceability, it’s also stamped into `conflated.parquet`’s own metadata
(see [`outputs/CONFLATED_PARQUET.md`](outputs/CONFLATED_PARQUET.md)),
so a run’s log and its data output can always be tied back together.
This happens regardless of whether the run succeeded — a failed run’s
log is exactly the one you want archived for debugging, not just a
successful one’s.

## Why bother

Beyond debugging a single run, having every week’s log archived means
you can compare stats *across* runs over time — memory/disk usage as
the planet grows, how a code change shifted step timings, that kind of
thing. Two concrete examples of what this data enables, both written by
[Claude Code](https://claude.com/claude-code) straight from a run’s
logs, with no other tooling built for the purpose:

- [alltheplaces/osm-diffs#665, comment](https://github.com/alltheplaces/osm-diffs/pull/665#issuecomment-5303068423) —
  a full memory/disk/timing analysis of a full-planet run on
  memory-constrained hardware, validating the design assumption behind
  `OsmFeatureIndex` (relying on the OS page cache instead of an
  explicit decode cache).
- [alltheplaces/osm-diffs#636](https://github.com/alltheplaces/osm-diffs/issues/636) —
  a survey of OpenStreetMap data-quality issues, found by combing
  through `pipeline.log`’s `could not build geometry` warnings and
  cross-referencing the flagged features against the live OSM API.

Neither of those needed bespoke analysis code — just the JSON logs
already described above, and enough of them archived to look back at.
