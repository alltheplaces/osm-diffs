# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

"""Committed tests for cloud_test.py -- replaces the throwaway
monkey-patched verification scripts that had to be rewritten from
scratch for every PR against this tool (see #737). Nothing here makes
real network/Hetzner/S3 calls: `boto3`, `ssh_cmd`, `scp_to` are all
mocked, so this suite runs the same everywhere, no credentials needed.
"""

import argparse
from datetime import UTC, datetime
from pathlib import Path
from unittest.mock import MagicMock, call

import cloud_test as ct
import pytest

Args = argparse.Namespace


# ── label_flags ──────────────────────────────────────────────────────


def test_label_flags_sanitizes_branch_slash():
    flags = ct.label_flags("myname", branch="feature/foo")
    assert flags == [
        "--label",
        "osm-diffs-test=true",
        "--label",
        "osm-diffs-test-owner=myname",
        "--label",
        "osm-diffs-test-branch=feature--foo",
    ]


def test_label_flags_sanitizes_image_slash_and_colon():
    flags = ct.label_flags("myname", image="ghcr.io/alltheplaces/osm-diffs:v1.2.3")
    assert flags[-1] == "osm-diffs-test-image=ghcr.io--alltheplaces--osm-diffs--v1.2.3"


def test_label_flags_no_branch_or_image():
    flags = ct.label_flags("myname")
    assert flags == ["--label", "osm-diffs-test=true", "--label", "osm-diffs-test-owner=myname"]


# ── s3_credentials / s3_endpoint / s3_client ─────────────────────────


def test_s3_credentials_missing_exits(monkeypatch):
    monkeypatch.delenv("HETZNER_TEST_S3_ACCESS_KEY_ID", raising=False)
    monkeypatch.delenv("HETZNER_TEST_S3_ACCESS_KEY_SECRET", raising=False)
    with pytest.raises(SystemExit):
        ct.s3_credentials()


def test_s3_endpoint_format():
    assert ct.s3_endpoint("fsn1") == "https://fsn1.your-objectstorage.com"


def test_s3_client_uses_path_style_addressing(monkeypatch):
    monkeypatch.setenv("HETZNER_TEST_S3_ACCESS_KEY_ID", "AKIA_TEST")
    monkeypatch.setenv("HETZNER_TEST_S3_ACCESS_KEY_SECRET", "supersecret")
    mock_factory = MagicMock()
    monkeypatch.setattr(ct.boto3, "client", mock_factory)

    ct.s3_client("fsn1")

    args, kwargs = mock_factory.call_args
    assert args == ("s3",)
    assert kwargs["endpoint_url"] == "https://fsn1.your-objectstorage.com"
    assert kwargs["aws_access_key_id"] == "AKIA_TEST"
    assert kwargs["aws_secret_access_key"] == "supersecret"
    assert kwargs["config"].s3["addressing_style"] == "path"


# ── bucket create/destroy ────────────────────────────────────────────


def test_cmd_bucket_create(monkeypatch):
    mock_client = MagicMock()
    monkeypatch.setattr(ct, "s3_client", lambda region: mock_client)

    ct.cmd_bucket_create(Args(name="testbucket1", region="fsn1"))

    mock_client.create_bucket.assert_called_once_with(Bucket="testbucket1")


def test_cmd_bucket_destroy_empties_before_deleting(monkeypatch):
    mock_client = MagicMock()
    monkeypatch.setattr(ct, "s3_client", lambda region: mock_client)
    mock_paginator = MagicMock()
    mock_paginator.paginate.return_value = [
        {"Contents": [{"Key": "conflated.parquet"}, {"Key": "pipeline.log"}]}
    ]
    mock_client.get_paginator.return_value = mock_paginator

    ct.cmd_bucket_destroy(Args(name="testbucket1", region="fsn1", yes=True))

    mock_client.delete_object.assert_has_calls(
        [
            call(Bucket="testbucket1", Key="conflated.parquet"),
            call(Bucket="testbucket1", Key="pipeline.log"),
        ],
        any_order=True,
    )
    mock_client.delete_bucket.assert_called_once_with(Bucket="testbucket1")


def test_cmd_bucket_destroy_yes_skips_prompt(monkeypatch):
    """No input() call means no hang -- the real regression this
    guards against is --yes accidentally not being wired through."""
    monkeypatch.setattr(ct, "s3_client", lambda region: MagicMock())

    def fail_if_called(*a, **kw):
        raise AssertionError("input() should not be called when --yes is set")

    monkeypatch.setattr("builtins.input", fail_if_called)
    ct.cmd_bucket_destroy(Args(name="testbucket1", region="fsn1", yes=True))


