# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

"""Tests for validate.py. Runs real DuckDB queries against small Parquet
fixtures built on the fly (rather than mocking the query engine itself,
which would just be re-asserting the SQL string instead of actually
exercising it) -- no S3/network/podman calls: fixtures are plain local
files, and the one check that does shell out (check_provenance_bom's
cyclonedx-cli call) has `subprocess.run` mocked.

One test (or one `parametrize`d test) per piece of actual logic, not one
per trivial branch -- a wrong `CheckResult.passed` on a check with
nothing to get wrong isn't the risk worth guarding against here.
"""

import json

import duckdb
import pytest
import validate as v

# A single valid row matching docs/outputs/CONFLATED_PARQUET.md's
# schema -- a matched ATP/OSM pair. Used as the default fixture content;
# individual tests override only what they need to.
VALID_ROW_SQL = """
SELECT
    {
        'tags': MAP(['shop'], ['bakery']),
        'fetched': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'spider': 'example_spider'}
    } AS atp,
    ST_Point(8.5, 47.4) AS atp_geometry,
    {
        'type': 'node',
        'id': 12345::UBIGINT,
        'tags': MAP(['shop'], ['bakery']),
        'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER},
        'way_members': NULL::UBIGINT[],
        'relation_members': NULL::STRUCT("type" VARCHAR, id UBIGINT, "role" VARCHAR)[]
    } AS osm,
    ST_Point(8.5, 47.4) AS osm_geometry
"""
# Native GEOMETRY, not WKB bytes cast to BLOB: docs/outputs/CONFLATED_PARQUET.md
# documents atp_geometry/osm_geometry as WKB, but DuckDB's spatial extension
# auto-decodes the real column (which carries the native Parquet GEOGRAPHY
# logical type) straight to GEOMETRY on read -- matching that here, not the
# documented on-disk representation, is what makes these fixtures behave the
# same way validate.py's checks see the real file behave.


def make_parquet(tmp_path, select_sql, name="conflated.parquet", kv_metadata=None):
    """Writes `select_sql`'s result to a real local Parquet file (loading
    the `spatial` extension first, since the fixture rows use ST_Point)
    and returns its path as a `str`, usable directly with
    `read_parquet()`."""
    con = duckdb.connect()
    con.execute("INSTALL spatial")
    con.execute("LOAD spatial")
    path = tmp_path / name
    kv = ""
    if kv_metadata:
        pairs = ", ".join(f"'{key}': '{value}'" for key, value in kv_metadata.items())
        kv = f", KV_METADATA {{{pairs}}}"
    con.execute(f"COPY ({select_sql}) TO '{path}' (FORMAT PARQUET{kv})")
    return con, str(path)


def fake_run(returncode=0, stdout="", stderr=""):
    """A `subprocess.run` stand-in for `check_provenance_bom`'s
    `cyclonedx-cli` call -- just the `.returncode`/`.stdout`/`.stderr`
    attributes that call site reads."""
    return lambda *a, **k: type("R", (), {"returncode": returncode, "stdout": stdout, "stderr": stderr})()


def _conflate_end(**fields):
    return {"level": "INFO", "message": "conflate: end", "fields": {"step": "conflate", "phase": "end", **fields}}


def _atp_tally_record(**counts):
    fields = dict.fromkeys(
        ("point", "line_string", "polygon", "multi_point", "multi_line_string", "multi_polygon", "geometry_collection"),
        0,
    )
    fields.update(counts)
    return {"message": "import_atp: alltheplaces.parquet geometry types", "fields": fields}


def _bom(pipeline_version="0.8.0", spec_version="1.7"):
    return json.dumps(
        {"specVersion": spec_version, "metadata": {"tools": {"components": [{"version": pipeline_version}]}}}
    )


# ── parse_byte_size / read_pipeline_log ──────────────────────────────


@pytest.mark.parametrize(
    "text,expected",
    [
        ("4g", 4 * 1024**3),
        ("512m", 512 * 1024**2),
        ("2k", 2 * 1024),
        ("100b", 100),
        ("2147483648", 2147483648),
        ("4G", 4 * 1024**3),
    ],
)
def test_parse_byte_size(text, expected):
    assert v.parse_byte_size(text) == expected


