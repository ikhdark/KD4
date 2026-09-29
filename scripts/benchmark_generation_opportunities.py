"""Run the eight opt-in core component benchmarks in the requested order.

Uses the repository's named-target/lane workflow. No installed app or external
model is started. Raw runner output and source identities accompany results.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
CASES = [
    (14, "tools::code_mode::response_tests::generation_bench_14"),
    (5, "tools::code_mode::response_tests::generation_bench_05"),
    (17, "tool_history::tests::generation_bench_17"),
    (13, "tools::handlers::read_file::tests::generation_bench_13"),
    (12, "tools::command_output_artifact::hardening_tests::generation_bench_12"),
    (10, "tools::command_output_artifact::hardening_tests::generation_bench_10"),
    (18, "tool_history::tests::generation_bench_18"),
    (19, "compact::tests::generation_bench_19"),
]


def identities() -> dict[str, str]:
    paths = [
        "core/src/generation_benchmarks.rs", "core/src/lib.rs",
        "core/src/tools/code_mode/mod.rs", "core/src/tools/code_mode/response_tests.rs",
        "core/src/tools/command_output_artifact.rs", "core/src/tools/command_output_artifact_tests.rs",
        "core/src/tools/handlers/read_tool_output.rs", "core/src/tools/handlers/read_file.rs",
        "core/src/tools/handlers/context_checkpoint.rs", "core/src/tool_history.rs",
        "core/src/tool_history_tests.rs", "core/src/compact.rs", "core/src/compact_tests.rs",
        "utils/output-truncation/src/tokenizer.rs", ".cargo/config.toml", ".config/nextest.toml",
    ]
    return {p: hashlib.sha256((ROOT / "codex-rs" / p).read_bytes()).hexdigest() for p in paths}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--start", type=int, choices=[n for n, _ in CASES], default=14)
    parser.add_argument("--test-binary", type=pathlib.Path)
    parser.add_argument("--source-manifest", type=pathlib.Path)
    parser.add_argument("--only", type=int, nargs="+", choices=[n for n, _ in CASES])
    args = parser.parse_args()
    if args.runs < 1:
        parser.error("--runs must be positive")
    if bool(args.test_binary) != bool(args.source_manifest):
        parser.error("snapshot execution requires both --test-binary and --source-manifest")
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                "source_sha256": identities(), "requested_order": [n for n, _ in CASES],
                "scope": "debug-profile local components; no live model generations or synthesis quality measured"}
    binary = None
    if args.test_binary:
        binary = (args.output / args.test_binary.name).resolve()
        shutil.copy2(args.test_binary, binary)
        manifest["binary_sha256"] = hashlib.file_digest(binary.open("rb"), "sha256").hexdigest()
        manifest["build_source_manifest"] = json.loads(args.source_manifest.read_text(encoding="utf-8"))
        manifest["scope"] += "; cached build snapshot, not current-checkout validation"
    (args.output / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    started = False
    for finding, name in CASES:
        started |= finding == args.start
        if not started:
            continue
        if args.only and finding not in args.only:
            continue
        for run in range(args.runs):
            if not binary:
                assert identities() == manifest["source_sha256"], "Benchmark source changed during the run"
            command = [sys.executable, str(ROOT / "scripts/rust_build_status.py"), "run-lane",
                       "--lane", "core-tests", "--warm-wait-seconds", "300", "--",
                       "just", "_core-test-reserved", "fast", "core_lib", "--run-ignored", "only",
                       "-E", f"test(={name})"]
            if binary:
                command = [str(binary), "--exact", name, "--ignored", "--nocapture", "--test-threads=1"]
            path = args.output / f"finding-{finding}-run-{run}.log"
            records_path = args.output / f"finding-{finding}-run-{run}.jsonl"
            env = dict(os.environ, GENERATION_BENCH_OUTPUT=str(records_path.resolve()))
            if binary:
                env = {k: v for k, v in env.items() if not k.startswith("CODEX_")}
                env["RUST_MIN_STACK"] = str(max(8388608, int(env.get("RUST_MIN_STACK", "0"))))
            start = time.perf_counter()
            with path.open("w", encoding="utf-8") as output:
                result = subprocess.run(command, cwd=ROOT / "codex-rs/core" if binary else ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, check=False)
            records = [json.loads(line) for line in records_path.read_text(encoding="utf-8").splitlines()] if records_path.exists() else []
            summary = {"finding": finding, "run": run, "exit_code": result.returncode,
                       "runner_elapsed_seconds": time.perf_counter()-start, "records": records, "command": command}
            (args.output / f"finding-{finding}-run-{run}.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
            print(f"#{finding} run {run}: exit={result.returncode}, records={len(records)}, log={path}", flush=True)
            if result.returncode or not records:
                raise SystemExit(f"Benchmark incomplete; inspect {path}. Completed evidence was retained.")
    if not binary:
        assert identities() == manifest["source_sha256"]


if __name__ == "__main__":
    main()
