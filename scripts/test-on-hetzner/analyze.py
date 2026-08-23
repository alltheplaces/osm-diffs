#!/usr/bin/env python3
"""Makes sense of what `logs` downloaded -- pipeline.log/vmstat.log/
disk.log/sysinfo.txt -- without re-writing the same throwaway parsing
script every time. Deliberately narrow: this handles the mechanical,
always-useful parts (turning pipeline.log into a skimmable timeline,
summarizing vmstat/disk over a time window, comparing machines side by
side), not an open-ended analysis framework -- the actual question
worth asking about a given run is usually specific to that run, and
this doesn't try to guess it for you.

Usage:
  analyze.py timeline logs/<name>/pipeline.log [--all]
  analyze.py vmstat-stats logs/<name>/vmstat.log [--step NAME | --from TS --to TS]
  analyze.py disk-stats logs/<name>/disk.log [--step NAME | --from TS --to TS]
  analyze.py compare logs/<name1> logs/<name2> ...
"""

import argparse
import json
import re
import statistics
import sys
from pathlib import Path

# Below this many occurrences of the same normalized message, lines are
# shown individually rather than collapsed into one summary line.
COLLAPSE_THRESHOLD = 5

# Replaces any run of path/identifier-ish characters that contains at
# least one digit with a single placeholder -- turns both
# "could not build geometry for way/116707169" and
# "using /mnt/HC_Volume_106613191/workdir/.tmp4ur8d4 as a temporary
# directory" into a normalized template so repeated noise collapses,
# without needing to hardcode which specific messages are "known noisy".
# A message with no digits in it at all (e.g. "opened OpenStreetMap
# planet file") is left alone -- those tend to be rare, significant,
# one-off events, not noise.
_TOKEN_WITH_DIGIT = re.compile(r"[A-Za-z0-9_./-]*\d[A-Za-z0-9_./-]*")


def normalize(message):
    return _TOKEN_WITH_DIGIT.sub("#", message)


def read_json_lines(path):
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                continue


def is_collapse_eligible(entry):
    """Step markers (fields.phase set), errors, and anything with more
    than one structured field are always shown individually -- fields
    beyond a bare step/phase marker usually mean the line carries real
    data (a byte count, a row count, a periodic progress snapshot),
    not just repeated boilerplate text."""
    fields = entry.get("fields", {})
    if entry.get("level") == "ERROR":
        return False
    if "phase" in fields:
        return False
    return len(fields) <= 1


def build_timeline(path, show_all):
    """Yields (timestamp, level, text) tuples, collapsing repeated
    boilerplate lines into one summary line each unless show_all."""
    entries = list(read_json_lines(path))

    counts = {}
    if not show_all:
        for e in entries:
            if is_collapse_eligible(e):
                counts[normalize(e.get("message", ""))] = (
                    counts.get(normalize(e.get("message", "")), 0) + 1
                )

    emitted_summary = set()
    for e in entries:
        msg = e.get("message", "")
        level = e.get("level", "?")
        ts = e.get("timestamp", "?")
        fields = e.get("fields", {})
        template = normalize(msg)

        if not show_all and is_collapse_eligible(e) and counts.get(template, 0) >= COLLAPSE_THRESHOLD:
            if template in emitted_summary:
                continue
            emitted_summary.add(template)
            first = next(x["timestamp"] for x in entries if normalize(x.get("message", "")) == template)
            last = next(
                x["timestamp"] for x in reversed(entries) if normalize(x.get("message", "")) == template
            )
            yield (first, level, f"{msg}  [x{counts[template]}, {first} .. {last}]")
            continue

        is_step = "step" in fields
        if is_step and not show_all:
            # Step markers carry a full crate::memstats snapshot (9
            # fields, mostly None outside a cgroup-limited container --
            # see src/memstats.rs), which drowns out the one number that
            # actually matters for a skim: elapsed_seconds. --all shows
            # the rest.
            interesting = {"elapsed_seconds": fields["elapsed_seconds"]} if fields.get("elapsed_seconds") is not None else {}
        else:
            interesting = {k: v for k, v in fields.items() if k not in ("step", "phase") and v is not None}
        extra = "  " + " ".join(f"{k}={v}" for k, v in interesting.items()) if interesting else ""
        step_prefix = f"[{fields['step']}:{fields.get('phase', '?')}] " if is_step else ""
        yield (ts, level, f"{step_prefix}{msg}{extra}")


def cmd_timeline(args):
    for ts, level, text in build_timeline(args.pipeline_log, args.all):
        print(f"{ts} {level:5} {text}")


def find_step_window(pipeline_log, step_name):
    start = end = None
    for e in read_json_lines(pipeline_log):
        fields = e.get("fields", {})
        if fields.get("step") != step_name:
            continue
        if fields.get("phase") == "start":
            start = e["timestamp"]
        elif fields.get("phase") == "end":
            end = e["timestamp"]
    if start is None or end is None:
        sys.exit(f"step {step_name!r} not found (or incomplete) in {pipeline_log}")
    # vmstat.log/disk.log timestamps are "YYYY-MM-DD HH:MM:SS", no
    # sub-second/timezone punctuation -- trim pipeline.log's RFC3339
    # timestamp down to match so plain string comparison works.
    return start[:19].replace("T", " "), end[:19].replace("T", " ")