def test_read_pipeline_log_parses_jsonl_and_skips_blank_lines(tmp_path):
    log = tmp_path / "pipeline.log"
    log.write_text('{"message": "a"}\n\n{"message": "b"}\n')
    assert [r["message"] for r in v.read_pipeline_log(log)] == ["a", "b"]


# ── check_nonempty / check_null_consistency / check_geometry_validity ─


@pytest.mark.parametrize("select_suffix,expected", [("", True), (" WHERE false", False)])
def test_check_nonempty(tmp_path, select_suffix, expected):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL + select_suffix)
    assert v.check_nonempty(con, url).passed is expected


@pytest.mark.parametrize(
    "sql,expected",
    [
        (VALID_ROW_SQL, True),
        (
            VALID_ROW_SQL.replace("ST_Point(8.5, 47.4) AS atp_geometry,", "NULL::GEOMETRY AS atp_geometry,"),
            False,
        ),
    ],
    ids=["consistent", "mismatched"],
)
def test_check_null_consistency(tmp_path, sql, expected):
    con, url = make_parquet(tmp_path, sql)
    assert v.check_null_consistency(con, url, "atp", "atp_geometry").passed is expected


@pytest.mark.parametrize(
    "geometry_sql,expected",
    [
        ("ST_Point(8.5, 47.4)", True),
        # A self-intersecting ("bowtie") polygon -- the textbook invalid geometry.
        ("ST_GeomFromText('POLYGON((0 0, 2 2, 2 0, 0 2, 0 0))')", False),
    ],
)
def test_check_geometry_validity(tmp_path, geometry_sql, expected):
    sql = VALID_ROW_SQL.replace("ST_Point(8.5, 47.4) AS osm_geometry", f"{geometry_sql} AS osm_geometry")
    con, url = make_parquet(tmp_path, sql)
    assert v.check_geometry_validity(con, url).passed is expected


# ── check_schema ─────────────────────────────────────────────────────


def test_check_schema_passes_for_documented_schema(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    assert v.check_schema(con, url).passed is True


# The regression this check exists for: #731 removed `changeset` from
# osm.modified -- if it ever came back, this must fail.
_SCHEMA_WITH_CHANGESET = VALID_ROW_SQL.replace(
    "'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER},",
    "'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER, "
    "'changeset': 99::UBIGINT},",
)


@pytest.mark.parametrize(
    "sql,expected_snippet",
    [
        (_SCHEMA_WITH_CHANGESET, "changeset"),
        ("SELECT NULL::STRUCT(tags MAP(VARCHAR, VARCHAR)) AS atp", "top-level"),
    ],
    ids=["changeset-reappears", "missing-top-level-column"],
)
def test_check_schema_fails(tmp_path, sql, expected_snippet):
    con, url = make_parquet(tmp_path, sql)
    result = v.check_schema(con, url)
    assert result.passed is False
    assert expected_snippet in result.message


# ── check_provenance_bom ─────────────────────────────────────────────


def test_check_provenance_bom_passes(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", fake_run())
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom()})
    assert v.check_provenance_bom(con, url, expect_pipeline_version=None).passed is True


def test_check_provenance_bom_fails_when_key_missing(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_provenance_bom(con, url, expect_pipeline_version=None)
    assert result.passed is False
    assert "org.cyclonedx.bom" in result.message


def test_check_provenance_bom_fails_when_cyclonedx_cli_rejects_it(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", fake_run(returncode=1, stdout="invalid"))
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom()})
    assert v.check_provenance_bom(con, url, expect_pipeline_version=None).passed is False


def test_check_provenance_bom_fails_on_pipeline_version_mismatch(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", fake_run())
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom(pipeline_version="0.7.0")})
    result = v.check_provenance_bom(con, url, expect_pipeline_version="0.8.0")
    assert result.passed is False
    assert "0.7.0" in result.message


# ── check_run_completed ──────────────────────────────────────────────


def test_check_run_completed_passes_when_conflate_ends_cleanly():
    records = [
        {"level": "INFO", "message": "conflate: start", "fields": {"step": "conflate", "phase": "start"}},
        {"level": "INFO", "message": "conflate: end", "fields": {"step": "conflate", "phase": "end"}},
    ]
    assert v.check_run_completed(records).passed is True