def test_cmd_bucket_destroy_prompts_without_yes(monkeypatch):
    mock_client = MagicMock()
    monkeypatch.setattr(ct, "s3_client", lambda region: mock_client)
    monkeypatch.setattr("builtins.input", lambda prompt: "n")

    ct.cmd_bucket_destroy(Args(name="testbucket1", region="fsn1", yes=False))

    mock_client.delete_bucket.assert_not_called()


# ── containerized_run_command ────────────────────────────────────────


def test_containerized_run_command_basic_flags(monkeypatch):
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: None)
    monkeypatch.setattr(ct, "scp_to", lambda ip, local, remote: None)
    args = Args(
        regional_extract=None, bucket_name=None, bucket_region=None, mem_limit="4g", cpu_limit="2", run_id=None
    )

    cmd = ct.containerized_run_command(args, "1.2.3.4", "/workdir")

    assert "podman run --rm --read-only" in cmd
    assert "--memory=4g" in cmd
    assert "--cpus=2" in cmd
    assert "-v /workdir:/workdir" in cmd
    assert "osm-diffs-test run --workdir /workdir" in cmd
    assert "--run_id" not in cmd


def test_containerized_run_command_with_run_id(monkeypatch):
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: None)
    monkeypatch.setattr(ct, "scp_to", lambda ip, local, remote: None)
    args = Args(
        regional_extract=None, bucket_name=None, bucket_region=None, mem_limit="4g", cpu_limit="2", run_id="42"
    )
    cmd = ct.containerized_run_command(args, "1.2.3.4", "/workdir")
    assert cmd.endswith("--run_id 42")


def test_containerized_run_command_fetches_regional_extract(monkeypatch):
    calls = []
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: calls.append(("ssh", cmd)))
    monkeypatch.setattr(ct, "scp_to", lambda ip, local, remote: calls.append(("scp", str(local), remote)))
    args = Args(
        regional_extract="europe/switzerland",
        bucket_name=None,
        bucket_region=None,
        mem_limit="4g",
        cpu_limit="2",
        run_id=None,
    )

    ct.containerized_run_command(args, "1.2.3.4", "/workdir")

    scp_calls = [c for c in calls if c[0] == "scp"]
    ssh_calls = [c for c in calls if c[0] == "ssh"]
    assert any("fetch_test_extract.sh" in c[1] for c in scp_calls)
    assert any("fetch_test_extract.sh europe/switzerland /workdir" in c[1] for c in ssh_calls)
    assert any("chown -R 1000:1000 /workdir" in c[1] for c in ssh_calls)


def test_containerized_run_command_s3_env_file(monkeypatch):
    ssh_calls = []
    scp_files = {}

    def fake_scp(ip, local, remote):
        if remote.endswith("s3.env"):
            scp_files["s3.env"] = Path(local).read_text()

    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: ssh_calls.append(cmd))
    monkeypatch.setattr(ct, "scp_to", fake_scp)
    monkeypatch.setenv("HETZNER_TEST_S3_ACCESS_KEY_ID", "AKIA_TEST")
    monkeypatch.setenv("HETZNER_TEST_S3_ACCESS_KEY_SECRET", "supersecret")
    args = Args(
        regional_extract=None,
        bucket_name="b1",
        bucket_region="fsn1",
        mem_limit="8g",
        cpu_limit="4",
        run_id=None,
    )

    cmd = ct.containerized_run_command(args, "1.2.3.4", "/workdir")

    assert "--env-file /root/osm-diffs/s3.env" in cmd
    env_content = scp_files["s3.env"]
    assert "S3_ENDPOINT=https://fsn1.your-objectstorage.com" in env_content
    assert "S3_BUCKET=b1" in env_content
    assert "S3_ACCESS_KEY_ID=AKIA_TEST" in env_content
    assert "S3_ACCESS_KEY_SECRET=supersecret" in env_content
    assert any("chmod 600" in c for c in ssh_calls)


def test_containerized_run_command_missing_s3_credentials_exits(monkeypatch):
    monkeypatch.delenv("HETZNER_TEST_S3_ACCESS_KEY_ID", raising=False)
    monkeypatch.delenv("HETZNER_TEST_S3_ACCESS_KEY_SECRET", raising=False)
    args = Args(
        regional_extract=None,
        bucket_name="b1",
        bucket_region="fsn1",
        mem_limit="8g",
        cpu_limit="4",
        run_id=None,
    )
    with pytest.raises(SystemExit):
        ct.containerized_run_command(args, "1.2.3.4", "/workdir")


