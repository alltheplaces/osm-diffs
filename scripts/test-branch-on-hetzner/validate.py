"""Hard validation checks for a completed `cloud_test.py start
--containerized` run: `conflated.parquet` (read straight off the S3 test
bucket via DuckDB) and the downloaded `pipeline.log`/`dmesg.log`, checked
against the invariants `docs/outputs/CONFLATED_PARQUET.md` and
`src/pipeline/logging.rs`/`src/pipeline/mod.rs` document. Implements the
"Validation checks (`validate`)" section of issue #722's plan.

Two tiers, per that plan:

- **Hard** checks (`run_hard_checks`) can't legitimately vary run-to-run
  (a schema break is a schema break, regardless of what today's
  real-world data looks like) -- `cmd_validate` in `cloud_test.py` exits
  non-zero if any of them fail.
- **Advisory** checks (`run_advisory_checks`) are content-shaped signals
  (match rate, memory distribution) expected to drift as real data and
  matching logic evolve -- reported prominently, never fail the run.
  `CheckResult.passed` is always `True` for these except when a check is
  genuinely inapplicable (`None`, e.g. match rate on a
  `--regional-extract` run) -- there's no "fail" outcome to report by
  design.

Kept a separate module rather than folded into `cloud_test.py` itself --
that file is already sizeable, and this logic (DuckDB queries, JSONL log
parsing) is independently testable against small local fixtures without
needing any of `cloud_test.py`'s VM/SSH machinery.
"""

import json
import subprocess
import tempfile
from dataclasses import dataclass
from pathlib import Path

import duckdb

PARQUET_KEY = "conflated.parquet"

# The struct field *names* docs/outputs/CONFLATED_PARQUET.md documents
# for each nested column -- kept in sync by hand with that doc, the same
# as any other two hand-maintained descriptions of one contract (e.g.
# CONTRIBUTING.md's `!`-marker paragraph and RELEASING.md's own). Checked
# as a set of names, not full types: robust to DuckDB rendering a
# logically-unchanged type slightly differently (e.g. TIMESTAMP vs
# TIMESTAMP WITH TIME ZONE) while still catching an added, removed, or
# renamed field -- e.g. this is exactly what would have caught
# `changeset` if #731 had removed it from the writer and forgotten the
# doc, or vice versa.
EXPECTED_TOP_LEVEL = {"atp", "atp_geometry", "osm", "osm_geometry"}
EXPECTED_ATP_FIELDS = {"tags", "fetched"}
EXPECTED_ATP_FETCHED_FIELDS = {"timestamp", "spider"}
EXPECTED_OSM_FIELDS = {"type", "id", "tags", "modified", "way_members", "relation_members"}
EXPECTED_OSM_MODIFIED_FIELDS = {"timestamp", "version"}
EXPECTED_RELATION_MEMBER_FIELDS = {"type", "id", "role"}

# The CycloneDX spec version docs/outputs/CONFLATED_PARQUET.md and
# src/pipeline/provenance.rs both currently target.
CYCLONEDX_SPEC_VERSION = "1.7"

BYTE_UNITS = {"b": 1, "k": 1024, "m": 1024**2, "g": 1024**3}


@dataclass
class CheckResult:
    """One check's outcome. `passed=None` means skipped (e.g. the
    cgroup-limit check when `--mem-limit` wasn't passed to `validate`) --
    printed and counted separately from an actual pass or fail."""

    name: str
    passed: bool | None
    message: str


def parse_byte_size(text):
    """Parses a podman `--memory=` value (e.g. "4g", "512m",
    "2147483648") into a plain integer number of bytes -- the same
    binary-suffix convention podman/docker's `--memory` flag uses
    (`b`/`k`/`m`/`g`, 1024-based), matching what `cgroup_max_bytes`
    actually reads back after `podman run --memory=4g` (empirically
    confirmed during the #711 cloud smoke test: 4g -> 4294967296)."""
    text = text.strip().lower()
    if text and text[-1] in BYTE_UNITS:
        return int(text[:-1]) * BYTE_UNITS[text[-1]]
    return int(text)


