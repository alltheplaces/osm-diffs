# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

"""Tests for validate.py's hard checks. Runs real DuckDB queries against
small Parquet fixtures built on the fly (rather than mocking the query
engine itself, which would just be re-asserting the SQL string instead
of actually exercising it) -- no S3/network/podman calls: fixtures are
plain local files, and the one check that does shell out
(check_provenance_bom's cyclonedx-cli call) has `subprocess.run` mocked.
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
    ST_AsWKB(ST_Point(8.5, 47.4))::BLOB AS atp_geometry,
    {
        'type': 'node',
        'id': 12345::UBIGINT,
        'tags': MAP(['shop'], ['bakery']),
        'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER},
        'way_members': NULL::UBIGINT[],
        'relation_members': NULL::STRUCT("type" VARCHAR, id UBIGINT, "role" VARCHAR)[]
    } AS osm,
    ST_AsWKB(ST_Point(8.5, 47.4))::BLOB AS osm_geometry
"""


def make_parquet(tmp_path, select_sql, name="conflated.parquet", kv_metadata=None):
    """Writes `select_sql`'s result to a real local Parquet file (loading
    the `spatial` extension first, since the fixture rows use ST_Point/
    ST_AsWKB) and returns its path as a `str`, usable directly with
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


# ── parse_byte_size ──────────────────────────────────────────────────


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


# ── read_pipeline_log / find_step_record ────────────────────────────


def test_read_pipeline_log_parses_jsonl(tmp_path):
    log = tmp_path / "pipeline.log"
    log.write_text('{"level": "INFO", "message": "a"}\n{"level": "ERROR", "message": "b"}\n')
    records = v.read_pipeline_log(log)
    assert [r["message"] for r in records] == ["a", "b"]


def test_read_pipeline_log_skips_blank_lines(tmp_path):
    log = tmp_path / "pipeline.log"
    log.write_text('{"message": "a"}\n\n{"message": "b"}\n')
    assert len(v.read_pipeline_log(log)) == 2


def test_find_step_record_matches_step_and_phase():
    records = [
        {"fields": {"step": "conflate", "phase": "start"}},
        {"fields": {"step": "conflate", "phase": "end"}, "marker": "found"},
    ]
    assert v.find_step_record(records, "conflate", "end") == records[1]


def test_find_step_record_returns_none_when_missing():
    assert v.find_step_record([{"fields": {"step": "conflate", "phase": "start"}}], "conflate", "end") is None


# ── check_nonempty ───────────────────────────────────────────────────


def test_check_nonempty_passes_with_rows(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_nonempty(con, url)
    assert result.passed is True


def test_check_nonempty_fails_when_empty(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL + " WHERE false")
    result = v.check_nonempty(con, url)
    assert result.passed is False


# ── check_schema ─────────────────────────────────────────────────────


def test_check_schema_passes_for_documented_schema(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_schema(con, url)
    assert result.passed is True


def test_check_schema_fails_if_changeset_reappears(tmp_path):
    """The regression this check exists for: #731 removed `changeset`
    from osm.modified -- if it ever came back, this must fail."""
    sql = VALID_ROW_SQL.replace(
        "'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER},",
        "'modified': {'timestamp': TIMESTAMP '2026-01-01 00:00:00', 'version': 3::UINTEGER, "
        "'changeset': 99::UBIGINT},",
    )
    con, url = make_parquet(tmp_path, sql)
    result = v.check_schema(con, url)
    assert result.passed is False
    assert "changeset" in result.message


def test_check_schema_fails_on_missing_top_level_column(tmp_path):
    sql = "SELECT NULL::STRUCT(tags MAP(VARCHAR, VARCHAR)) AS atp"
    con, url = make_parquet(tmp_path, sql)
    result = v.check_schema(con, url)
    assert result.passed is False


# ── check_null_consistency ───────────────────────────────────────────


def test_check_null_consistency_passes_when_consistent(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_null_consistency(con, url, "atp", "atp_geometry")
    assert result.passed is True


def test_check_null_consistency_fails_when_mismatched(tmp_path):
    sql = VALID_ROW_SQL.replace("ST_AsWKB(ST_Point(8.5, 47.4))::BLOB AS atp_geometry,", "NULL::BLOB AS atp_geometry,")
    con, url = make_parquet(tmp_path, sql)
    result = v.check_null_consistency(con, url, "atp", "atp_geometry")
    assert result.passed is False


# ── check_geometry_validity ──────────────────────────────────────────


def test_check_geometry_validity_passes_for_valid_geometry(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_geometry_validity(con, url)
    assert result.passed is True


def test_check_geometry_validity_fails_for_invalid_polygon(tmp_path):
    # A self-intersecting ("bowtie") polygon -- the textbook invalid
    # geometry.
    sql = VALID_ROW_SQL.replace(
        "ST_AsWKB(ST_Point(8.5, 47.4))::BLOB AS osm_geometry",
        "ST_AsWKB(ST_GeomFromText("
        "'POLYGON((0 0, 2 2, 2 0, 0 2, 0 0))')) ::BLOB AS osm_geometry",
    )
    con, url = make_parquet(tmp_path, sql)
    result = v.check_geometry_validity(con, url)
    assert result.passed is False


# ── check_provenance_bom ─────────────────────────────────────────────


def _bom(pipeline_version="0.8.0", spec_version="1.7"):
    return json.dumps(
        {
            "specVersion": spec_version,
            "metadata": {"tools": {"components": [{"version": pipeline_version}]}},
        }
    )


def test_check_provenance_bom_passes(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", lambda *a, **k: type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})())
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom()})
    result = v.check_provenance_bom(con, url, expect_pipeline_version=None)
    assert result.passed is True


def test_check_provenance_bom_fails_when_key_missing(tmp_path):
    con, url = make_parquet(tmp_path, VALID_ROW_SQL)
    result = v.check_provenance_bom(con, url, expect_pipeline_version=None)
    assert result.passed is False
    assert "org.cyclonedx.bom" in result.message


def test_check_provenance_bom_fails_when_cyclonedx_cli_rejects_it(tmp_path, monkeypatch):
    monkeypatch.setattr(
        v.subprocess,
        "run",
        lambda *a, **k: type("R", (), {"returncode": 1, "stdout": "invalid", "stderr": ""})(),
    )
    con, url = make_parquet(tmp_path, VALID_ROW_SQL, kv_metadata={"org.cyclonedx.bom": _bom()})
    result = v.check_provenance_bom(con, url, expect_pipeline_version=None)
    assert result.passed is False


def test_check_provenance_bom_fails_on_pipeline_version_mismatch(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", lambda *a, **k: type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})())
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


def _conflate_end(**fields):
    return {
        "level": "INFO",
        "message": "conflate: end",
        "fields": {"step": "conflate", "phase": "end", **fields},
    }


def test_check_cgroup_signal_passes_when_fields_present_and_matching():
    records = [_conflate_end(cgroup_current_bytes=1000, cgroup_max_bytes=4 * 1024**3)]
    result = v.check_cgroup_signal(records, mem_limit="4g")
    assert result.passed is True


def test_check_cgroup_signal_fails_when_fields_null():
    records = [_conflate_end()]
    result = v.check_cgroup_signal(records, mem_limit="4g")
    assert result.passed is False


def test_check_cgroup_signal_fails_when_mem_limit_mismatched():
    records = [_conflate_end(cgroup_current_bytes=1000, cgroup_max_bytes=2 * 1024**3)]
    result = v.check_cgroup_signal(records, mem_limit="4g")
    assert result.passed is False


def test_check_cgroup_signal_skips_mem_limit_comparison_when_not_given():
    records = [_conflate_end(cgroup_current_bytes=1000, cgroup_max_bytes=2 * 1024**3)]
    result = v.check_cgroup_signal(records, mem_limit=None)
    assert result.passed is True


def test_check_cgroup_signal_fails_when_conflate_end_missing():
    assert v.check_cgroup_signal([], mem_limit="4g").passed is False


# ── check_no_oom ──────────────────────────────────────────────────────


def test_check_no_oom_passes_on_clean_log():
    assert v.check_no_oom("kernel: some unrelated line\n").passed is True


def test_check_no_oom_fails_on_oom_kill_signature():
    result = v.check_no_oom("kernel: Out of memory: Killed process 123 (osm-diffs)\n")
    assert result.passed is False


# ── check_atp_geometry_floor ───────────────────────────────────────────


def _atp_tally_record(**counts):
    fields = {"point": 0, "line_string": 0, "polygon": 0, "multi_point": 0, "multi_line_string": 0, "multi_polygon": 0, "geometry_collection": 0}
    fields.update(counts)
    return {"message": "import_atp: alltheplaces.parquet geometry types", "fields": fields}


def test_check_atp_geometry_floor_passes_above_floor():
    records = [_atp_tally_record(point=1000)]
    result = v.check_atp_geometry_floor(records, min_features=500)
    assert result.passed is True


def test_check_atp_geometry_floor_fails_below_floor():
    records = [_atp_tally_record(point=10)]
    result = v.check_atp_geometry_floor(records, min_features=500)
    assert result.passed is False


def test_check_atp_geometry_floor_skips_when_no_floor_given():
    records = [_atp_tally_record(point=10)]
    result = v.check_atp_geometry_floor(records, min_features=None)
    assert result.passed is None


def test_check_atp_geometry_floor_fails_when_record_missing():
    result = v.check_atp_geometry_floor([], min_features=500)
    assert result.passed is False


# ── run_hard_checks (integration) ─────────────────────────────────────


def test_run_hard_checks_returns_all_checks_in_order(tmp_path, monkeypatch):
    monkeypatch.setattr(v.subprocess, "run", lambda *a, **k: type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})())
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
