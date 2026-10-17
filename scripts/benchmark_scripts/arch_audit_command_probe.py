#!/usr/bin/env python3
"""Command-level probes for the 2026-10-16 architecture audit.

Read-only. From `item_completed/CommandExecution` records:

* inline Python programs (PowerShell here-string piped to Python, `python -c`):
  what they do (write files, walk trees, parse JSON, run subprocesses);
* Rust validation runs (`cargo test|nextest|check`, `just core-*`) by repository
  (`cwd`), with time spent in runs that ended in a compile error;
* PowerShell process floor: duration of trivially fast commands;
* `rg` failures that are not "no match", by cause.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys
from pathlib import Path

from arch_audit_corpus_profile import classify_script, iter_records, quantiles, shell_script

COMPILE_FAIL_RE = re.compile(r"error\[E\d{4}\]|error: could not compile|error: aborting due to")
RUST_VALIDATION_RE = re.compile(
    r"\bcargo\s+(?:\+\S+\s+)?(?:test|nextest|check|clippy|build)\b|\bjust\s+(?:core-test|core-gate|test|clippy)|rust_test_runner\.py"
)
RG_FAILURE_CLASSES = [
    ("missing path", re.compile(r"os error [23]\b|cannot find the (?:file|path) specified", re.I)),
    ("unexpanded glob or invalid path syntax", re.compile(r"os error 123\b|volume label syntax is incorrect", re.I)),
    ("regex parse error", re.compile(r"regex parse error", re.I)),
]
PY_TRAITS = {
    "writes_files": re.compile(r"write_text|write_bytes|open\([^)]*['\"][wa]b?['\"]|json\.dump\(|\.to_csv|shutil\.(?:copy|move)"),
    "walks_tree": re.compile(r"rglob|os\.walk|glob\.glob|iterdir|scandir"),
    "parses_json": re.compile(r"json\.loads?\("),
    "runs_subprocess": re.compile(r"subprocess\.|os\.system"),
    "regex": re.compile(r"\bre\.(?:search|findall|finditer|match|sub|compile)"),
    "reads_files": re.compile(r"read_text|read_bytes|open\("),
}


def repo_of(cwd: str) -> str:
    lowered = (cwd or "").replace("\\", "/").lower()
    for marker in ("desktop/kd4", "rolloutdetect", "desktop/kda", "rd-exp", "kdpc", "kdgma", "kdsb", "mdpwa", "twitch", "local-kd"):
        if marker in lowered:
            return marker
    return "other"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--home", default=r"C:\Users\kuh\Desktop\LOCAL-KD")
    parser.add_argument("--subdir", default="sessions")
    parser.add_argument("--until", default="", help="only rollouts whose file name start time is < YYYY-MM-DDTHH-MM-SS")
    args = parser.parse_args()
    home = Path(args.home)
    payloads = home / "rollout-payloads"
    files = sorted((home / args.subdir).rglob("*.jsonl"), key=lambda p: p.name)
    if args.until:
        files = [p for p in files if p.name[8:27] < args.until]

    inline = collections.Counter()
    inline_traits = collections.Counter()
    inline_lines = []
    rust = collections.defaultdict(lambda: collections.Counter())
    rust_fail_durations = []
    rust_ok_durations = []
    floor = []
    profile_flag = collections.Counter()
    rg_outcomes = collections.Counter()

    for path in files:
        for record in iter_records(path, payloads):
            payload = record.get("payload") or {}
            if record.get("type") != "event_msg" or payload.get("type") != "item_completed":
                continue
            item = payload.get("item") or {}
            if item.get("type") != "CommandExecution":
                continue
            argv = item.get("command") or []
            if isinstance(argv, str):
                argv = [argv]
            lowered = [str(a).lower() for a in argv]
            profile_flag["-noprofile" in lowered] += 1
            _, script = shell_script(argv)
            family, _ = classify_script(script)
            duration = item.get("duration") or {}
            seconds = (duration.get("secs") or 0) + (duration.get("nanos") or 0) / 1e9
            output = item.get("aggregated_output") or ""
            if family in ("python-inline", "python-c"):
                inline[family] += 1
                inline_lines.append(script.count("\n") + 1)
                for trait, pattern in PY_TRAITS.items():
                    if pattern.search(script):
                        inline_traits[trait] += 1
            if RUST_VALIDATION_RE.search(script):
                bucket = rust[repo_of(item.get("cwd") or "")]
                bucket["runs"] += 1
                bucket["seconds"] += seconds
                if COMPILE_FAIL_RE.search(output):
                    bucket["compile_failure_runs"] += 1
                    bucket["compile_failure_seconds"] += seconds
                    rust_fail_durations.append(seconds)
                else:
                    rust_ok_durations.append(seconds)
                    if item.get("exit_code") not in (0, None):
                        bucket["other_nonzero_runs"] += 1
            if family in ("git", "rg", "ps-test-path") and len(output) < 4096:
                floor.append(seconds)
            if family == "rg":
                rg_outcomes["runs"] += 1
                no_match = (item.get("output_metadata") or {}).get("search_no_match")
                if item.get("exit_code") not in (0, None) and not no_match:
                    message = item.get("stderr") or output
                    for label, pattern in RG_FAILURE_CLASSES:
                        if pattern.search(message):
                            rg_outcomes[label] += 1
                            break
                    else:
                        rg_outcomes["other nonzero (often a later command in the same script)"] += 1

    report = {
        "files": len(files),
        "inline_python": {
            "commands": dict(inline),
            "lines_quantiles": quantiles(inline_lines),
            "traits": dict(inline_traits.most_common()),
        },
        "rust_validation_by_repo": {k: {kk: round(vv, 1) if isinstance(vv, float) else vv for kk, vv in v.items()} for k, v in rust.items()},
        "rust_compile_failure_seconds_quantiles": quantiles(rust_fail_durations),
        "rust_other_seconds_quantiles": quantiles(rust_ok_durations),
        "small_command_floor_seconds": quantiles(floor, (0.1, 0.5, 0.9)),
        "small_command_count": len(floor),
        "argv_has_noprofile": {str(k): v for k, v in profile_flag.items()},
        "rg_outcomes": dict(rg_outcomes.most_common()),
    }
    json.dump(report, sys.stdout, indent=1)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