def parquet_url(bucket_name):
    return f"s3://{bucket_name}/{PARQUET_KEY}"


def connect(bucket_region, access_key, secret_key):
    """A DuckDB connection with `httpfs` configured against Hetzner
    Object Storage, and `spatial` loaded for `ST_IsValid`. Extensions are
    downloaded from DuckDB's official extension repository on first use
    -- this machine already needs network access for everything else
    `cloud_test.py` does."""
    con = duckdb.connect()
    con.execute("INSTALL httpfs")
    con.execute("LOAD httpfs")
    con.execute("INSTALL spatial")
    con.execute("LOAD spatial")
    con.execute("SET s3_endpoint = ?", [f"{bucket_region}.your-objectstorage.com"])
    con.execute("SET s3_region = ?", [bucket_region])
    con.execute("SET s3_url_style = 'path'")
    con.execute("SET s3_access_key_id = ?", [access_key])
    con.execute("SET s3_secret_access_key = ?", [secret_key])
    return con


def read_pipeline_log(path):
    """Parses `pipeline.log` (JSON lines -- see `src/pipeline/logging.rs`)
    into a list of records, each `{"timestamp", "level", "target",
    "message", "fields": {...}}` ("fields" omitted on records with no
    structured key-values attached)."""
    records = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def find_step_record(records, step, phase):
    """The record for `step`'s `phase` ("start" or "end"), as logged by
    `run_step`/`log_snapshot` (`src/pipeline/mod.rs`). `None` if missing
    -- e.g. because the step never finished."""
    for r in records:
        fields = r.get("fields", {})
        if fields.get("step") == step and fields.get("phase") == phase:
            return r
    return None


def struct_field_names(duckdb_type):
    return {name for name, _ in duckdb_type.children}


def list_element_type(duckdb_type):
    # See validate.py's tests for why this shape: a LIST's only child is
    # named "child" and holds the element type, empirically confirmed
    # against this project's own DuckDB version.
    return duckdb_type.children[0][1]


def check_nonempty(con, url):
    count = con.execute("SELECT count(*) FROM read_parquet(?)", [url]).fetchone()[0]
    return CheckResult("non-empty output", count > 0, f"{count} rows")


def check_schema(con, url):
    rel = con.sql("SELECT * FROM read_parquet($1)", params=[url])
    columns = set(rel.columns)
    if columns != EXPECTED_TOP_LEVEL:
        return CheckResult(
            "schema", False, f"top-level columns {sorted(columns)} != {sorted(EXPECTED_TOP_LEVEL)}"
        )
    types = dict(zip(rel.columns, rel.types))

    atp_fields = struct_field_names(types["atp"])
    if atp_fields != EXPECTED_ATP_FIELDS:
        return CheckResult("schema", False, f"atp fields {sorted(atp_fields)} != {sorted(EXPECTED_ATP_FIELDS)}")
    atp_children = dict(types["atp"].children)
    fetched_fields = struct_field_names(atp_children["fetched"])
    if fetched_fields != EXPECTED_ATP_FETCHED_FIELDS:
        return CheckResult(
            "schema",
            False,
            f"atp.fetched fields {sorted(fetched_fields)} != {sorted(EXPECTED_ATP_FETCHED_FIELDS)}",
        )

    osm_fields = struct_field_names(types["osm"])
    if osm_fields != EXPECTED_OSM_FIELDS:
        return CheckResult("schema", False, f"osm fields {sorted(osm_fields)} != {sorted(EXPECTED_OSM_FIELDS)}")
    osm_children = dict(types["osm"].children)
    modified_fields = struct_field_names(osm_children["modified"])
    if modified_fields != EXPECTED_OSM_MODIFIED_FIELDS:
        return CheckResult(
            "schema",
            False,
            f"osm.modified fields {sorted(modified_fields)} != {sorted(EXPECTED_OSM_MODIFIED_FIELDS)}",
        )
    relation_member_fields = struct_field_names(list_element_type(osm_children["relation_members"]))
    if relation_member_fields != EXPECTED_RELATION_MEMBER_FIELDS:
        return CheckResult(
            "schema",
            False,
            f"osm.relation_members fields {sorted(relation_member_fields)} "
            f"!= {sorted(EXPECTED_RELATION_MEMBER_FIELDS)}",
        )
    return CheckResult("schema", True, "matches docs/outputs/CONFLATED_PARQUET.md")