def default_pipeline_log(data_log_path):
    return Path(data_log_path).parent / "pipeline.log"


def resolve_window(args):
    if args.step:
        pipeline_log = args.pipeline_log or default_pipeline_log(args.log)
        return find_step_window(pipeline_log, args.step)
    if args.from_ts or args.to_ts:
        return args.from_ts or "0000-00-00 00:00:00", args.to_ts or "9999-99-99 99:99:99"
    return None, None


def cmd_vmstat_stats(args):
    lo, hi = resolve_window(args)
    rows = []
    with open(args.log) as f:
        for line in f:
            parts = line.split()
            if len(parts) < 19 or parts[0] in ("procs", "r"):
                continue
            try:
                nums = [int(x) for x in parts[:18]]
            except ValueError:
                continue
            ts = f"{parts[18]} {parts[19]}"
            if lo and not (lo <= ts <= hi):
                continue
            rows.append(nums)
    if not rows:
        sys.exit("no vmstat samples in that window")

    cols = ["r", "b", "swpd", "free", "buff", "cache", "si", "so", "bi", "bo", "in", "cs", "us", "sy", "id", "wa", "st", "gu"]
    print(f"{len(rows)} samples" + (f" from {lo} to {hi}" if lo else ""))
    for name in ("r", "b", "bi", "bo", "us", "sy", "id", "wa", "st"):
        i = cols.index(name)
        vals = [r[i] for r in rows]
        print(f"  {name:5} avg={statistics.mean(vals):8.1f}  max={max(vals):8d}")


def cmd_disk_stats(args):
    lo, hi = resolve_window(args)
    rows = []
    with open(args.log) as f:
        for line in f:
            parts = line.split()
            if len(parts) != 4:
                continue
            ts = f"{parts[0]} {parts[1]}"
            if lo and not (lo <= ts <= hi):
                continue
            rows.append((ts, int(parts[2]), int(parts[3])))
    if not rows:
        sys.exit("no disk samples in that window")

    used_start, used_end = rows[0][1], rows[-1][1]
    peak_used = max(r[1] for r in rows)
    gb = lambda b: b / 1e9
    print(f"{len(rows)} samples" + (f" from {lo} to {hi}" if lo else ""))
    print(f"  used: {gb(used_start):.2f} GB -> {gb(used_end):.2f} GB  (peak {gb(peak_used):.2f} GB)")
    print(f"  grew by {gb(used_end - used_start):+.2f} GB over the window")


def cmd_compare(args):
    for name_dir in args.dirs:
        pipeline_log = Path(name_dir) / "pipeline.log"
        print(f"=== {name_dir} ===")
        if not pipeline_log.exists():
            print("  (no pipeline.log found)")
            continue
        steps = {}
        for e in read_json_lines(pipeline_log):
            fields = e.get("fields", {})
            step = fields.get("step")
            if not step or "phase" not in fields:
                continue
            steps.setdefault(step, {})[fields["phase"]] = (e["timestamp"], fields.get("elapsed_seconds"))
        for step, phases in steps.items():
            elapsed = phases.get("end", (None, None))[1]
            status = f"{elapsed:.1f}s" if elapsed is not None else "(in progress or failed)"
            print(f"  {step:20} {status}")
        print()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)

    p_timeline = sub.add_parser("timeline", help="skimmable view of pipeline.log")
    p_timeline.add_argument("pipeline_log")
    p_timeline.add_argument("--all", action="store_true", help="don't collapse repeated lines")
    p_timeline.set_defaults(func=cmd_timeline)

    def add_window_args(p):
        p.add_argument("log")
        p.add_argument("--step", help="derive the time window from this pipeline.log step")
        p.add_argument("--pipeline-log", help="default: pipeline.log next to LOG")
        p.add_argument("--from", dest="from_ts", help="YYYY-MM-DD HH:MM:SS")
        p.add_argument("--to", dest="to_ts", help="YYYY-MM-DD HH:MM:SS")

    p_vmstat = sub.add_parser("vmstat-stats", help="summarize vmstat.log over a window")
    add_window_args(p_vmstat)
    p_vmstat.set_defaults(func=cmd_vmstat_stats)

    p_disk = sub.add_parser("disk-stats", help="summarize disk.log over a window")
    add_window_args(p_disk)
    p_disk.set_defaults(func=cmd_disk_stats)

    p_compare = sub.add_parser("compare", help="step timings side by side across machines")
    p_compare.add_argument("dirs", nargs="+", help="e.g. logs/machine-1 logs/machine-2")
    p_compare.set_defaults(func=cmd_compare)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
