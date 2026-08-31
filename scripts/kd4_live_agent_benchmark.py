#!/usr/bin/env python3
"""Run a minimal paired live-agent benchmark for KD4 versus official upstream C."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import re
import shutil
import statistics
import subprocess
import tempfile
import threading
import time
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


TASK_PROMPT = """Fix `parse_duration` in `duration.py` so it satisfies the contract in
`README.md`. Work until the implementation is correct and the tests pass. Modify only
`duration.py`, and run the tests before finishing.
"""

FIXTURE_FILES = {
    ".gitignore": "__pycache__/\n*.pyc\n",
    "AGENTS.md": """# Task instructions

- Modify only `duration.py`.
- Use only the Python standard library.
- Run `python -m unittest -q` before finishing.
""",
    "README.md": """# Duration parser

Implement `parse_duration(text)` in `duration.py`.

- Return the duration as an integer number of milliseconds.
- Accepted units are `h`, `m`, `s`, and `ms`.
- A duration contains one or more non-negative integer components.
- Components may touch or be separated by whitespace.
- Components must be ordered from largest to smallest unit and each unit may appear once.
- Leading and trailing whitespace is allowed.
- Empty strings, decimals, signs, unsupported units, repeated units, out-of-order units,
  and any unmatched text must raise `ValueError`.

Examples: `500ms`, `2m15s`, and `1h 30m 4ms`.
""",
    "duration.py": """import re


_MULTIPLIERS = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}


def parse_duration(text: str) -> int:
    match = re.fullmatch(r"(\\d+)(ms|s|m|h)", text.strip())
    if match is None:
        raise ValueError("invalid duration")
    value, unit = match.groups()
    return int(value) * _MULTIPLIERS[unit]
""",
    "test_duration.py": """import unittest

from duration import parse_duration


class ParseDurationTests(unittest.TestCase):
    def test_single_component(self):
        self.assertEqual(parse_duration("12s"), 12_000)
        self.assertEqual(parse_duration("500ms"), 500)

    def test_compound_components(self):
        self.assertEqual(parse_duration("2m15s"), 135_000)
        self.assertEqual(parse_duration("1h 30m 4ms"), 5_400_004)

    def test_whitespace(self):
        self.assertEqual(parse_duration("  1h   2m 3s 4ms  "), 3_723_004)

    def test_invalid_input(self):
        for text in ("", "1.5s", "-1s", "1x", "1s junk", "1s2m", "1m1m"):
            with self.subTest(text=text):
                with self.assertRaises(ValueError):
                    parse_duration(text)


if __name__ == "__main__":
    unittest.main()
""",
}

CORRECT_IMPLEMENTATION = """import re


_MULTIPLIERS = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}
_RANKS = {"h": 3, "m": 2, "s": 1, "ms": 0}
_COMPONENT = re.compile(r"(\\d+)(ms|[hms])")


def parse_duration(text: str) -> int:
    if not isinstance(text, str):
        raise ValueError("invalid duration")
    position = 0
    total = 0
    previous_rank = 4
    matched = False
    while position < len(text):
        while position < len(text) and text[position].isspace():
            position += 1
        if position == len(text):
            break
        component = _COMPONENT.match(text, position)
        if component is None:
            raise ValueError("invalid duration")
        value, unit = component.groups()
        rank = _RANKS[unit]
        if rank >= previous_rank:
            raise ValueError("invalid duration")
        total += int(value) * _MULTIPLIERS[unit]
        previous_rank = rank
        matched = True
        position = component.end()
    if not matched:
        raise ValueError("invalid duration")
    return total