# ── cmd_start validation ─────────────────────────────────────────────


def test_cmd_start_containerized_requires_mem_and_cpu_limit():
    args = Args(
        containerized=True, mem_limit=None, cpu_limit=None, bucket_name=None, bucket_region=None
    )
    with pytest.raises(SystemExit, match="mem-limit"):
        ct.cmd_start(args)


def test_cmd_start_bucket_name_requires_bucket_region():
    args = Args(
        containerized=True, mem_limit="4g", cpu_limit="2", bucket_name="b1", bucket_region=None
    )
    with pytest.raises(SystemExit, match="bucket-region"):
        ct.cmd_start(args)


def test_cmd_start_validation_happens_before_any_network_call():
    """server_ip()/workdir_for() aren't monkeypatched here on purpose --
    if validation didn't happen first, this test would hit a real
    (failing) hcloud call instead of the expected SystemExit."""
    args = Args(
        containerized=True, mem_limit=None, cpu_limit=None, bucket_name=None, bucket_region=None
    )
    with pytest.raises(SystemExit):
        ct.cmd_start(args)


# ── cmd_deploy: branch vs image ──────────────────────────────────────


def test_cmd_deploy_image_path(monkeypatch):
    calls = []
    monkeypatch.setattr(ct, "server_ip", lambda name: "1.2.3.4")
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: calls.append(("ssh", cmd)))
    monkeypatch.setattr(ct, "scp_to", lambda ip, local, remote: calls.append(("scp", str(local))))

    ct.cmd_deploy(Args(name="t", image="ghcr.io/x:v1", branch=None, repo="ignored"))

    scp_names = [c[1] for c in calls if c[0] == "scp"]
    ssh_cmds = [c[1] for c in calls if c[0] == "ssh"]
    assert any("pull.sh" in s for s in scp_names)
    assert not any("build.sh" in s for s in scp_names)
    assert any("pull.sh ghcr.io/x:v1" in c for c in ssh_cmds)


def test_cmd_deploy_branch_path(monkeypatch):
    calls = []
    monkeypatch.setattr(ct, "server_ip", lambda name: "1.2.3.4")
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: calls.append(("ssh", cmd)))
    monkeypatch.setattr(ct, "scp_to", lambda ip, local, remote: calls.append(("scp", str(local))))

    ct.cmd_deploy(Args(name="t", image=None, branch="my/feature", repo="https://example/repo.git"))

    scp_names = [c[1] for c in calls if c[0] == "scp"]
    ssh_cmds = [c[1] for c in calls if c[0] == "ssh"]
    assert any("build.sh" in s for s in scp_names)
    assert not any("pull.sh" in s for s in scp_names)
    assert any("build.sh" in c and "my/feature" in c for c in ssh_cmds)


# ── compute_run_cost / teardown_cost_note ──────────────────────────


PRICING_FIXTURE = {
    "server_types": [
        {"name": "cpx32", "prices": [{"location": "hel1", "price_hourly": {"gross": "0.10"}}]},
    ],
    "volume": {"price_per_gb_month": {"gross": "0.05"}},
}


def test_compute_run_cost():
    # 10h @ 0.10/h server + 100GB @ 0.05/GB-month, prorated for 10h.
    cost = ct.compute_run_cost(PRICING_FIXTURE, "cpx32", "hel1", volume_gb=100, hours=10)
    assert cost == pytest.approx(0.10 * 10 + 0.05 * 100 * (10 / (24 * 30)))


@pytest.mark.parametrize(
    "pricing,server_type,location",
    [
        (PRICING_FIXTURE, "unknown-type", "hel1"),  # server type not in pricing
        (PRICING_FIXTURE, "cpx32", "unknown-location"),  # location not in that type's prices
        ({"server_types": []}, "cpx32", "hel1"),  # no volume pricing at all
    ],
)
def test_compute_run_cost_returns_none_on_unexpected_shape(pricing, server_type, location):
    assert ct.compute_run_cost(pricing, server_type, location, volume_gb=100, hours=10) is None