def test_check_run_completed_fails_when_conflate_end_missing():
    records = [{"level": "INFO", "message": "conflate: start", "fields": {"step": "conflate", "phase": "start"}}]
    assert v.check_run_completed(records).passed is False


def test_check_run_completed_fails_on_error_after_start():
    records = [
        {"level": "INFO", "message": "conflate: start", "fields": {"step": "conflate", "phase": "start"}},
        {"level": "ERROR", "message": "conflate failed: boom", "fields": {"step": "conflate"}},
        {"level": "INFO", "message": "conflate: end", "fields": {"step": "conflate", "phase": "end"}},
    ]
    result = v.check_run_completed(records)
    assert result.passed is False
    assert "boom" in result.message


# ── check_cgroup_signal ───────────────────────────────────────────────


@pytest.mark.parametrize(
    "fields,mem_limit,expected",
    [
        ({"cgroup_current_bytes": 1000, "cgroup_max_bytes": 4 * 1024**3}, "4g", True),
        ({}, "4g", False),  # cgroup fields null -- not really containerized
        ({"cgroup_current_bytes": 1000, "cgroup_max_bytes": 2 * 1024**3}, "4g", False),  # != --mem-limit
        ({"cgroup_current_bytes": 1000, "cgroup_max_bytes": 2 * 1024**3}, None, True),  # no --mem-limit: skip compare
    ],
    ids=["matching", "fields-null", "mem-limit-mismatch", "no-mem-limit-given"],
)
def test_check_cgroup_signal(fields, mem_limit, expected):
    assert v.check_cgroup_signal([_conflate_end(**fields)], mem_limit).passed is expected


def test_check_cgroup_signal_fails_when_conflate_end_missing():
    assert v.check_cgroup_signal([], mem_limit="4g").passed is False


# ── check_no_oom ──────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "dmesg_text,expected",
    [
        ("kernel: some unrelated line\n", True),
        ("kernel: Out of memory: Killed process 123 (osm-diffs)\n", False),
    ],
)
def test_check_no_oom(dmesg_text, expected):
    assert v.check_no_oom(dmesg_text).passed is expected


# ── check_atp_geometry_floor ─────────────────────────────────────────


@pytest.mark.parametrize(
    "records,min_features,expected",
    [
        ([_atp_tally_record(point=1000)], 500, True),
        ([_atp_tally_record(point=10)], 500, False),
        ([_atp_tally_record(point=10)], None, None),  # no floor given: skipped, not silently passed
        ([], 500, False),  # no tally record at all
    ],
    ids=["above-floor", "below-floor", "no-floor-given", "no-tally-record"],
)
def test_check_atp_geometry_floor(records, min_features, expected):
    assert v.check_atp_geometry_floor(records, min_features).passed is expected


# ── run_hard_checks (integration) ─────────────────────────────────────


def test_run_hard_checks_returns_all_checks_in_order(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", fake_run())
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom()})
    records = [
        {"level": "INFO", "message": "conflate: start", "fields": {"step": "conflate", "phase": "start"}},
        _conflate_end(cgroup_current_bytes=1000, cgroup_max_bytes=4 * 1024**3),
        _atp_tally_record(point=1000),
    ]
    results = v.run_hard_checks(
        con, url, records, dmesg_text="", mem_limit="4g", expect_pipeline_version=None, min_atp_features=500
    )
    assert [r.name for r in results] == [
        "non-empty output",
        "schema",
        "atp/atp_geometry null-consistency",
        "osm/osm_geometry null-consistency",
        "osm_geometry validity",
        "provenance BOM",
        "run completed",
        "cgroup signal",
        "no OOM",
        "ATP geometry floor",
    ]
    assert all(r.passed for r in results)


# ── advisory checks ─────────────────────────────────────────────────
#
# Leaner than the hard-check tests above: one test per piece of actual
# logic (rate/ratio math, last-sample parsing, summing), not one per
# trivial branch -- these checks never fail validate, so a wrong
# CheckResult.passed value isn't the risk here; a wrong *number* is.


