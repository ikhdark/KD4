"""Run component, mock-provider E2E, or explicitly authorized live benchmarks.

Uses the repository's reserved Cargo lane and named test targets. Candidates
are test-only. Only live-e2e measures real provider usage; it requires --allow-live.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parents[1]
ORDER = [15, 17, 5, 2, 10, 18]
BENCH = "tools::code_mode::token_cache_benchmarks::ordered_token_cache_benchmark"
CANDIDATE_E2E = "token_cache_e2e::candidates_complete_turn_non_regression"
LIVE_E2E = "token_cache_e2e::live::integrated_live_model_pair"
E2E = [
    "code_mode_output_only_preserves_running_command_and_recovers_middle",
    "code_mode_output_only_zero_budget_preserves_running_command_and_recovers_middle",
    "code_mode_truncated_cell_output_names_a_recoverable_artifact",
    "code_mode_preserves_read_history_until_its_source_changes",
    "code_mode_only_guides_all_tools_search_and_calls_deferred_app_tools",
    "code_mode_can_return_exec_command_output",
    "code_mode_can_print_structured_mcp_tool_result_fields",
    "code_mode_can_use_mcp_image_result_with_image_helper",
    "a_projected_nested_read_returns_its_execution_result_to_javascript",
    "code_mode_can_print_content_only_mcp_tool_result_fields",
    "code_mode_can_print_error_mcp_tool_result_fields",
]
SOURCES = [
    "core/src/tools/code_mode/mod.rs",
    "core/src/tools/code_mode/token_cache_benchmarks.rs",
    "core/src/tools/context.rs",
    "core/src/tools/handlers/read_file.rs",
    "core/src/tools/handlers/tool_search.rs",
    "core/src/tools/spec_plan.rs",
    "core/src/tool_history.rs",
    "core/src/tools/command_output_artifact.rs",
    "core/tests/suite/code_mode.rs",
    "core/tests/suite/code_mode_token_cache_e2e.rs",
    "core/tests/suite/code_mode_token_cache_live.rs",
    "tools/src/tool_output.rs",
    "protocol/src/models.rs",
    "utils/output-truncation/src/tokenizer.rs",
]


def identities() -> dict[str, str]:
    return {
        name: hashlib.sha256((ROOT / "codex-rs" / name).read_bytes()).hexdigest()
        for name in SOURCES
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "stage", choices=["components", "e2e", "candidate-e2e", "live-e2e"]
    )
    parser.add_argument(
        "--allow-live", action="store_true", help="Authorize actual model usage"
    )
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--test", choices=E2E, help="Rerun one affected E2E check")
    args = parser.parse_args()
    if args.test and args.stage != "e2e":
        parser.error("--test requires the e2e stage")
    if args.stage == "live-e2e" and not args.allow_live:
        parser.error("live-e2e requires --allow-live and consumes real model usage")
    selected_e2e = (
        [LIVE_E2E]
        if args.stage == "live-e2e"
        else [CANDIDATE_E2E]
        if args.stage == "candidate-e2e"
        else [args.test]
        if args.test
        else E2E
    )
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    before = identities()
    manifest = {
        "stage": args.stage,
        "requested_order": ORDER,
        "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "head": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "source_sha256_before": before,
        "scope": "debug-profile components or mock-provider E2E; not live-model A/B",
    }
    env = os.environ.copy()
    if args.stage == "live-e2e":
        home = Path(env["CODEX_HOME"])
        config = tomllib.loads((home / "config.toml").read_text(encoding="utf-8"))
        if not (home / "auth.json").is_file() or not config.get("model"):
            raise SystemExit(
                "Active fork home requires login and an explicit configured model"
            )
        env.update(
            KD4_LIVE_CONFIRM="1",
            KD4_LIVE_AUTH_PATH=str(home / "auth.json"),
            KD4_LIVE_MODEL=config["model"],
            KD4_LIVE_EFFORT=config.get("model_reasoning_effort", "medium"),
            KD4_LIVE_OUTPUT=str(output),
        )
        manifest.update(
            scope="integrated live model; test-only provider-boundary adapter",
            model=config["model"],
            reasoning_effort=env["KD4_LIVE_EFFORT"],
            tool_mode="mixed code mode override for finding 2",
        )
    if args.stage == "components":
        env["KD4_TOKEN_CACHE_BENCH_OUTPUT"] = str(output / "records.jsonl")
        command = [
            "just",
            "_core-test-reserved",
            "fast",
            "core_lib",
            "--run-ignored",
            "only",
            "-E",
            f"test(={BENCH})",
        ]
    else:
        expression = " | ".join(
            f"test(=suite::code_mode::{name})" for name in selected_e2e
        )
        command = [
            "just",
            "_core-test-reserved",
            "local",
            "core_code_mode_mcp",
            "--no-fail-fast",
            "-E",
            expression,
        ]
        manifest["expected_tests"] = selected_e2e
        if args.stage in ("candidate-e2e", "live-e2e"):
            env["KD4_TOKEN_CACHE_E2E_OUTPUT"] = str(output / "records.jsonl")
            command.extend(["--run-ignored", "only"])
    command = [
        sys.executable,
        str(ROOT / "scripts" / "rust_build_status.py"),
        "run-lane",
        "--lane",
        "core-tests",
        "--warm-wait-seconds",
        "600",
        "--",
        *command,
    ]
    manifest["command"] = command
    path = output / "manifest.json"
    path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    start = time.perf_counter()
    with (output / "runner.log").open("w", encoding="utf-8") as log:
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=False,
        )
    manifest.update(
        exit_code=result.returncode,
        elapsed_seconds=time.perf_counter() - start,
        source_sha256_after=identities(),
    )
    manifest["sources_unchanged"] = before == manifest["source_sha256_after"]
    path.write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    print(
        json.dumps(
            {
                "stage": args.stage,
                "exit_code": result.returncode,
                "sources_unchanged": manifest["sources_unchanged"],
                "output": str(output),
            }
        ),
        flush=True,
    )
    if result.returncode:
        raise SystemExit(result.returncode)
    if not manifest["sources_unchanged"]:
        raise SystemExit(
            "Source changed during measurement; inspect manifest before relying on results"
        )
    if args.stage != "components":
        log = (output / "runner.log").read_text(encoding="utf-8")
        log = re.sub(r"\x1b\[[0-9;]*m", "", log)
        summaries = re.findall(
            r"Summary\s+\[[^\]]+\]\s+(\d+) tests? run:\s+(\d+) passed", log
        )
        if not summaries or summaries[-1] != (
            str(len(selected_e2e)),
            str(len(selected_e2e)),
        ):
            raise SystemExit(
                f"Expected {len(selected_e2e)} executed/passing E2E tests, observed {summaries}"
            )
    if args.stage in ("components", "candidate-e2e", "live-e2e"):
        records = [
            json.loads(line)
            for line in (output / "records.jsonl")
            .read_text(encoding="utf-8")
            .splitlines()
        ]
        if args.stage in ("candidate-e2e", "live-e2e"):
            if [record["candidate"] for record in records] != [False, True]:
                raise SystemExit("Incomplete candidate-on/off E2E pair")
            for record in records:
                print(json.dumps(record), flush=True)
            return
        observed = list(dict.fromkeys(record["finding"] for record in records))
        if observed != ORDER or len(records) != 12:
            raise SystemExit(
                f"Incomplete ordered benchmark: {observed}, {len(records)} records"
            )
        for record in records:
            print(
                f"#{record['finding']} {record['case']}: "
                f"{record['baseline_tokens']} -> {record['candidate_tokens']} tokens",
                flush=True,
            )


if __name__ == "__main__":
    main()