def _fake_server_describe(**overrides):
    # Field shapes taken from hcloud-go's schema/server.go, not guessed
    # -- see the 2026-08-21 incident this fixture exists to prevent a
    # repeat of: `location` sits directly on the server object, *not*
    # nested under a `datacenter` key (that assumption shipped once,
    # untested against a real account, and crashed `cmd_destroy` before
    # it deleted anything).
    fields = {
        "created": "2026-01-01T00:00:00+00:00",
        "server_type": {"name": "cpx32"},
        "location": {"name": "hel1"},
    }
    fields.update(overrides)
    return fields


def test_teardown_cost_note_reports_estimate(monkeypatch):
    def fake_hcloud_json(args):
        if args[0] == "server":
            return _fake_server_describe()
        return {"size": 100}

    monkeypatch.setattr(ct, "hcloud_json", fake_hcloud_json)
    monkeypatch.setattr(ct, "fetch_pricing", lambda: PRICING_FIXTURE)

    note = ct.teardown_cost_note("t")
    assert note is not None
    assert "Estimated cost: ~€" in note


def test_teardown_cost_note_is_none_when_pricing_unavailable(monkeypatch):
    monkeypatch.setattr(ct, "hcloud_json", lambda args: _fake_server_describe(size=1))
    monkeypatch.setattr(ct, "fetch_pricing", lambda: None)
    assert ct.teardown_cost_note("t") is None


def test_teardown_cost_note_is_none_on_unexpected_response_shape(monkeypatch):
    """The actual 2026-08-21 incident, reproduced: `hcloud_json`/
    `fetch_pricing` both return *something*, but not the shape the
    parsing code expects (any KeyError/TypeError while reading it) --
    must degrade to None, not raise out of `cmd_destroy` before it
    deletes anything."""

    def fake_hcloud_json(args):
        if args[0] == "server":
            return _fake_server_describe(location={"unexpected_key": "hel1"})
        return {"size": 100}

    monkeypatch.setattr(ct, "hcloud_json", fake_hcloud_json)
    monkeypatch.setattr(ct, "fetch_pricing", lambda: PRICING_FIXTURE)
    assert ct.teardown_cost_note("t") is None


def test_cmd_destroy_deletes_everything_even_when_cost_note_fails(monkeypatch):
    calls = []
    monkeypatch.setattr(ct, "teardown_cost_note", lambda name: None)
    monkeypatch.setattr(ct.subprocess, "run", lambda cmd, **kw: calls.append(cmd))

    ct.cmd_destroy(Args(name="t", yes=True))

    assert len(calls) == 3  # volume detach, volume delete, server delete


# ── cmd_list: --bucket-region ────────────────────────────────────────


def test_cmd_list_skips_buckets_without_bucket_region(monkeypatch, capsys):
    monkeypatch.setattr(ct, "hcloud_json", lambda args: [])
    monkeypatch.setattr(ct, "s3_client", lambda region: (_ for _ in ()).throw(AssertionError("should not be called")))

    ct.cmd_list(Args(bucket_region=None))

    assert "bucket:" not in capsys.readouterr().out


def test_cmd_list_lists_buckets_with_bucket_region(monkeypatch, capsys):
    mock_client = MagicMock()
    mock_client.list_buckets.return_value = {
        "Buckets": [{"Name": "osm-diffs-container-test-1", "CreationDate": datetime(2026, 1, 1, tzinfo=UTC)}]
    }
    monkeypatch.setattr(ct, "hcloud_json", lambda args: [])
    monkeypatch.setattr(ct, "s3_client", lambda region: mock_client)

    ct.cmd_list(Args(bucket_region="fsn1"))

    assert "osm-diffs-container-test-1" in capsys.readouterr().out


# ── cmd_create prints the teardown command ────────────────────────────


def test_cmd_create_prints_destroy_command(monkeypatch, capsys):
    monkeypatch.setattr(ct, "sh", lambda cmd, **kw: None)
    monkeypatch.setattr(ct, "hcloud_json", lambda args: {"id": "123"})
    monkeypatch.setattr(ct, "server_ip", lambda name: "1.2.3.4")
    monkeypatch.setattr(ct, "wait_for_ssh", lambda ip: None)
    monkeypatch.setattr(ct, "ssh_cmd", lambda ip, cmd, **kw: None)
    monkeypatch.setattr(ct, "run_sysinfo", lambda ip: None)
    monkeypatch.setattr(ct, "run_fio", lambda ip, workdir: None)

    ct.cmd_create(
        Args(name="t", type="cpx32", location="hel1", ssh_key="k", branch=None, volume_size=400)
    )

    assert "cloud_test.py destroy --name t" in capsys.readouterr().out