def test_check_match_rate_skips_for_regional_extract(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_match_rate(con, url, regional_extract="europe/switzerland")
    assert result.passed is None
    assert "skipped" in result.message


def test_check_match_rate_computes_ratio(tmp_path):
    # One matched row (VALID_ROW_SQL) plus one ATP-only (unmatched) row,
    # built by nulling out osm/osm_geometry on a second copy of it.
    sql = f"{VALID_ROW_SQL} UNION ALL SELECT atp, atp_geometry, NULL, NULL FROM ({VALID_ROW_SQL})"
    con, url = make_parquet(tmp_path, sql)
    result = v.check_match_rate(con, url, regional_extract=None)
    assert result.passed is True
    assert "1/2 rows matched (50.0%)" in result.message


def test_check_memory_distribution_reports_ratio_and_flags_shmem():
    records = [_conflate_end(rss_file_bytes=800, rss_anon_bytes=200, rss_shmem_bytes=900)]
    result = v.check_memory_distribution(records)
    assert "ratio=4.00" in result.message
    assert "tmpfs" in result.message


def test_check_memory_distribution_no_note_when_shmem_below_file():
    records = [_conflate_end(rss_file_bytes=800, rss_anon_bytes=200, rss_shmem_bytes=10)]
    result = v.check_memory_distribution(records)
    assert "tmpfs" not in result.message


def _match_progress(**fields):
    return {"message": "conflate.match: progress", "fields": fields}


def test_check_memory_distribution_prefers_peak_progress_record_over_conflate_end():
    records = [
        _match_progress(rss_bytes=100, rss_file_bytes=50, rss_anon_bytes=50, rss_shmem_bytes=0),
        _match_progress(rss_bytes=900, rss_file_bytes=800, rss_anon_bytes=100, rss_shmem_bytes=0),
        _conflate_end(rss_file_bytes=1, rss_anon_bytes=1, rss_shmem_bytes=1),
    ]
    result = v.check_memory_distribution(records)
    assert "rss_file_bytes=800" in result.message


def test_check_memory_distribution_falls_back_to_conflate_end_without_progress_records():
    records = [_conflate_end(rss_file_bytes=800, rss_anon_bytes=200, rss_shmem_bytes=0)]
    result = v.check_memory_distribution(records)
    assert "rss_file_bytes=800" in result.message


def test_check_cgroup_warnings_lists_hits():
    records = [
        {"level": "WARN", "fields": {"step": "conflate", "phase": "end", "cgroup_usage_fraction": 0.9}},
        {"level": "INFO", "fields": {"step": "conflate", "phase": "end"}},
    ]
    result = v.check_cgroup_warnings(records)
    assert "1 logged" in result.message
    assert "conflate/end=90%" in result.message


def test_check_cgroup_warnings_reports_none_when_absent():
    result = v.check_cgroup_warnings([{"level": "INFO", "fields": {}}])
    assert result.message == "none logged"


def test_check_disk_headroom_parses_last_sample():
    text = "2026-01-01 00:00:00 100 900\n2026-01-01 00:00:05 200 800\n"
    result = v.check_disk_headroom(text)
    assert "used=200 avail=800 (20.0% full)" in result.message


def test_check_timings_sums_elapsed():
    records = [
        {"fields": {"step": "import_atp", "phase": "end", "elapsed_seconds": 10.0}},
        {"fields": {"step": "conflate", "phase": "end", "elapsed_seconds": 20.5}},
        {"fields": {"step": "conflate", "phase": "start", "elapsed_seconds": None}},
    ]
    result = v.check_timings(records)
    assert "total=30.5s" in result.message


def test_check_osm_geometry_count_sums_tally():
    records = [{"message": "conflate.write: osm_geometry geometry types", "fields": {"point": 5, "polygon": 3}}]
    result = v.check_osm_geometry_count(records)
    assert "8 matched OSM geometries" in result.message


def test_run_advisory_checks_returns_all_in_order(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    results = v.run_advisory_checks(con, url, records=[], disk_log_text="", regional_extract=None)
    assert [r.name for r in results] == [
        "match rate",
        "memory distribution",
        "cgroup 85% warnings",
        "disk headroom",
        "timings",
        "OSM geometry count",
    ]