"""

VALID_CASES = {
    "0ms": 0,
    "1ms": 1,
    "500ms": 500,
    "1s": 1_000,
    "2m15s": 135_000,
    "1h30m": 5_400_000,
    "1h 30m 4ms": 5_400_004,
    " 1h   2m 3s 4ms ": 3_723_004,
}

INVALID_CASES = (
    "",
    "   ",
    "-1s",
    "+1s",
    "1.5s",
    "1x",
    "1s junk",
    "junk 1s",
    "1s2m",
    "1m1m",
    "1ms1s",
)

REPORT_SCHEMA_VERSION = 2
REQUIRED_TEST_COMMAND = "python -m unittest -q"
_REQUIRED_TEST_PATTERN = re.compile(
    r"(?i)(?:^|[\s;&|])(?:python(?:\d+(?:\.\d+)*)?(?:\.exe)?|py(?:\.exe)?)"
    r"\s+-m\s+unittest\s+-q(?:\s|$|[;&|])"
)

_DIAGNOSTIC_PATTERNS: dict[str, tuple[tuple[str, str], ...]] = {
    "schema_rejection": (
        ("argument preflight failed", "argument preflight rejected the call"),
        ("not valid under any of the given schemas", "JSON Schema rejected the call"),
        ("schema rejection", "schema rejection was reported"),
    ),
    "freshness_invalidation": (
        ("stale_workspace_evidence", "workspace evidence was marked stale"),
        ("stale workspace evidence", "workspace evidence was marked stale"),
        ('"force_fresh":true', "the call requested a fresh rerun"),
        ('"force_fresh": true', "the call requested a fresh rerun"),
    ),
    "reuse_suppression": (
        ("unchanged failure", "an unchanged failure was suppressed"),
        ("unchanged-failure", "an unchanged failure was suppressed"),
        ("reuse suppression", "reuse suppression was reported"),
        ("negative cache", "negative-cache reuse was reported"),
    ),
    "unsupported_interactive_tool": (
        (
            "request_user_input is not supported in exec mode",
            "request_user_input was unsupported in exec mode",
        ),
        (
            "request_user_input is unavailable when approval policy is",
            "request_user_input was unavailable under the approval policy",
        ),
        ("unsupported interactive tool", "an interactive tool was unsupported"),
    ),
    "patch_mismatch": (
        ("apply_patch verification failed", "apply_patch verification failed"),
        ("failed to find expected lines", "a patch could not find its expected context"),
        ("patch mismatch", "a patch mismatch was reported"),
    ),
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def text_sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def fixture_manifest() -> dict[str, str]:
    return {name: text_sha256(content) for name, content in sorted(FIXTURE_FILES.items())}


def create_fixture(root: Path) -> str:
    for relative, content in FIXTURE_FILES.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
    env = os.environ.copy()
    env.update(
        {
            "GIT_AUTHOR_DATE": "2000-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2000-01-01T00:00:00Z",
        }
    )
    subprocess.run(["git", "init", "-q", "-b", "main"], cwd=root, check=True, env=env)
    subprocess.run(["git", "add", "."], cwd=root, check=True, env=env)
    subprocess.run(
        [
            "git",
            "-c",
            "user.name=KD4 Benchmark",
            "-c",
            "user.email=benchmark.invalid",
            "commit",
            "-q",
            "-m",
            "benchmark fixture",
        ],
        cwd=root,
        check=True,
        env=env,
    )
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=root, text=True, encoding="utf-8"
    ).strip()


def verify_fixture(root: Path) -> tuple[bool, list[str]]:
    failures: list[str] = []
    for protected in ("README.md", "test_duration.py", "AGENTS.md"):
        actual = sha256(root / protected)
        expected = text_sha256(FIXTURE_FILES[protected])
        if actual != expected:
            failures.append(f"{protected} was modified")

    verifier = f"""
import importlib.util
from pathlib import Path