def check_null_consistency(con, url, struct_col, geom_col):
    n = con.execute(
        f"SELECT count(*) FROM read_parquet(?) WHERE ({struct_col} IS NULL) != ({geom_col} IS NULL)",
        [url],
    ).fetchone()[0]
    return CheckResult(f"{struct_col}/{geom_col} null-consistency", n == 0, f"{n} mismatched rows")


def check_geometry_validity(con, url):
    n = con.execute(
        "SELECT count(*) FROM read_parquet(?) "
        "WHERE osm_geometry IS NOT NULL AND NOT ST_IsValid(ST_GeomFromWKB(osm_geometry))",
        [url],
    ).fetchone()[0]
    return CheckResult("osm_geometry validity", n == 0, f"{n} invalid geometries")


def check_provenance_bom(con, url, expect_pipeline_version):
    row = con.execute(
        "SELECT decode(value) FROM parquet_kv_metadata(?) WHERE key::VARCHAR = 'org.cyclonedx.bom'",
        [url],
    ).fetchone()
    if row is None:
        return CheckResult("provenance BOM", False, "no org.cyclonedx.bom key in Parquet metadata")
    bom_json = row[0]

    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        f.write(bom_json)
        bom_path = f.name
    try:
        # Same tool this repo already validates its container SBOM with
        # -- see .github/workflows/test-container.yml.
        result = subprocess.run(
            [
                "podman",
                "run",
                "--rm",
                "--volume",
                f"{Path(bom_path).parent}:/artifacts:ro",
                "cyclonedx/cyclonedx-cli",
                "validate",
                "--input-file",
                f"/artifacts/{Path(bom_path).name}",
                "--fail-on-errors",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    finally:
        Path(bom_path).unlink()
    if result.returncode != 0:
        return CheckResult("provenance BOM", False, f"cyclonedx-cli: {result.stdout}{result.stderr}")

    bom = json.loads(bom_json)
    spec_version = bom.get("specVersion")
    if spec_version != CYCLONEDX_SPEC_VERSION:
        return CheckResult(
            "provenance BOM", False, f"specVersion {spec_version!r} != {CYCLONEDX_SPEC_VERSION!r}"
        )
    if expect_pipeline_version:
        actual = bom.get("metadata", {}).get("tools", {}).get("components", [{}])[0].get("version")
        if actual != expect_pipeline_version:
            return CheckResult(
                "provenance BOM",
                False,
                f"pipeline_version {actual!r} != expected {expect_pipeline_version!r}",
            )
    return CheckResult("provenance BOM", True, "valid CycloneDX, spec version matches")


def check_run_completed(records):
    end = find_step_record(records, "conflate", "end")
    if end is None:
        return CheckResult("run completed", False, "no conflate/end record in pipeline.log")
    start = find_step_record(records, "conflate", "start")
    start_index = records.index(start) if start else 0
    errors = [
        r for r in records[start_index:] if r.get("level") == "ERROR"
    ]
    if errors:
        messages = "; ".join(r["message"] for r in errors)
        return CheckResult("run completed", False, f"{len(errors)} ERROR record(s): {messages}")
    return CheckResult("run completed", True, "conflate/end reached with no ERROR records")


def check_cgroup_signal(records, mem_limit):
    end = find_step_record(records, "conflate", "end")
    if end is None:
        return CheckResult("cgroup signal", False, "no conflate/end record in pipeline.log")
    fields = end.get("fields", {})
    current = fields.get("cgroup_current_bytes")
    maximum = fields.get("cgroup_max_bytes")
    if current is None or maximum is None:
        return CheckResult(
            "cgroup signal",
            False,
            "cgroup_current_bytes/cgroup_max_bytes are null -- this run wasn't containerized "
            "under a real cgroup memory limit",
        )
    if mem_limit is not None:
        expected = parse_byte_size(mem_limit)
        if maximum != expected:
            return CheckResult(
                "cgroup signal", False, f"cgroup_max_bytes={maximum} != --mem-limit {mem_limit} ({expected} bytes)"
            )
    return CheckResult("cgroup signal", True, f"cgroup_current_bytes={current} cgroup_max_bytes={maximum}")


# Kernel log phrasing this checks for, across dmesg's and journalctl's
# slightly different wordings for the same event -- both mention
# "out of memory" and "kill" on the offending line either way.
OOM_SIGNATURES = ("out of memory", "oom-kill", "oom_kill", "killed process")


def check_no_oom(dmesg_text):
    hits = [
        line
        for line in dmesg_text.splitlines()
        if any(sig in line.lower() for sig in OOM_SIGNATURES)
    ]
    if hits:
        return CheckResult("no OOM", False, f"{len(hits)} OOM-looking kernel log line(s): {hits[0]}")
    return CheckResult("no OOM", True, "no OOM-kill signature in dmesg/journalctl -k")


def check_atp_geometry_floor(records, min_features):
    record = next(
        (
            r
            for r in records
            if r.get("message") == "import_atp: alltheplaces.parquet geometry types"
        ),
        None,
    )
    if record is None:
        return CheckResult("ATP geometry floor", False, "no import_atp geometry-tally record in pipeline.log")
    fields = record.get("fields", {})
    types = ("point", "line_string", "polygon", "multi_point", "multi_line_string", "multi_polygon", "geometry_collection")
    total = sum(fields.get(t, 0) for t in types)
    if min_features is None:
        return CheckResult("ATP geometry floor", None, f"{total} ATP geometries imported (no --min-atp-features given)")
    if total < min_features:
        return CheckResult("ATP geometry floor", False, f"only {total} ATP geometries imported, expected >= {min_features}")
    return CheckResult("ATP geometry floor", True, f"{total} ATP geometries imported")


def check_match_rate(con, url, regional_extract):
    if regional_extract:
        # AllThePlaces is always worldwide -- a regional OSM extract
        # shows ~0% match outside its region by design, not by defect.
        return CheckResult("match rate", None, "skipped (regional extract)")
    total, matched = con.execute(
        "SELECT count(*), count(*) FILTER (WHERE atp IS NOT NULL AND osm IS NOT NULL) "
        "FROM read_parquet(?)",
        [url],
    ).fetchone()
    rate = matched / total if total else 0.0
    return CheckResult("match rate", True, f"{matched}/{total} rows matched ({rate:.1%})")


def check_memory_distribution(records):
    # The mmap/page-cache design's own signal (#711): the "conflate" step
    # is the only one with fine-grained enough logging to read this from
    # -- there's no separate "conflate.match" log_snapshot record, just a
    # progress-bar label (src/pipeline/conflate/mod.rs), so the step's
    # own end-of-step snapshot is the closest available granularity.
    end = find_step_record(records, "conflate", "end")
    fields = end.get("fields", {}) if end else {}
    file_bytes = fields.get("rss_file_bytes")
    anon_bytes = fields.get("rss_anon_bytes")
    shmem_bytes = fields.get("rss_shmem_bytes")
    if file_bytes is None or anon_bytes is None:
        return CheckResult("memory distribution", True, "rss_file_bytes/rss_anon_bytes unavailable")
    ratio = file_bytes / anon_bytes if anon_bytes else float("inf")
    note = ""
    if shmem_bytes and shmem_bytes > file_bytes:
        note = " -- rss_shmem_bytes exceeds rss_file_bytes, worth a look for a tmpfs workdir misconfiguration"
    return CheckResult(
        "memory distribution",
        True,
        f"rss_file_bytes={file_bytes} rss_anon_bytes={anon_bytes} rss_shmem_bytes={shmem_bytes} "
        f"(file/anon ratio={ratio:.2f}){note}",
    )


def check_cgroup_warnings(records):
    # The 85%-of-limit WARN log_snapshot self-logs (CGROUP_WARN_THRESHOLD
    # in src/pipeline/mod.rs) -- an early signal ahead of an actual
    # OOM-kill, worth surfacing even on a run that never hit one.
    hits = [r for r in records if r.get("level") == "WARN" and "cgroup_usage_fraction" in r.get("fields", {})]
    if not hits:
        return CheckResult("cgroup 85% warnings", True, "none logged")
    lines = [
        f"{r['fields'].get('step')}/{r['fields'].get('phase')}={r['fields']['cgroup_usage_fraction']:.0%}"
        for r in hits
    ]
    return CheckResult("cgroup 85% warnings", True, f"{len(hits)} logged: {', '.join(lines)}")


def check_disk_headroom(disk_log_text):
    # disk.log lines: "YYYY-MM-DD HH:MM:SS <used_bytes> <avail_bytes>"
    # (remote/monitor.sh's df -B1 --output=used,avail sampling loop).
    lines = [line for line in disk_log_text.splitlines() if line.strip()]
    if not lines:
        return CheckResult("disk headroom", True, "no disk.log samples available")
    used, avail = (int(x) for x in lines[-1].split()[-2:])
    total = used + avail
    fraction = used / total if total else 0.0
    return CheckResult("disk headroom", True, f"used={used} avail={avail} ({fraction:.1%} full)")


def check_timings(records):
    ends = [r for r in records if r.get("fields", {}).get("phase") == "end" and r["fields"].get("elapsed_seconds") is not None]
    if not ends:
        return CheckResult("timings", True, "no step timings available")
    total = sum(r["fields"]["elapsed_seconds"] for r in ends)
    per_step = ", ".join(f"{r['fields']['step']}={r['fields']['elapsed_seconds']:.1f}s" for r in ends)
    return CheckResult("timings", True, f"total={total:.1f}s ({per_step})")


def check_osm_geometry_count(records):
    record = next((r for r in records if r.get("message") == "conflate.write: osm_geometry geometry types"), None)
    if record is None:
        return CheckResult("OSM geometry count", True, "no conflate.write osm_geometry tally in pipeline.log")
    fields = record.get("fields", {})
    types = ("point", "line_string", "polygon", "multi_point", "multi_line_string", "multi_polygon", "geometry_collection")
    total = sum(fields.get(t, 0) for t in types)
    return CheckResult("OSM geometry count", True, f"{total} matched OSM geometries")


def run_advisory_checks(con, url, records, disk_log_text, regional_extract):
    """Runs every advisory check and returns the list of `CheckResult`s,
    in the order printed. Never fails `validate` by itself -- see the
    module docstring for why."""
    return [
        check_match_rate(con, url, regional_extract),
        check_memory_distribution(records),
        check_cgroup_warnings(records),
        check_disk_headroom(disk_log_text),
        check_timings(records),
        check_osm_geometry_count(records),
    ]


def run_hard_checks(con, url, records, dmesg_text, mem_limit, expect_pipeline_version, min_atp_features):
    """Runs every hard check and returns the list of `CheckResult`s, in
    the order printed. Doesn't short-circuit on the first failure --
    `cmd_validate` wants the full picture in one pass, not a
    fix-one-rerun-find-the-next loop."""
    return [
        check_nonempty(con, url),
        check_schema(con, url),
        check_null_consistency(con, url, "atp", "atp_geometry"),
        check_null_consistency(con, url, "osm", "osm_geometry"),
        check_geometry_validity(con, url),
        check_provenance_bom(con, url, expect_pipeline_version),
        check_run_completed(records),
        check_cgroup_signal(records, mem_limit),
        check_no_oom(dmesg_text),
        check_atp_geometry_floor(records, min_atp_features),
    ]
