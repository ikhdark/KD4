"""Benchmark findings 1,2,3,9,12,15,21,23 and scoped mock-provider E2E.

Candidates are test-only policies, not production changes. The combined local
pipeline and existing full-turn integration checks are reported separately.
"""
from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
ORDER = [1, 2, 3, 9, 12, 15, 21, 23, "pipeline"]
BENCH = "tools::code_mode::output_recovery_benchmarks::ordered_output_recovery_benchmark"
E2E = [
    "output_recovery_bench::full_turn_output_recovery_benchmark",
    "code_mode_output_only_preserves_running_command_and_recovers_middle",
    "code_mode_output_only_zero_budget_preserves_running_command_and_recovers_middle",
    "code_mode_truncated_cell_output_names_a_recoverable_artifact",
    "code_mode_preserves_read_history_until_its_source_changes",
    "code_mode_can_return_exec_command_output",
]
SOURCES = [
    "core/src/tools/code_mode/output_recovery_benchmarks.rs",
    "core/src/tools/code_mode/mod.rs",
    "core/src/tools/context.rs",
    "core/src/tools/registry.rs",
    "core/src/tools/handlers/read_tool_output.rs",
    "core/src/tools/handlers/read_tool_output_spec.rs",
    "core/src/tools/command_output_artifact.rs",
    "core/src/tool_history.rs",
    "core/tests/suite/code_mode.rs",
    "core/tests/suite/code_mode_output_recovery_bench.rs",
    "utils/output-truncation/src/lib.rs",
    "utils/output-truncation/src/tokenizer.rs",
    "utils/string/src/truncate.rs",
    "code-mode-protocol/src/runtime.rs",
]


def identities(stage: str) -> dict[str, str]:
    paths = [p for p in SOURCES if stage != "components" or not p.startswith("core/tests/")]
    return {p: hashlib.sha256((ROOT / "codex-rs" / p).read_bytes()).hexdigest() for p in paths}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stage", choices=["components", "e2e"])
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    before = identities(args.stage)
    env = os.environ.copy()
    if args.stage == "components":
        env["OUTPUT_RECOVERY_BENCH_OUTPUT"] = str(output / "records.jsonl")
        command = ["just", "core-test-fast", "core_lib", "--run-ignored", "only", "-E", f"test(={BENCH})"]
    else:
        env["OUTPUT_RECOVERY_E2E_OUTPUT"] = str(output / "e2e-records.jsonl")
        env["NEXTEST_TEST_THREADS"] = "1"
        expression = " | ".join(f"test(=suite::code_mode::{name})" for name in E2E)
        command = ["just", "core-test", "core_code_mode_mcp", "--run-ignored", "all", "-E", expression]
    manifest = {
        "stage": args.stage, "requested_order": ORDER,
        "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_sha256_before": before, "command": command,
        "scope": "debug-profile local A/B components; mock-provider baseline E2E, not live-model A/B",
        "expected_e2e_tests": E2E if args.stage == "e2e" else [],
    }
    path = output / "manifest.json"
    path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    start = time.perf_counter()
    with (output / "runner.log").open("w", encoding="utf-8") as log:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=log, stderr=subprocess.STDOUT, check=False)
    after = identities(args.stage)
    manifest.update(exit_code=result.returncode, elapsed_seconds=time.perf_counter() - start,
                    source_sha256_after=after, sources_unchanged=before == after)
    path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(json.dumps({"stage": args.stage, "exit_code": result.returncode,
                      "sources_unchanged": before == after, "output": str(output)}), flush=True)
    if result.returncode:
        raise SystemExit(result.returncode)
    if before != after:
        raise SystemExit("Relevant source changed during measurement; retain results as a snapshot only")
    if args.stage == "components":
        records = [json.loads(line) for line in (output / "records.jsonl").read_text().splitlines()]
        observed = list(dict.fromkeys(record["finding"] for record in records))
        if observed != ORDER or len(records) != 17:
            raise SystemExit(f"Incomplete ordered benchmark: {observed}, {len(records)} records")
        summary = [{"finding": r["finding"], "case": r["case"], "comparison": r["comparison"]} for r in records]
        (output / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    else:
        # The repository runner verifies nonempty selection and all selected results.
        log = (output / "runner.log").read_text(encoding="utf-8")
        summaries = re.findall(r"Summary.*", log)
        records = [json.loads(line) for line in (output / "e2e-records.jsonl").read_text().splitlines()]
        if len(records) != 6 or not all(r["task_success"] for r in records):
            raise SystemExit("Incomplete full-turn benchmark")
        (output / "summary.json").write_text(json.dumps({"expected_tests": E2E,
            "runner_summaries": summaries, "records": records,
            "timing_limit": "complete_turn_ns excludes setup but uses scripted mock inference; runner wall time includes build/setup"}, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