path = Path({str(root / 'duration.py')!r})
spec = importlib.util.spec_from_file_location('bench_duration', path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
parse_duration = module.parse_duration
valid = {VALID_CASES!r}
invalid = {INVALID_CASES!r}
for text, expected in valid.items():
    actual = parse_duration(text)
    assert type(actual) is int and actual == expected, (text, expected, actual)
for text in invalid:
    try:
        parse_duration(text)
    except ValueError:
        pass
    else:
        raise AssertionError(('expected ValueError', text))
"""
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    hidden = subprocess.run(
        ["python", "-I", "-c", verifier],
        cwd=root,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        env=env,
    )
    if hidden.returncode != 0:
        tail = (hidden.stderr or hidden.stdout).strip().splitlines()[-1:]
        failures.append("external verifier failed" + (f": {tail[0]}" if tail else ""))

    visible = subprocess.run(
        ["python", "-m", "unittest", "-q"],
        cwd=root,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        env=env,
    )
    if visible.returncode != 0:
        failures.append("visible tests failed")
    return not failures, failures


def git_output(root: Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(root), *args], text=True, encoding="utf-8"
    ).strip()


def git_bytes(root: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "-C", str(root), *args])


def exact_source_state(
    root: Path, expected_revision: str, label: str
) -> dict[str, Any]:
    expected_commit = git_output(root, "rev-parse", f"{expected_revision}^{{commit}}")
    head_commit = git_output(root, "rev-parse", "HEAD^{commit}")
    if head_commit != expected_commit:
        raise RuntimeError(
            f"{label} HEAD is {head_commit}, expected {expected_commit}; "
            "refusing to benchmark a different source revision"
        )
    status = git_bytes(
        root, "status", "--porcelain=v1", "-z", "--untracked-files=all"
    )
    if status:
        dirty_paths = status.count(b"\0")
        raise RuntimeError(
            f"{label} source tree has {dirty_paths} dirty paths; commit or remove "
            "every change before building or benchmarking"
        )
    return {
        "commit": head_commit,
        "tree": git_output(root, "rev-parse", "HEAD^{tree}"),
        "clean": True,
        "statusPorcelainV1zSha256": hashlib.sha256(status).hexdigest(),
        "exactContentsReconstructable": True,
    }


def binary_identity(
    path: Path,
    source_root: Path,
    source_state: dict[str, Any],
    label: str,
    *,
    build_command: str | None = None,
) -> dict[str, Any]:
    stat = path.stat()
    identity = {
        "label": label,
        "sourceRoot": str(source_root),
        "revision": source_state["commit"],
        "sourceState": source_state,
        "binary": {
            "path": str(path),
            "sha256": sha256(path),
            "sizeBytes": stat.st_size,
            "mtimeUtc": datetime.fromtimestamp(stat.st_mtime, timezone.utc).isoformat(),
        },
        "buildProfile": "release",
    }
    if build_command is not None:
        identity["recordedBuildCommand"] = build_command
    return identity


def prepare_home(root: Path, auth_source: Path) -> None:
    root.mkdir(parents=True, exist_ok=True)
    shutil.copy2(auth_source, root / "auth.json")


def terminate_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def build_agent_command(
    *,
    binary: Path,
    workspace: Path,
    model: str,
    reasoning_effort: str,
    personality: str,
    code_mode: str,
) -> list[str]:
    code_mode_enabled = "true" if code_mode == "enabled" else "false"
    return [
        str(binary),
        "exec",
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--model",
        model,
        "-c",
        f'model_reasoning_effort="{reasoning_effort}"',
        "-c",
        f'personality="{personality}"',
        "-c",
        'approval_policy="never"',
        "-c",
        f"features.code_mode={code_mode_enabled}",
        "--sandbox",
        "danger-full-access",
        "-C",
        str(workspace),
        TASK_PROMPT,
    ]


def is_required_test_command(command: str) -> bool:
    return _REQUIRED_TEST_PATTERN.search(command) is not None


def classify_diagnostics(
    *,
    observed_text: str,
    timed_out: bool,
    exit_code: int,
    terminal_event: str | None,
    invalid_json_lines: int,
    verifier_passed: bool,
    required_test_passed: bool,
    command_execution_failures: int,
) -> list[dict[str, Any]]:
    lowered = observed_text.lower()
    diagnostics: list[dict[str, Any]] = []
    for category, patterns in _DIAGNOSTIC_PATTERNS.items():
        signals = [signal for needle, signal in patterns if needle in lowered]
        if signals:
            diagnostics.append(
                {"category": category, "signals": sorted(set(signals))}
            )

    execution_signals: list[str] = []
    if command_execution_failures:
        execution_signals.append(
            f"{command_execution_failures} command execution(s) did not complete successfully"
        )
    if timed_out:
        execution_signals.append("the agent process timed out")
    if exit_code != 0:
        execution_signals.append(f"the agent process exited with code {exit_code}")
    if terminal_event != "turn.completed":
        execution_signals.append(f"terminal event was {terminal_event!r}")
    if execution_signals:
        diagnostics.append(
            {"category": "execution_failure", "signals": execution_signals}
        )
    if not verifier_passed:
        diagnostics.append(
            {
                "category": "verifier_failure",
                "signals": ["the post-turn visible or hidden verifier failed"],
            }
        )
    if not required_test_passed:
        diagnostics.append(
            {
                "category": "task_contract_violation",
                "signals": [f"no successful `{REQUIRED_TEST_COMMAND}` attempt was observed"],
            }
        )
    if invalid_json_lines:
        diagnostics.append(
            {
                "category": "invalid_event_stream",
                "signals": [f"{invalid_json_lines} JSONL line(s) were invalid"],
            }
        )
    return diagnostics


def run_agent(
    *,
    binary: Path,
    label: str,
    repetition: int,
    model: str,
    reasoning_effort: str,
    personality: str,
    code_mode: str,
    auth_source: Path,
    timeout_seconds: int,
) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix=f"kd4-live-{label}-{repetition}-") as temp:
        temp_root = Path(temp)
        workspace = temp_root / "workspace"
        workspace.mkdir()
        fixture_revision = create_fixture(workspace)
        home = temp_root / "home"
        prepare_home(home, auth_source)

        command = build_agent_command(
            binary=binary,
            workspace=workspace,
            model=model,
            reasoning_effort=reasoning_effort,
            personality=personality,
            code_mode=code_mode,
        )
        env = os.environ.copy()
        env["CODEX_HOME"] = str(home)
        env["RUST_LOG"] = "error"
        env["NO_COLOR"] = "1"
        creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0)

        started_ns = time.perf_counter_ns()
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
            env=env,
            creationflags=creationflags,
        )
        assert process.stdout and process.stderr
        stdout_queue: queue.Queue[tuple[int, str] | None] = queue.Queue()
        stderr_lines: list[str] = []

        def read_stdout() -> None:
            assert process.stdout
            for line in process.stdout:
                stdout_queue.put((time.perf_counter_ns(), line.rstrip()))
            stdout_queue.put(None)

        def read_stderr() -> None:
            assert process.stderr
            stderr_lines.extend(line.rstrip() for line in process.stderr)

        stdout_thread = threading.Thread(target=read_stdout, daemon=True)
        stderr_thread = threading.Thread(target=read_stderr, daemon=True)
        stdout_thread.start()
        stderr_thread.start()

        deadline = time.monotonic() + timeout_seconds
        event_counts: Counter[str] = Counter()
        item_counts: Counter[str] = Counter()
        invalid_json_lines = 0
        first_output_ns: int | None = None
        terminal_ns: int | None = None
        terminal_event: str | None = None
        final_message: str | None = None
        timed_out = False
        observed_text_parts: list[str] = []
        required_test_attempts: list[dict[str, Any]] = []
        command_execution_failures = 0

        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                timed_out = True
                terminate_process(process)
                break
            try:
                queued = stdout_queue.get(timeout=min(0.25, remaining))
            except queue.Empty:
                if process.poll() is not None and not stdout_thread.is_alive():
                    break
                continue
            if queued is None:
                break
            observed_ns, line = queued
            if not line:
                continue
            observed_text_parts.append(line[:20_000])
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                invalid_json_lines += 1
                continue
            event_type = str(event.get("type", "unknown"))
            event_counts[event_type] += 1
            item = event.get("item") or {}
            item_type = item.get("type")
            if item_type:
                item_counts[str(item_type)] += 1
            if (
                first_output_ns is None
                and event_type in {"item.started", "item.completed"}
                and item_type != "error"
            ):
                first_output_ns = observed_ns
            if event_type == "item.completed" and item_type == "agent_message":
                final_message = item.get("text")
            if event_type == "item.completed" and item_type == "command_execution":
                command_value = item.get("command", "")
                command_text = (
                    " ".join(str(part) for part in command_value)
                    if isinstance(command_value, list)
                    else str(command_value)
                )
                command_status = item.get("status")
                command_exit_code = item.get("exit_code")
                command_passed = (
                    command_status in {None, "completed"} and command_exit_code == 0
                )
                if command_status == "failed" or (
                    command_exit_code is not None and command_exit_code != 0
                ):
                    command_execution_failures += 1
                if is_required_test_command(command_text):
                    required_test_attempts.append(
                        {
                            "command": command_text[:1_000],
                            "status": command_status,
                            "exitCode": command_exit_code,
                            "passed": command_passed,
                        }
                    )
            if event_type in {"turn.completed", "turn.failed"} and terminal_ns is None:
                terminal_ns = observed_ns
                terminal_event = event_type

        if process.poll() is None:
            terminate_process(process)
        exit_code = process.wait()
        ended_ns = time.perf_counter_ns()
        stdout_thread.join(timeout=1)
        stderr_thread.join(timeout=1)
        verifier_passed, verifier_failures = verify_fixture(workspace)

        reasons: list[str] = []
        if timed_out:
            reasons.append("agent timed out")
        if exit_code != 0:
            reasons.append(f"agent exited with code {exit_code}")
        if terminal_event != "turn.completed":
            reasons.append(f"terminal event was {terminal_event!r}")
        if invalid_json_lines:
            reasons.append(f"agent emitted {invalid_json_lines} invalid JSONL lines")
        reasons.extend(verifier_failures)
        outcome_correct = not reasons and verifier_passed
        required_test_passed = any(
            attempt["passed"] for attempt in required_test_attempts
        )
        task_contract_compliant = outcome_correct and required_test_passed
        diagnostic_text = "\n".join(
            [
                *observed_text_parts,
                *stderr_lines,
                *reasons,
                final_message or "",
            ]
        )
        diagnostics = classify_diagnostics(
            observed_text=diagnostic_text,
            timed_out=timed_out,
            exit_code=exit_code,
            terminal_event=terminal_event,
            invalid_json_lines=invalid_json_lines,
            verifier_passed=verifier_passed,
            required_test_passed=required_test_passed,
            command_execution_failures=command_execution_failures,
        )
        completion_ns = (terminal_ns or ended_ns) - started_ns
        ttfo_ns = None if first_output_ns is None else first_output_ns - started_ns
        return {
            "variant": label,
            "repetition": repetition,
            "fixtureRevision": fixture_revision,
            "success": outcome_correct,
            "outcomeCorrect": outcome_correct,
            "taskContractCompliant": task_contract_compliant,
            "failureReasons": reasons,
            "complianceFailureReasons": (
                []
                if task_contract_compliant
                else [
                    reason
                    for reason in (
                        *reasons,
                        None
                        if required_test_passed
                        else f"no successful `{REQUIRED_TEST_COMMAND}` attempt was observed",
                    )
                    if reason is not None
                ]
            ),
            "taskContract": {
                "requiredTestCommand": REQUIRED_TEST_COMMAND,
                "successfulTestObserved": required_test_passed,
                "testAttempts": required_test_attempts,
            },
            "diagnostics": diagnostics,
            "completionMs": round(completion_ns / 1_000_000, 3),
            "ttfoMs": None if ttfo_ns is None else round(ttfo_ns / 1_000_000, 3),
            "exitCode": exit_code,
            "terminalEvent": terminal_event,
            "eventCounts": dict(sorted(event_counts.items())),
            "itemCounts": dict(sorted(item_counts.items())),
            "finalMessage": None if final_message is None else final_message[:500],
            "verifierPassed": verifier_passed,
            "stderrTail": (
                "\n".join(stderr_lines[-10:])[-2000:]
                if not task_contract_compliant
                else ""
            ),
        }


def distribution(values: list[float]) -> dict[str, Any] | None:
    if not values:
        return None
    return {
        "count": len(values),
        "averageMs": round(statistics.fmean(values), 3),
        "medianMs": round(statistics.median(values), 3),
        "minMs": round(min(values), 3),
        "maxMs": round(max(values), 3),
    }


def summarize(runs: list[dict[str, Any]]) -> dict[str, Any]:
    outcome_correct = [
        run for run in runs if run.get("outcomeCorrect", run.get("success", False))
    ]
    contract_compliant = [
        run for run in runs if run.get("taskContractCompliant", False)
    ]
    completion = [run["completionMs"] for run in outcome_correct]
    ttfo = [run["ttfoMs"] for run in outcome_correct if run["ttfoMs"] is not None]
    diagnostic_counts = Counter(
        diagnostic["category"]
        for run in runs
        for diagnostic in run.get("diagnostics", [])
    )
    outcome_rate = round(len(outcome_correct) / len(runs) * 100, 3)
    compliance_rate = round(len(contract_compliant) / len(runs) * 100, 3)
    return {
        "runs": len(runs),
        # Retained for schema-v1 consumers: success means outcome correctness.
        "successfulRuns": len(outcome_correct),
        "failedRuns": len(runs) - len(outcome_correct),
        "successRatePercent": outcome_rate,
        "successfulCompletionTime": distribution(completion),
        "successfulTtfo": distribution(ttfo),
        "outcomeCorrectness": {
            "correctRuns": len(outcome_correct),
            "incorrectRuns": len(runs) - len(outcome_correct),
            "ratePercent": outcome_rate,
            "correctCompletionTime": distribution(completion),
            "correctTtfo": distribution(ttfo),
        },
        "taskContractCompliance": {
            "compliantRuns": len(contract_compliant),
            "noncompliantRuns": len(runs) - len(contract_compliant),
            "ratePercent": compliance_rate,
        },
        "diagnosticCategoryCounts": dict(sorted(diagnostic_counts.items())),
    }


def make_report(args: argparse.Namespace) -> dict[str, Any]:
    fork_binary = args.fork_binary.resolve()
    upstream_binary = args.upstream_binary.resolve()
    fork_root = args.fork_root.resolve()
    upstream_root = args.upstream_root.resolve()
    auth_source = args.auth_source.resolve()
    for path in (fork_binary, upstream_binary, auth_source):
        if not path.is_file():
            raise FileNotFoundError(path)
    fork_source_state = exact_source_state(
        fork_root, args.fork_revision, "current fork"
    )
    upstream_source_state = exact_source_state(
        upstream_root, args.upstream_revision, "official upstream C"
    )

    pairs: list[dict[str, Any]] = []
    for repetition in range(1, args.repetitions + 1):
        upstream_first = repetition % 2 == 1
        order = (
            (("upstreamC", upstream_binary), ("currentFork", fork_binary))
            if upstream_first
            else (("currentFork", fork_binary), ("upstreamC", upstream_binary))
        )
        runs: dict[str, dict[str, Any]] = {}
        for label, binary in order:
            print(f"pair {repetition}/{args.repetitions}: starting {label}", flush=True)
            run = run_agent(
                binary=binary,
                label=label,
                repetition=repetition,
                model=args.model,
                reasoning_effort=args.reasoning_effort,
                personality=args.personality,
                code_mode=args.code_mode,
                auth_source=auth_source,
                timeout_seconds=args.timeout_seconds,
            )
            runs[label] = run
            print(
                f"pair {repetition}/{args.repetitions}: {label} "
                f"outcomeCorrect={run['outcomeCorrect']} "
                f"taskContractCompliant={run['taskContractCompliant']} "
                f"completionMs={run['completionMs']} "
                f"ttfoMs={run['ttfoMs']} failures={run['failureReasons']}",
                flush=True,
            )
        pairs.append(
            {
                "repetition": repetition,
                "order": "upstreamC,currentFork" if upstream_first else "currentFork,upstreamC",
                "currentFork": runs["currentFork"],
                "upstreamC": runs["upstreamC"],
            }
        )

    fork_runs = [pair["currentFork"] for pair in pairs]
    upstream_runs = [pair["upstreamC"] for pair in pairs]
    if exact_source_state(fork_root, args.fork_revision, "current fork") != fork_source_state:
        raise RuntimeError("current fork source state changed during the benchmark")
    if (
        exact_source_state(upstream_root, args.upstream_revision, "official upstream C")
        != upstream_source_state
    ):
        raise RuntimeError("official upstream C source state changed during the benchmark")
    return {
        "schemaVersion": REPORT_SCHEMA_VERSION,
        "kind": "fork-vs-official-upstream-c-live-agent-task",
        "capturedAt": datetime.now(timezone.utc).isoformat(),
        "scope": "input-identical live coding task",
        "currentFork": {
            **binary_identity(
                fork_binary,
                fork_root,
                fork_source_state,
                "current fork",
                build_command=args.fork_build_command,
            ),
        },
        "upstreamC": {
            **binary_identity(
                upstream_binary,
                upstream_root,
                upstream_source_state,
                "official upstream C",
            ),
            "immutableReference": True,
        },
        "methodology": {
            "taskPrompt": TASK_PROMPT,
            "fixtureManifestSha256": text_sha256(
                json.dumps(fixture_manifest(), sort_keys=True, separators=(",", ":"))
            ),
            "fixtureFiles": fixture_manifest(),
            "outcomeCorrectnessCheck": "unchanged task files, visible unittest suite, and external hidden cases",
            "taskContractComplianceCheck": (
                f"outcome correctness plus an observed successful `{REQUIRED_TEST_COMMAND}` "
                "command execution before the turn finished"
            ),
            "legacySuccessField": "retained as an alias for outcomeCorrect so schema-v1 outcome rates remain comparable",
            "sourceIdentityCheck": (
                "each supplied revision must equal HEAD and `git status --porcelain=v1 "
                "-z --untracked-files=all` must be empty before and after all runs"
            ),
            "repetitionsPerVariant": args.repetitions,
            "pairOrder": "alternating; upstream first in odd repetitions",
            "model": args.model,
            "reasoningEffort": args.reasoning_effort,
            "personality": args.personality,
            "codeMode": f"{args.code_mode}; explicitly pinned for both variants",
            "approvalPolicy": "never",
            "sandbox": "danger-full-access",
            "home": "fresh CODEX_HOME per run with the same auth source and no user config",
            "completionMetric": "process launch through turn.completed JSONL event",
            "ttfoMetric": "process launch through first non-error item.started or item.completed JSONL event",
            "timeoutSeconds": args.timeout_seconds,
            "latencyInterpretation": (
                "This benchmark distinguishes TTFO from post-TTFO completion. Similar "
                "TTFO with much slower completion is evidence of a post-TTFO "
                "harness/tool-loop problem; it does not establish an inference-speed problem."
            ),
        },
        "results": {
            "currentFork": summarize(fork_runs),
            "upstreamC": summarize(upstream_runs),
            "pairs": pairs,
        },
        "limitations": [
            (
                "This is one fixed coding task repeated "
                f"{args.repetitions} times per variant, not a broad task suite."
            ),
            "Live model behavior is intentionally stochastic even with identical inputs and settings.",
            "Completion-time summaries include outcome-correct runs only; every incorrect or noncompliant run remains explicit in the pair records.",
            "The external verifier adjudicates success after the timed agent turn and is not included in completion time.",
            "The recorded build command is provenance supplied by the benchmark operator; the report independently binds the candidate to a clean commit/tree and binary SHA-256.",
        ],
    }


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="kd4-live-self-test-") as temp:
        root_a = Path(temp) / "a"
        root_b = Path(temp) / "b"
        root_a.mkdir()
        root_b.mkdir()
        revision_a = create_fixture(root_a)
        revision_b = create_fixture(root_b)
        assert revision_a == revision_b
        source_state = exact_source_state(root_a, revision_a, "self-test fixture")
        assert source_state["commit"] == revision_a
        assert source_state["clean"]
        assert not verify_fixture(root_a)[0]
        (root_a / "duration.py").write_text(
            CORRECT_IMPLEMENTATION, encoding="utf-8", newline="\n"
        )
        passed, failures = verify_fixture(root_a)
        assert passed, failures
    summary = summarize(
        [
            {
                "success": True,
                "outcomeCorrect": True,
                "taskContractCompliant": True,
                "completionMs": 100.0,
                "ttfoMs": 10.0,
                "diagnostics": [],
            },
            {
                "success": True,
                "outcomeCorrect": True,
                "taskContractCompliant": False,
                "completionMs": 300.0,
                "ttfoMs": 30.0,
                "diagnostics": [{"category": "task_contract_violation"}],
            },
            {
                "success": False,
                "outcomeCorrect": False,
                "taskContractCompliant": False,
                "completionMs": 50.0,
                "ttfoMs": None,
                "diagnostics": [{"category": "verifier_failure"}],
            },
        ]
    )
    assert summary["successRatePercent"] == 66.667
    assert summary["outcomeCorrectness"]["ratePercent"] == 66.667
    assert summary["taskContractCompliance"]["ratePercent"] == 33.333
    assert summary["successfulCompletionTime"]["averageMs"] == 200.0
    assert summary["successfulCompletionTime"]["medianMs"] == 200.0
    assert summary["diagnosticCategoryCounts"] == {
        "task_contract_violation": 1,
        "verifier_failure": 1,
    }
    assert is_required_test_command("python -m unittest -q")
    assert is_required_test_command("python.exe -m unittest -q")
    assert not is_required_test_command("python -m unittest")
    command = build_agent_command(
        binary=Path("codex"),
        workspace=Path("workspace"),
        model="test-model",
        reasoning_effort="high",
        personality="pragmatic",
        code_mode="enabled",
    )
    assert command.count("features.code_mode=true") == 1
    assert command[command.index("features.code_mode=true") - 1] == "-c"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--fork-binary", type=Path)
    parser.add_argument("--upstream-binary", type=Path)
    parser.add_argument("--fork-root", type=Path)
    parser.add_argument("--fork-revision")
    parser.add_argument("--fork-build-command")
    parser.add_argument("--upstream-root", type=Path)
    parser.add_argument("--upstream-revision")
    parser.add_argument("--auth-source", type=Path, default=Path.home() / ".codex" / "auth.json")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--model", default="gpt-5.6-sol")
    parser.add_argument("--reasoning-effort", default="high")
    parser.add_argument("--personality", default="pragmatic")
    parser.add_argument(
        "--code-mode", choices=("enabled", "disabled"), default="enabled"
    )
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--timeout-seconds", type=int, default=600)
    args = parser.parse_args()
    if args.self_test:
        return args
    required = (
        "fork_binary",
        "upstream_binary",
        "fork_root",
        "fork_revision",
        "fork_build_command",
        "upstream_root",
        "upstream_revision",
        "output",
    )
    missing = [name for name in required if getattr(args, name) is None]
    if missing:
        parser.error("missing required arguments: " + ", ".join(missing))
    if args.repetitions <= 0 or args.timeout_seconds <= 0:
        parser.error("repetitions and timeout-seconds must be positive")
    return args


def main() -> None:
    args = parse_args()
    if args.self_test:
        self_test()
        print("self-test passed")
        return
    report = make_report(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report["results"], indent=2), flush=True)


if __name__ == "__main__":
    main()
