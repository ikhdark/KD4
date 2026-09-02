from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
import time
import unittest
from io import StringIO
from pathlib import Path
from unittest import mock

from scripts import kd4_live_agent_benchmark as benchmark


def _pid_is_running(pid: int) -> bool:
    """Whether a pid is still live, without reaping anything we do not own."""
    if os.name == "nt":
        probe = subprocess.run(
            ["tasklist", "/FI", f"PID eq {pid}", "/NH"],
            capture_output=True,
            text=True,
            check=False,
        )
        return str(pid) in probe.stdout
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _run(*, outcome_correct: bool, task_contract_compliant: bool) -> dict[str, object]:
    return {
        "success": outcome_correct,
        "outcomeCorrect": outcome_correct,
        "taskContractCompliant": task_contract_compliant,
        "completionMs": 100.0,
        "wallClockMs": 110.0,
        "ttfoMs": 10.0,
        "diagnostics": [],
        "modelWaitMs": 75.0,
        "continuationCount": 2,
        "actualCommandCount": 4,
        "taskContract": {"successfulTestObserved": task_contract_compliant},
    }


def _gate_run(**overrides: object) -> dict[str, object]:
    run: dict[str, object] = {
        "success": True,
        "outcomeCorrect": True,
        "taskContractCompliant": True,
        "terminalEvent": "turn.completed",
        "completionMs": 100.0,
        "modelWaitMs": 75.0,
        "actualCommandCount": 4,
        "duplicateCommandCount": 0,
        "taskContract": {"successfulTestObserved": True},
        "latencyExplanation": {
            "observed": {
                "postFirstOutputMs": 90.0,
                "firstWorkspaceMutationObservedMs": 20.0,
                "firstRequiredTestCompletedObservedMs": 80.0,
                "requiredTestToTerminalMs": 20.0,
            },
            "instrumentedRuntime": {
                "available": True,
                "counters": {"logicalGenerationCount": 2},
                "tokenTotalsAcrossRequests": {"totalTokens": 100},
            },
        },
    }
    run.update(overrides)
    return run


def _gate_pairs() -> list[dict[str, object]]:
    return [
        {
            "taskId": task.task_id,
            "taskShape": task.shape,
            "repetition": repetition,
            "currentFork": _gate_run(),
            "upstreamC": _gate_run(),
        }
        for task in benchmark.BENCHMARK_TASKS
        for repetition in range(1, benchmark.MIN_GATE_REPETITIONS_PER_TASK + 1)
    ]


def _model_request(**overrides: object) -> dict[str, object]:
    request = {
        "generationIndex": 0,
        "generationReason": "initial",
        "generationPurpose": "implementation",
        "disposition": "decision_bearing",
        "attemptKind": "primary",
        "isContinuation": False,
        "progressKinds": ["workspace_mutation"],
        "unchangedRelevantState": False,
        "nextStructuredActionChanged": True,
        "modelStreamWaitNs": 1_000_000_000,
        "samplingRequestId": "req-0",
        "physicalAttemptIds": ["attempt-0"],
        "dispatchMs": 0,
    }
    request.update(overrides)
    return request


def _tool_call(**overrides: object) -> dict[str, object]:
    call = {
        "callId": "call-0",
        "toolName": "exec",
        "generationIndex": 0,
        "outcome": "success",
        "acceptedAtMs": 100,
        "handlerEntryAtMs": 100,
        "processSpawnedAtMs": 100,
        "handlerExitAtMs": 1000,
        "outputModelVisibleAtMs": 1000,
    }
    call.update(overrides)
    return call


def _record_commands(
    specs: list[tuple[str, str, int]],
) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    """Feed `item.started`/`item.completed` pairs through the lifecycle recorder."""
    records: dict[str, dict[str, object]] = {}
    order: list[str] = []
    item_events: list[dict[str, object]] = []
    sequence = 0
    for index, (item_id, command, exit_code) in enumerate(specs):
        for phase, observed_ms in (
            ("item.started", 10.0 + index * 100),
            ("item.completed", 50.0 + index * 100),
        ):
            sequence += 1
            completed = phase == "item.completed"
            benchmark.record_command_event(
                records=records,
                order=order,
                event_type=phase,
                item={
                    "id": item_id,
                    "type": "command_execution",
                    "command": command,
                    "status": "completed" if completed else "in_progress",
                    "exit_code": exit_code if completed else None,
                },
                sequence=sequence,
                observed_ms=observed_ms,
                observed_at_unix_ms=1_700_000_000_000.0 + observed_ms,
            )
            item_events.append(
                {
                    "sequence": sequence,
                    "eventType": phase,
                    "itemId": item_id,
                    "itemType": "command_execution",
                    "observedMs": observed_ms,
                    "observedAtUnixMs": 1_700_000_000_000.0 + observed_ms,
                }
            )
    return [records[key] for key in order], item_events


def _trace(
    *,
    timing: dict[str, object] | None,
    commands: list[dict[str, object]],
    item_events: list[dict[str, object]],
    terminal_event: str | None = "turn.completed",
    timed_out: bool = False,
) -> dict[str, object]:
    return benchmark.build_turn_trace(
        terminal_event=terminal_event,
        terminal_payload=None if timing is None else {"timing": timing},
        timed_out=timed_out,
        process_started_at_unix_ms=1_700_000_000_000.0,
        wall_clock_ms=9_000.0,
        turn_started_observed_ms=0.0,
        terminal_observed_ms=None if terminal_event is None else 8_000.0,
        commands=commands,
        item_events=item_events,
    )


class Kd4LiveAgentBenchmarkTest(unittest.TestCase):
    def test_turn_measurements_use_union_wait_and_continuation_flags(self) -> None:
        event = {
            "type": "turn.completed",
            "timing": {
                "unions": {"modelStreamWaitUnionNs": 12_345_678},
                "modelRequests": [
                    {"isContinuation": False},
                    {"isContinuation": True},
                    {"isContinuation": True},
                ],
            },
        }

        self.assertEqual(benchmark.turn_measurements(event), (12.346, 2))
        self.assertEqual(
            benchmark.turn_measurements({"type": "turn.completed"}),
            (None, None),
        )

    def test_required_test_command_requires_quiet_unittest_execution(self) -> None:
        accepted = (
            "python -m unittest -q",
            "/usr/bin/python -m unittest -q",
            '"C:\\Program Files\\Python\\python.exe" -m unittest -q',
            "$env:PYTHONDONTWRITEBYTECODE=1; python.exe -m unittest -q",
        )
        rejected = (
            "python -m unittest",
            "python -m pytest -q",
            "echo python -m unittest -q",
            "python -m unittest -q -k nothing_matches",
            "python -m unittest -q -p test_none.py",
            "python -m unittest -q test_duration",
        )

        for command in accepted:
            with self.subTest(command=command):
                self.assertTrue(benchmark.is_required_test_command(command))
        for command in rejected:
            with self.subTest(command=command):
                self.assertFalse(benchmark.is_required_test_command(command))

    def test_fixture_ignores_global_git_config_and_persists_lf_checkout(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-git-config-") as temp:
            temp_root = Path(temp)
            global_config = temp_root / "global.gitconfig"
            global_config.write_text(
                "[core]\n\tautocrlf = true\n[init]\n\ttemplateDir = missing-template\n",
                encoding="utf-8",
                newline="\n",
            )
            env = {"GIT_CONFIG_GLOBAL": str(global_config)}
            with mock.patch.dict(os.environ, env):
                roots = (temp_root / "a", temp_root / "b")
                for root in roots:
                    root.mkdir()
                revisions = [benchmark.create_fixture(root) for root in roots]

                self.assertEqual(revisions[0], revisions[1])
                local_autocrlf = subprocess.check_output(
                    ["git", "config", "--local", "--get", "core.autocrlf"],
                    cwd=roots[0],
                    text=True,
                    encoding="utf-8",
                ).strip()
                self.assertEqual(local_autocrlf, "false")

                readme = roots[0] / "README.md"
                readme.unlink()
                subprocess.run(
                    ["git", "checkout", "--", "README.md"],
                    cwd=roots[0],
                    check=True,
                    env=os.environ.copy(),
                    capture_output=True,
                )
                self.assertNotIn(b"\r\n", readme.read_bytes())

    def test_protected_fixture_check_normalizes_newlines(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-newlines-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content.replace("\n", "\r\n").encode("utf-8"))

            self.assertEqual(benchmark.protected_fixture_failures(root), [])

    def test_external_verifier_disables_bytecode_under_isolated_mode(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-verifier-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8", newline="\n")
            (root / "duration.py").write_text(
                benchmark.CORRECT_IMPLEMENTATION, encoding="utf-8", newline="\n"
            )
            passed, failures = benchmark.verify_fixture(root)

            self.assertTrue(passed, failures)
            self.assertFalse((root / "__pycache__").exists())

    def test_hidden_cases_cover_ambiguous_spacing_order_and_unicode_digits(
        self,
    ) -> None:
        for invalid in ("1 s", "1ms 1s", "1s 1s", "30ms1m", "\u0661s"):
            with self.subTest(invalid=invalid):
                self.assertIn(invalid, benchmark.INVALID_CASES)

    def test_every_benchmark_task_has_a_working_independent_verifier(self) -> None:
        solutions = {
            "duration_parser": benchmark.CORRECT_IMPLEMENTATION,
            "slug_diagnostic": """import re


def normalize_slug(value: str) -> str:
    if not isinstance(value, str) or not value.isascii():
        raise ValueError("invalid slug")
    slug = re.sub(r"[^A-Za-z0-9]+", "-", value.strip()).strip("-").lower()
    if not slug:
        raise ValueError("invalid slug")
    return slug
""",
            "inventory_multi_file": {
                "inventory/parser.py": """def parse_rows(text: str) -> list[tuple[str, int]]:
    rows = []
    for line in text.splitlines():
        if not line.strip():
            continue
        parts = line.split(",")
        if len(parts) != 2:
            raise ValueError("invalid row")
        name, quantity = (part.strip() for part in parts)
        if not name or not quantity.isascii() or not quantity.isdecimal():
            raise ValueError("invalid row")
        rows.append((name, int(quantity)))
    return rows
""",
                "inventory/report.py": """from .parser import parse_rows


def render_report(text: str) -> str:
    rows = parse_rows(text)
    lines = [f"{name}: {quantity}" for name, quantity in rows]
    lines.append(f"TOTAL: {sum(quantity for _, quantity in rows)}")
    return "\\n".join(lines)
""",
            },
        }
        with tempfile.TemporaryDirectory(prefix="kd4-task-suite-") as temp:
            for task in benchmark.BENCHMARK_TASKS:
                with self.subTest(task=task.task_id):
                    root = Path(temp) / task.task_id
                    root.mkdir()
                    benchmark.create_fixture(root, task)
                    self.assertFalse(benchmark.verify_fixture(root, task)[0])
                    solution = solutions[task.task_id]
                    if isinstance(solution, str):
                        self.assertEqual(len(task.editable_files), 1)
                        (root / task.editable_files[0]).write_text(
                            solution, encoding="utf-8", newline="\n"
                        )
                    else:
                        for relative, content in solution.items():
                            (root / relative).write_text(
                                content, encoding="utf-8", newline="\n"
                            )
                    passed, failures = benchmark.verify_fixture(root, task)
                    self.assertTrue(passed, failures)

    def test_elapsed_measurements_require_a_terminal_event_for_completion(self) -> None:
        self.assertEqual(
            benchmark.elapsed_measurements(
                started_ns=100_000_000,
                ended_ns=700_000_000,
                terminal_ns=None,
            ),
            (None, 600.0),
        )
        self.assertEqual(
            benchmark.elapsed_measurements(
                started_ns=100_000_000,
                ended_ns=700_000_000,
                terminal_ns=600_000_000,
            ),
            (500.0, 600.0),
        )

    def test_timing_comparison_marks_uninstrumented_variant_unavailable(self) -> None:
        fork = benchmark.summarize(
            [_run(outcome_correct=True, task_contract_compliant=True)]
        )
        upstream_run = _run(outcome_correct=True, task_contract_compliant=True)
        upstream_run["modelWaitMs"] = None
        upstream_run["continuationCount"] = None
        upstream = benchmark.summarize([upstream_run])

        comparison = benchmark.timing_metric_comparability(
            fork_label="guidance-on",
            fork_summary=fork,
            upstream_label="official-upstream",
            upstream_summary=upstream,
        )

        self.assertFalse(comparison["headToHeadComparable"])
        self.assertEqual(comparison["unavailableVariants"], ["official-upstream"])

    def test_comparison_latency_explanation_states_the_measured_mechanism(self) -> None:
        fork = {
            "successfulCompletionTime": {"medianMs": 100_000.0},
            "actualCommandCount": {"median": 8.0},
            "latencyExplanation": {
                "harnessObserved": {
                    "commandExecutionObservedMs": {"medianMs": 120.0}
                },
                "instrumentedRuntime": {
                    "available": True,
                    "availableRuns": 2,
                    "exclusiveOwnershipTotalMs": {
                        "modelOnlyMs": 190_000.0,
                        "toolOnlyMs": 4_000.0,
                        "orchestrationMs": 6_000.0,
                    },
                    "exclusiveOwnershipSharePercent": {
                        "modelOnlyPercent": 95.0
                    },
                    "counterTotals": {
                        "logicalGenerationCount": 20,
                        "modelRetryCount": 0,
                        "modelFallbackCount": 0,
                        "attributableRecoveryGenerationCount": 4,
                    },
                },
            },
        }
        upstream = {
            "successfulCompletionTime": {"medianMs": 50_000.0},
            "actualCommandCount": {"median": 4.0},
            "latencyExplanation": {
                "harnessObserved": {
                    "commandExecutionObservedMs": {"medianMs": 100.0}
                },
                "instrumentedRuntime": {"available": False},
            },
        }

        explanation = benchmark.comparison_latency_explanation(
            fork_label="fork",
            fork_summary=fork,
            upstream_label="upstream",
            upstream_summary=upstream,
        )

        joined = " ".join(explanation["findings"])
        self.assertIn("2.00x", joined)
        self.assertIn("command processes themselves do not explain", joined)
        self.assertIn("18 continuation(s)", joined)
        self.assertIn("0 model retries", joined)
        self.assertFalse(explanation["internalOwnershipHeadToHeadComparable"])

    def test_summary_reports_outcome_and_task_contract_separately(self) -> None:
        runs = [
            _run(outcome_correct=True, task_contract_compliant=True),
            _run(outcome_correct=True, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
            _run(outcome_correct=False, task_contract_compliant=False),
        ]

        summary = benchmark.summarize(runs)

        self.assertEqual(summary["successRatePercent"], 40.0)
        self.assertEqual(summary["outcomeCorrectness"]["ratePercent"], 40.0)
        self.assertEqual(summary["taskContractCompliance"]["ratePercent"], 20.0)
        self.assertEqual(summary["successfulCompletionTime"]["count"], 2)
        self.assertEqual(summary["modelWait"]["averageMs"], 75.0)
        self.assertEqual(summary["continuationCount"]["average"], 2.0)
        self.assertEqual(summary["actualCommandCount"]["average"], 4.0)
        self.assertEqual(summary["testsRan"]["runs"], 1)
        self.assertEqual(summary["continuationClassification"]["tracedRuns"], 0)
        self.assertEqual(summary["continuationClassification"]["untracedRuns"], 5)
        self.assertEqual(summary["censoring"]["traceStatusCounts"], {"absent": 5})

    def test_diagnostics_use_structured_categories(self) -> None:
        observed = (
            "argument preflight failed: not valid under any of the given schemas\n"
            "stale_workspace_evidence force_fresh reuse suppression negative cache\n"
            "request_user_input is not supported in exec mode\n"
            "apply_patch verification failed: failed to find expected lines"
        )

        diagnostics = benchmark.classify_diagnostics(
            observed_text=observed,
            timed_out=False,
            exit_code=1,
            terminal_event="turn.failed",
            invalid_json_lines=1,
            verifier_passed=False,
            required_test_passed=False,
            command_execution_failures=1,
        )

        categories = {diagnostic["category"] for diagnostic in diagnostics}
        self.assertEqual(
            categories,
            {
                "schema_rejection",
                "freshness_invalidation",
                "reuse_suppression",
                "unsupported_interactive_tool",
                "patch_mismatch",
                "execution_failure",
                "verifier_failure",
                "task_contract_violation",
                "invalid_event_stream",
            },
        )

    def test_exact_source_state_rejects_dirty_contents(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-source-test-") as temp:
            root = Path(temp)
            revision = benchmark.create_fixture(root)

            state = benchmark.exact_source_state(root, revision, "candidate")

            self.assertEqual(state["commit"], revision)
            self.assertTrue(state["clean"])
            self.assertTrue(state["exactContentsReconstructable"])

            (root / "duration.py").write_text(
                benchmark.CORRECT_IMPLEMENTATION,
                encoding="utf-8",
                newline="\n",
            )
            with self.assertRaisesRegex(RuntimeError, "1 dirty paths"):
                benchmark.exact_source_state(root, revision, "candidate")

    def test_exact_source_state_ignores_ambient_git_repository_pointers(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-git-env-") as temp:
            base = Path(temp)
            requested = base / "requested"
            redirected = base / "redirected"
            requested.mkdir()
            redirected.mkdir()
            requested_revision = benchmark.create_fixture(requested)
            redirected_revision = benchmark.create_fixture(redirected)
            (requested / "duration.py").write_text(
                benchmark.CORRECT_IMPLEMENTATION, encoding="utf-8", newline="\n"
            )
            redirected_git_dir = benchmark.git_output(
                redirected, "rev-parse", "--absolute-git-dir"
            )

            with mock.patch.dict(
                os.environ,
                {
                    "GIT_DIR": redirected_git_dir,
                    "GIT_WORK_TREE": str(redirected),
                    "GIT_NAMESPACE": "ambient-namespace-must-not-apply",
                    "git_replace_ref_base": "refs/ambient-replacements/",
                },
            ):
                self.assertFalse(
                    any(name.upper().startswith("GIT_") for name in benchmark.git_env())
                )
                with self.assertRaisesRegex(RuntimeError, "1 dirty paths"):
                    benchmark.exact_source_state(
                        requested, requested_revision, "candidate"
                    )
                redirected_state = benchmark.exact_source_state(
                    redirected, redirected_revision, "redirected"
                )

            self.assertEqual(redirected_state["topLevel"], str(redirected.resolve()))

    def test_required_test_detection_sees_through_a_shell_wrapper(self) -> None:
        accepted = (
            'bash -lc "python -m unittest -q"',
            "bash -lc 'python -m unittest -q'",
            'powershell.exe -NoProfile -Command "cd repo; python -m unittest -q"',
            'cmd.exe /c "python -m unittest -q"',
        )
        rejected = (
            'bash -lc "python -m unittest -q test_duration"',
            'bash -lc "echo python -m unittest -q"',
            'bash -lc "python -m pytest -q"',
        )

        for command in accepted:
            with self.subTest(command=command):
                self.assertTrue(benchmark.is_required_test_command(command))
        for command in rejected:
            with self.subTest(command=command):
                self.assertFalse(benchmark.is_required_test_command(command))

    def test_masked_exit_codes_do_not_prove_the_required_suite_passed(self) -> None:
        """`python -m unittest -q || true` exits 0 whether or not the suite did."""
        trusted = (
            "python -m unittest -q",
            'bash -lc "python -m unittest -q"',
            # Last position: the payload exits with the suite's own status.
            "cd repo && python -m unittest -q",
            "git status; python -m unittest -q",
            # Explicit propagation of the suite's status.
            "python -m unittest -q; exit $?",
            "python -m unittest -q; exit $LASTEXITCODE",
            'bash -lc "python -m unittest -q; exit $?"',
            "python -m unittest -q; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }",
        )
        masked = (
            "python -m unittest -q || true",
            "python -m unittest -q; echo done",
            "python -m unittest -q; exit 0",
            'bash -lc "python -m unittest -q || true"',
            "python -m unittest -q && echo done",
            "python -m unittest -q 2>&1 | tail -5",
            "false || python -m unittest -q",
            "true || python -m unittest -q",
            "true || cd repo && python -m unittest -q",
        )

        for command in trusted:
            with self.subTest(command=command):
                self.assertTrue(
                    benchmark.required_test_exit_code_reflects_suite(command)
                )
        for command in masked:
            with self.subTest(command=command):
                self.assertTrue(benchmark.is_required_test_command(command))
                self.assertFalse(
                    benchmark.required_test_exit_code_reflects_suite(command)
                )

        records, _ = _record_commands(
            [
                ("item_1", "python -m unittest -q || true", 0),
                ("item_2", "python -m unittest -q", 0),
            ]
        )
        self.assertTrue(records[0]["requiredTest"])
        self.assertFalse(records[0]["exitCodeReflectsSuite"])
        self.assertFalse(records[0]["passed"])
        self.assertTrue(records[1]["exitCodeReflectsSuite"])
        self.assertTrue(records[1]["passed"])

    def test_command_classification_labels_kind_and_mutation(self) -> None:
        cases = {
            "python -m unittest -q": ("required_test", False),
            'bash -lc "python -m unittest -q"': ("required_test", False),
            "python -m pytest -q tests": ("test", False),
            "git status --porcelain": ("inspection", False),
            "cat duration.py": ("inspection", False),
            "git status && cat README.md": ("inspection", False),
            "git checkout -- duration.py": ("mutation", True),
            'bash -lc "echo x > duration.py"': ("mutation", True),
            "sed -i 's/a/b/' duration.py": ("mutation", True),
            'echo ">"': ("inspection", False),
            "some-unknown-tool --run": ("other", False),
        }

        for command, (kind, mutating) in cases.items():
            with self.subTest(command=command):
                self.assertEqual(benchmark.classify_command(command), kind)
                self.assertEqual(benchmark.command_is_mutating(command), mutating)

    def test_trace_links_each_command_to_its_model_generation(self) -> None:
        timing = {
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="validation_interpretation",
                    isContinuation=True,
                    progressKinds=[],
                    unchangedRelevantState=True,
                    nextStructuredActionChanged=False,
                    dispatchMs=5_000,
                    samplingRequestId="req-1",
                ),
            ],
            "toolCalls": [
                _tool_call(),
                _tool_call(
                    callId="call-1",
                    generationIndex=1,
                    acceptedAtMs=5_100,
                    handlerEntryAtMs=5_100,
                    processSpawnedAtMs=5_100,
                    handlerExitAtMs=6_000,
                    outputModelVisibleAtMs=6_000,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("item_1", "python -m unittest -q", 0),
                ("item_3", "python -m unittest -q", 0),
            ]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)

        self.assertEqual(trace["status"], "complete")
        self.assertEqual(
            [
                command["requestLink"]["generationIndex"]
                for command in trace["commands"]
            ],
            [0, 1],
        )
        self.assertEqual(
            trace["linkage"]["commandToToolMethods"],
            {"one_to_one_chronological": 2},
        )
        self.assertEqual(trace["linkage"]["unmatchedCommandItemIds"], [])
        self.assertEqual(trace["modelRequests"][1]["commandItemIds"], ["item_3"])
        # The first command's result is the boundary the next generation waited
        # on, so the tool-to-next-request latency is recorded rather than dropped.
        self.assertEqual(
            trace["commands"][0]["runtimeLatencyToNextAction"][
                "completionToNextRequestDispatchMs"
            ],
            4_000.0,
        )
        self.assertIsNotNone(trace["commands"][0]["nextObservedAction"]["latencyMs"])

    def test_process_spawn_fallback_links_complete_one_to_one_population(self) -> None:
        timing = {
            "classificationComplete": True,
            "startedAtUnixMs": 1_700_000_000_000.0,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    isContinuation=True,
                    dispatchMs=90,
                    samplingRequestId="req-1",
                ),
            ],
            "toolCalls": [
                _tool_call(callId="runtime-a", processSpawnedAtMs=10),
                _tool_call(
                    callId="runtime-b",
                    generationIndex=1,
                    acceptedAtMs=110,
                    handlerEntryAtMs=110,
                    processSpawnedAtMs=110,
                    handlerExitAtMs=200,
                    outputModelVisibleAtMs=200,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("jsonl-a", "git status", 0),
                ("jsonl-b", "python -m unittest -q", 0),
            ]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)

        self.assertEqual(
            [command["toolLink"]["method"] for command in trace["commands"]],
            ["nearest_process_spawn", "nearest_process_spawn"],
        )
        self.assertEqual(
            [command["toolLink"]["timingCallId"] for command in trace["commands"]],
            ["runtime-a", "runtime-b"],
        )

    def test_runtime_tool_call_id_links_nested_command_without_using_outer_call(self) -> None:
        timing = {
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    isContinuation=True,
                    dispatchMs=2_000,
                    samplingRequestId="req-1",
                ),
            ],
            "toolCalls": [
                _tool_call(
                    callId="outer-call",
                    runtimeToolCallId="outer-runtime",
                    toolName="exec",
                ),
                _tool_call(
                    callId="nested-call",
                    runtimeToolCallId="nested-runtime",
                    toolName="exec_command",
                    generationIndex=1,
                    acceptedAtMs=2_100,
                    processSpawnedAtMs=2_100,
                    handlerExitAtMs=2_500,
                    outputModelVisibleAtMs=2_500,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        records: dict[str, dict[str, object]] = {}
        order: list[str] = []
        item_events = []
        for sequence, phase in enumerate(("item.started", "item.completed"), 1):
            completed = phase == "item.completed"
            item = {
                "id": "item_nested",
                "type": "command_execution",
                "command": "git status",
                "runtime_tool_call_id": "nested-runtime",
                "parent_tool_call_id": "outer-call",
                "status": "completed" if completed else "in_progress",
                "exit_code": 0 if completed else None,
            }
            benchmark.record_command_event(
                records=records,
                order=order,
                event_type=phase,
                item=item,
                sequence=sequence,
                observed_ms=100.0 * sequence,
                observed_at_unix_ms=1_700_000_000_000.0 + 100.0 * sequence,
            )
            item_events.append(
                {
                    "sequence": sequence,
                    "eventType": phase,
                    "itemId": "item_nested",
                    "itemType": "command_execution",
                    "observedMs": 100.0 * sequence,
                    "observedAtUnixMs": 1_700_000_000_000.0 + 100.0 * sequence,
                }
            )

        trace = _trace(
            timing=timing,
            commands=[records["item_nested"]],
            item_events=item_events,
        )

        command = trace["commands"][0]
        self.assertEqual(command["toolLink"]["method"], "exact_runtime_tool_call_id")
        self.assertEqual(command["toolLink"]["timingCallId"], "nested-call")
        self.assertEqual(command["requestLink"]["generationIndex"], 1)
        self.assertEqual(trace["toolCalls"][0]["commandItemId"], None)
        self.assertEqual(trace["toolCalls"][1]["commandItemId"], "item_nested")

    def test_failure_evidence_keeps_a_bounded_prefix_and_full_hash(self) -> None:
        output = "failure: " + "x" * (
            benchmark.MAX_MODEL_VISIBLE_EVIDENCE_CHARS + 500
        )
        row = benchmark.failure_evidence_from_event(
            event_type="item.completed",
            event={},
            item={
                "id": "failed-command",
                "type": "command_execution",
                "status": "failed",
                "exit_code": 2,
                "aggregated_output": output,
                "cell_id": "cell-7",
                "runtime_tool_call_id": "runtime-7",
            },
            item_type="command_execution",
            item_id="failed-command",
            sequence=4,
            observed_ms=12.0,
        )

        self.assertIsNotNone(row)
        assert row is not None
        evidence = row["modelVisibleText"]
        self.assertEqual(evidence["textChars"], len(output))
        self.assertTrue(evidence["textTruncated"])
        self.assertEqual(
            len(evidence["textPrefix"]),
            benchmark.MAX_MODEL_VISIBLE_EVIDENCE_CHARS,
        )
        self.assertEqual(evidence["textSha256"], benchmark.text_sha256(output))
        self.assertEqual(row["cellId"], "cell-7")
        self.assertEqual(row["runtimeToolCallId"], "runtime-7")

    def test_trace_retains_bounded_command_evidence_and_absolute_times(self) -> None:
        runtime_start = 1_700_100_000_000
        request_categories = {"logicalTotal": 321, "repeatedUnchangedContext": 12}
        lifecycle_events = [{"boundary": "request_created", "offsetMs": 100}]
        timing = {
            "schemaVersion": 26,
            "profileValid": True,
            "classificationComplete": True,
            "startedAtUnixMs": runtime_start,
            "modelRequests": [
                _model_request(
                    relevantStateFingerprint="redacted-state-fingerprint",
                    requestTokenCategories=request_categories,
                    dispatchMs=25,
                    completedMs=90,
                )
            ],
            "toolCalls": [
                _tool_call(
                    acceptedAtMs=100,
                    lifecycleEvents=lifecycle_events,
                    modelResumedAtMs=1_250,
                )
            ],
            "toolCallTimingOverflow": 0,
            "futureRuntimeField": {"kept": True},
        }
        full_command = "unknown-program --payload " + "x" * 2_000
        commands, item_events = _record_commands([("item_1", full_command, 0)])

        trace = _trace(timing=timing, commands=commands, item_events=item_events)
        request = trace["modelRequests"][0]
        tool_call = trace["toolCalls"][0]
        command = trace["commands"][0]

        # The raw row arrays are not duplicated into `terminalTiming`: they are
        # the retained representation on the trace itself and are the arrays the
        # retention ceilings apply to. Every other terminal timing key, including
        # one this harness does not know about, is kept verbatim.
        self.assertEqual(
            trace["terminalTiming"],
            {
                key: value
                for key, value in timing.items()
                if key not in {"modelRequests", "toolCalls"}
            }
            | {
                "retainedRows": {
                    "modelRequests": "turnTrace.modelRequests",
                    "toolCalls": "turnTrace.toolCalls",
                }
            },
        )
        self.assertEqual(trace["terminalTiming"]["futureRuntimeField"], {"kept": True})
        self.assertEqual(len(trace["modelRequests"]), len(timing["modelRequests"]))
        self.assertEqual(len(trace["toolCalls"]), len(timing["toolCalls"]))
        self.assertEqual(request["requestTokenCategories"], request_categories)
        self.assertEqual(
            request["relevantStateFingerprint"], "redacted-state-fingerprint"
        )
        self.assertEqual(
            request["absoluteTimestamps"]["dispatchAtUnixMs"], runtime_start + 25
        )
        self.assertEqual(tool_call["lifecycleEvents"], lifecycle_events)
        self.assertEqual(
            tool_call["absoluteTimestamps"]["acceptedAtUnixMs"], runtime_start + 100
        )
        # Command text is capped where it is first recorded, so one heredoc
        # carrying a whole file cannot make the report unbounded. The cap is
        # reported rather than applied silently.
        self.assertEqual(
            command["command"], full_command[: benchmark.MAX_COMMAND_TEXT_CHARS]
        )
        self.assertTrue(command["commandTruncated"])
        self.assertEqual(command["commandChars"], len(full_command))
        self.assertEqual(
            command["commandSha256"], benchmark.text_sha256(full_command)
        )
        self.assertEqual(command["itemId"], "item_1")
        self.assertEqual(command["requestLink"]["generationIndex"], 0)
        self.assertEqual(command["completedObservedAtUnixMs"], 1_700_000_000_050.0)
        self.assertEqual(
            trace["causalScope"]["guidanceRuleAttribution"], "not_observed"
        )

    def test_required_test_must_cover_final_workspace_state(self) -> None:
        self.assertTrue(
            benchmark.required_test_covers_final_workspace_state(3, 2)
        )
        self.assertTrue(
            benchmark.required_test_covers_final_workspace_state(3, 3)
        )
        self.assertFalse(
            benchmark.required_test_covers_final_workspace_state(2, 3)
        )
        self.assertFalse(
            benchmark.required_test_covers_final_workspace_state(None, 3)
        )

    def test_rerunning_a_passing_suite_over_unchanged_state_is_redundant(self) -> None:
        timing = {
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="validation_interpretation",
                    isContinuation=True,
                    progressKinds=[],
                    unchangedRelevantState=True,
                    nextStructuredActionChanged=False,
                    dispatchMs=5_000,
                ),
            ],
            "toolCalls": [
                _tool_call(),
                _tool_call(
                    callId="call-1",
                    generationIndex=1,
                    acceptedAtMs=5_100,
                    handlerEntryAtMs=5_100,
                    processSpawnedAtMs=5_100,
                    handlerExitAtMs=6_000,
                    outputModelVisibleAtMs=6_000,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("item_1", "python -m unittest -q", 0),
                ("item_3", "python -m unittest -q", 0),
            ]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)
        continuation = trace["modelRequests"][1]
        classification = continuation["continuationClassification"]

        self.assertEqual(classification["primary"], "verification")
        self.assertIn("post_success_verification", classification["tags"])
        self.assertIn("no_intervening_mutation", classification["tags"])
        self.assertEqual(classification["interpretation"], "redundant_verification")
        self.assertFalse(classification["necessityCausallyEstablished"])
        self.assertFalse(continuation["interveningWorkspaceMutation"])
        self.assertIn("redundant verification", continuation["narrative"])
        self.assertIn("`python -m unittest -q`", continuation["narrative"])

    def test_a_patch_between_test_runs_is_not_a_redundant_verification(self) -> None:
        timing = {
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="validation_interpretation",
                    isContinuation=True,
                    progressKinds=[],
                    unchangedRelevantState=True,
                    nextStructuredActionChanged=False,
                    dispatchMs=5_000,
                ),
            ],
            "toolCalls": [
                _tool_call(),
                # An edit normally arrives as a patch tool call rather than as a
                # shell command, so the mutation check has to see it there.
                _tool_call(
                    callId="patch-0",
                    toolName="apply_patch",
                    acceptedAtMs=2_000,
                    handlerEntryAtMs=2_000,
                    processSpawnedAtMs=None,
                    handlerExitAtMs=2_500,
                    outputModelVisibleAtMs=2_500,
                ),
                _tool_call(
                    callId="call-1",
                    generationIndex=1,
                    acceptedAtMs=5_100,
                    handlerEntryAtMs=5_100,
                    processSpawnedAtMs=5_100,
                    handlerExitAtMs=6_000,
                    outputModelVisibleAtMs=6_000,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("item_1", "python -m unittest -q", 0),
                ("item_3", "python -m unittest -q", 0),
            ]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)
        continuation = trace["modelRequests"][1]

        self.assertTrue(continuation["interveningWorkspaceMutation"])
        self.assertEqual(
            continuation["continuationClassification"]["interpretation"],
            "post_success_verification_after_mutation",
        )

    def test_jsonl_file_change_is_direct_mutation_evidence(self) -> None:
        timing = {
            "startedAtUnixMs": 1_700_000_000_000.0,
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="validation_interpretation",
                    isContinuation=True,
                    progressKinds=[],
                    unchangedRelevantState=True,
                    nextStructuredActionChanged=False,
                    dispatchMs=5_000,
                ),
            ],
            "toolCalls": [
                _tool_call(),
                _tool_call(
                    callId="call-1",
                    generationIndex=1,
                    acceptedAtMs=5_100,
                    handlerEntryAtMs=5_100,
                    processSpawnedAtMs=5_100,
                    handlerExitAtMs=6_000,
                    outputModelVisibleAtMs=6_000,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("item_1", "python -m unittest -q", 0),
                ("item_3", "python -m unittest -q", 0),
            ]
        )
        item_events.append(
            {
                "sequence": 3,
                "eventType": "item.completed",
                "itemId": "edit-1",
                "itemType": "file_change",
                "observedMs": 2_500.0,
                "observedAtUnixMs": 1_700_000_002_500.0,
            }
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)

        self.assertTrue(trace["modelRequests"][1]["interveningWorkspaceMutation"])
        self.assertEqual(
            trace["modelRequests"][1]["continuationClassification"]["interpretation"],
            "post_success_verification_after_mutation",
        )

    def test_linkage_refuses_to_guess_when_populations_disagree(self) -> None:
        timing = {
            "modelRequests": [_model_request()],
            "toolCalls": [_tool_call()],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [
                ("item_1", "python -m unittest -q", 0),
                ("item_3", "git status", 0),
            ]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)

        self.assertEqual(trace["linkage"]["commandToToolMethods"], {"unlinked": 2})
        self.assertEqual(
            trace["linkage"]["unmatchedCommandItemIds"], ["item_1", "item_3"]
        )
        for command in trace["commands"]:
            self.assertEqual(command["requestLink"]["status"], "unlinked")

    def test_linkage_refuses_ordinal_guess_after_runtime_overflow(self) -> None:
        timing = {
            "profileValid": True,
            "modelRequests": [_model_request()],
            "toolCalls": [_tool_call()],
            "toolCallTimingOverflow": 1,
        }
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )

        trace = _trace(timing=timing, commands=commands, item_events=item_events)
        fallback = trace["linkage"]["chronologicalFallback"]

        self.assertFalse(fallback["used"])
        self.assertFalse(fallback["eligible"])
        self.assertEqual(fallback["runtimeToolCallTimingOverflow"], 1)
        self.assertTrue(
            any("omitted 1" in reason for reason in fallback["disabledReasons"])
        )
        self.assertEqual(trace["linkage"]["commandToToolMethods"], {"unlinked": 1})
        self.assertEqual(trace["commands"][0]["requestLink"]["status"], "unlinked")

    def test_timed_out_run_is_right_censored_with_observed_floors(self) -> None:
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )
        trace = _trace(
            timing=None,
            commands=commands,
            item_events=item_events,
            terminal_event=None,
            timed_out=True,
        )
        stream = [
            {
                "sequence": event["sequence"],
                "offsetMs": event["observedMs"],
                "eventType": event["eventType"],
                "itemType": event["itemType"],
                "itemId": event["itemId"],
            }
            for event in item_events
        ]
        benchmark.attach_stream_evidence(
            trace, benchmark.stream_derived_rounds(stream), command_count=1
        )

        self.assertEqual(trace["status"], "right_censored")
        self.assertTrue(trace["censoring"]["rightCensored"])
        self.assertEqual(trace["censoring"]["reason"], "timeout")
        self.assertIn("killed", trace["censoring"]["timingMissingReason"])
        floors = trace["censoring"]["observedFloors"]
        self.assertEqual(floors["commandExecutionsAtLeast"], 1)
        self.assertEqual(floors["modelRoundsAtLeast"], 1)
        # A round count is not a bound in either direction, so it must not be
        # reported as a floor.
        self.assertNotIn("continuationsAtLeast", floors)
        self.assertIn("approximateModelRounds", trace["censoring"]["approximations"])

    def test_turn_failure_is_distinguished_from_an_uninstrumented_build(self) -> None:
        commands, item_events = _record_commands([("item_1", "git status", 0)])

        failed = _trace(
            timing=None,
            commands=commands,
            item_events=item_events,
            terminal_event="turn.failed",
        )
        uninstrumented = _trace(
            timing=None,
            commands=commands,
            item_events=item_events,
            terminal_event="turn.completed",
        )

        self.assertEqual(failed["status"], "terminal_failure_without_timing")
        self.assertIn("turn.failed", failed["censoring"]["timingMissingReason"])
        self.assertEqual(uninstrumented["status"], "timing_unavailable")
        self.assertIn(
            "emits no `timing` block",
            uninstrumented["censoring"]["timingMissingReason"],
        )

    def test_latency_explanation_identifies_the_dominant_owner_and_round_tax(
        self,
    ) -> None:
        timing = {
            "profileValid": True,
            "classificationComplete": True,
            "machineDurationNs": 10_000_000_000,
            "exclusive": {
                "modelOnlyNs": 9_000_000_000,
                "toolOnlyNs": 250_000_000,
                "modelPlusToolNs": 0,
                "orchestrationNs": 750_000_000,
                "finalizationNs": 0,
                "unclassifiedNs": 0,
            },
            "unions": {
                "modelRequestWaitUnionNs": 100_000_000,
                "modelStreamWaitUnionNs": 8_900_000_000,
                "modelStreamProcessingUnionNs": 5_000_000,
            },
            "local": {"planningUnionNs": 50_000_000},
            "counters": {
                "logicalGenerationCount": 2,
                "modelRequestCount": 2,
                "modelRetryCount": 0,
                "toolCallCount": 1,
                "attributableRecoveryGenerationCount": 1,
                "truncationInducedContinuationCount": 1,
                "toolOutputArtifactCreationCount": 1,
                "purposeAggregates": [
                    {
                        "purpose": "implementation",
                        "generations": 2,
                        "modelStreamWaitNs": 8_900_000_000,
                        "decisionLatencyNs": 4_000_000_000,
                    }
                ],
            },
            "modelRequests": [
                _model_request(
                    tokenUsage={
                        "inputTokens": 100,
                        "cachedInputTokens": 0,
                        "visibleOutputTokens": 10,
                        "reasoningTokens": 5,
                        "totalTokens": 115,
                    }
                ),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    isContinuation=True,
                    modelStreamWaitNs=7_900_000_000,
                    decisionLatencyNs=3_000_000_000,
                    tokenUsage={
                        "inputTokens": 250,
                        "cachedInputTokens": 100,
                        "visibleOutputTokens": 20,
                        "reasoningTokens": 10,
                        "totalTokens": 280,
                    },
                ),
            ],
            "toolCalls": [_tool_call()],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )
        trace = _trace(timing=timing, commands=commands, item_events=item_events)

        explanation = benchmark.explain_turn_latency(
            trace, completion_ms=10_100.0, wall_clock_ms=10_200.0, ttfo_ms=500.0
        )

        self.assertEqual(explanation["status"], "instrumented")
        self.assertEqual(
            explanation["observed"]["commandExecutionObservedMs"], 40.0
        )
        runtime = explanation["instrumentedRuntime"]
        self.assertEqual(runtime["dominantOwner"], "modelOnlyMs")
        self.assertEqual(
            runtime["exclusiveOwnershipSharePercent"]["modelOnlyPercent"], 90.0
        )
        self.assertEqual(runtime["counters"]["logicalGenerationCount"], 2)
        self.assertEqual(runtime["providerInputGrowth"]["deltaTokens"], 150)
        self.assertEqual(
            runtime["topSlowModelRounds"][0]["generationIndex"], 1
        )
        self.assertTrue(
            any("tool-output projection recovery" in row for row in explanation["findings"])
        )

        run = {
            "repetition": 1,
            "latencyExplanation": explanation,
        }
        aggregate = benchmark.summarize_latency_explanations([run])
        self.assertEqual(aggregate["instrumentedRuns"], 1)
        self.assertEqual(
            aggregate["instrumentedRuntime"]["counterTotals"][
                "logicalGenerationCount"
            ],
            2,
        )

    def test_latency_explanation_keeps_uninstrumented_ownership_unknown(self) -> None:
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )
        trace = _trace(timing=None, commands=commands, item_events=item_events)

        explanation = benchmark.explain_turn_latency(
            trace, completion_ms=100.0, wall_clock_ms=110.0, ttfo_ms=10.0
        )

        self.assertEqual(explanation["status"], "harness_only")
        self.assertFalse(explanation["instrumentedRuntime"]["available"])
        self.assertIn(
            "model-versus-local ownership inside this build",
            explanation["remainingUnknowns"],
        )

    def test_summary_aggregates_continuation_classes_and_censoring(self) -> None:
        timing = {
            "classificationComplete": True,
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="failure_diagnosis",
                    isContinuation=True,
                    attemptKind="retry",
                    progressKinds=["failure_observation"],
                    dispatchMs=5_000,
                ),
            ],
            "toolCalls": [_tool_call()],
            "toolCallTimingOverflow": 0,
        }
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 1)]
        )
        traced = _trace(timing=timing, commands=commands, item_events=item_events)
        censored = _trace(
            timing=None,
            commands=commands,
            item_events=item_events,
            terminal_event=None,
            timed_out=True,
        )
        runs = [
            {
                **_run(outcome_correct=True, task_contract_compliant=True),
                "turnTrace": traced,
                "turnTraceSummary": benchmark.summarize_turn_trace(traced),
            },
            {
                **_run(outcome_correct=False, task_contract_compliant=False),
                "modelWaitMs": None,
                "continuationCount": None,
                "turnTrace": censored,
                "turnTraceSummary": benchmark.summarize_turn_trace(censored),
            },
        ]

        summary = benchmark.summarize(runs)

        self.assertEqual(
            summary["continuationClassification"]["byPrimaryClass"]["retry"], 1
        )
        self.assertEqual(
            summary["continuationClassification"]["byPurpose"]["failure_diagnosis"],
            1,
        )
        self.assertEqual(
            summary["continuationClassification"]["byReason"]["tool_continuation"],
            1,
        )
        self.assertEqual(
            summary["continuationClassification"]["byDisposition"]["decision_bearing"],
            2,
        )
        self.assertEqual(summary["continuationClassification"]["tracedRuns"], 2)
        self.assertEqual(summary["continuationClassification"]["classifiedRuns"], 1)
        self.assertEqual(summary["commandKindCounts"]["required_test"], 2)
        self.assertEqual(summary["censoring"]["rightCensoredRuns"], 1)
        self.assertEqual(
            summary["censoring"]["traceStatusCounts"],
            {"complete": 1, "right_censored": 1},
        )
        self.assertEqual(
            summary["commandLinkageMethods"]["one_to_one_chronological"], 1
        )

    def test_paired_comparison_excludes_censored_pairs_and_reports_sign_test(
        self,
    ) -> None:
        def pair(
            repetition: int,
            fork_wait: float | None,
            upstream_wait: float,
            censored: bool,
        ) -> dict[str, object]:
            censoring = {"rightCensored": censored}
            return {
                "repetition": repetition,
                "currentFork": {
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "modelWaitMs": fork_wait,
                    "turnTrace": {"censoring": censoring},
                },
                "upstreamC": {
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "modelWaitMs": upstream_wait,
                    "turnTrace": {"censoring": {"rightCensored": False}},
                },
            }

        comparison = benchmark.paired_comparison(
            [
                pair(1, 120.0, 100.0, False),
                pair(2, 140.0, 100.0, False),
                pair(3, None, 100.0, True),
            ],
            fork_label="currentFork",
            upstream_label="upstreamC",
        )
        model_wait = comparison["metrics"]["modelWaitMs"]

        self.assertEqual(model_wait["usablePairs"], 2)
        self.assertEqual(model_wait["excludedPairs"], 1)
        self.assertEqual(model_wait["medianDelta"], 30.0)
        self.assertEqual(model_wait["forkHigherPairs"], 2)
        self.assertEqual(model_wait["signTestTwoSidedP"], 0.5)
        self.assertTrue(model_wait["pairs"][2]["eitherSideRightCensored"])

    def test_regression_gate_passes_each_required_task_shape_independently(
        self,
    ) -> None:
        gate = benchmark.build_regression_gate(
            _gate_pairs(),
            fork_label="currentFork",
            upstream_label="upstreamC",
            experiment_feature="reasoning_governor",
        )

        self.assertTrue(gate["passed"])
        self.assertEqual(
            set(gate["taskGates"]),
            {task.task_id for task in benchmark.BENCHMARK_TASKS},
        )
        self.assertTrue(
            all(task_gate["passed"] for task_gate in gate["taskGates"].values())
        )

    def test_regression_gate_rejects_one_task_p90_even_when_aggregate_passes(
        self,
    ) -> None:
        pairs = _gate_pairs()
        regressed_pair = next(
            pair
            for pair in pairs
            if pair["taskId"] == "slug_diagnostic" and pair["repetition"] == 6
        )
        regressed_pair["currentFork"]["completionMs"] = 106.0

        gate = benchmark.build_regression_gate(
            pairs,
            fork_label="currentFork",
            upstream_label="upstreamC",
            experiment_feature="terminalization",
        )

        completion = gate["taskGates"]["slug_diagnostic"]["metrics"][
            "completionMs"
        ]
        self.assertFalse(gate["passed"])
        self.assertTrue(completion["median"]["passed"])
        self.assertFalse(completion["p90"]["passed"])
        self.assertTrue(gate["aggregateDiagnosticOnly"]["passed"])

    def test_uninstrumented_variant_reports_null_not_zero_continuations(self) -> None:
        """A build that cannot classify rounds must not look like one with none."""
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )
        uninstrumented = _trace(timing=None, commands=commands, item_events=item_events)
        instrumented = _trace(
            timing={
                "classificationComplete": True,
                "modelRequests": [
                    _model_request(),
                    _model_request(
                        generationIndex=1,
                        isContinuation=True,
                        attemptKind="retry",
                        dispatchMs=5_000,
                    ),
                ],
                "toolCalls": [_tool_call()],
                "toolCallTimingOverflow": 0,
            },
            commands=commands,
            item_events=item_events,
        )
        fork_run = {
            "outcomeCorrect": True,
            "taskContractCompliant": True,
            "modelWaitMs": 100.0,
            "actualCommandCount": 1,
            "turnTrace": instrumented,
            "turnTraceSummary": benchmark.summarize_turn_trace(instrumented),
        }
        upstream_run = {
            "outcomeCorrect": True,
            "taskContractCompliant": True,
            "modelWaitMs": None,
            "actualCommandCount": 1,
            "turnTrace": uninstrumented,
            "turnTraceSummary": benchmark.summarize_turn_trace(uninstrumented),
        }

        self.assertEqual(benchmark._run_metric(fork_run, "retryContinuations"), 1)
        self.assertIsNone(benchmark._run_metric(upstream_run, "retryContinuations"))
        self.assertIsNone(benchmark._run_metric(upstream_run, "redundantVerifications"))
        # Harness-measured quantities stay populated for both variants.
        self.assertEqual(benchmark._run_metric(upstream_run, "mutatingCommands"), 0)

        comparison = benchmark.paired_comparison(
            [
                {
                    "repetition": 1,
                    "currentFork": fork_run,
                    "upstreamC": upstream_run,
                }
            ],
            fork_label="currentFork",
            upstream_label="upstreamC",
        )

        self.assertEqual(comparison["metrics"]["retryContinuations"]["usablePairs"], 0)
        self.assertIsNone(comparison["metrics"]["retryContinuations"]["medianDelta"])

    def test_sign_test_matches_the_exact_binomial_tail(self) -> None:
        self.assertIsNone(benchmark.sign_test_p_value(0, 0))
        self.assertEqual(benchmark.sign_test_p_value(1, 0), 1.0)
        self.assertEqual(benchmark.sign_test_p_value(5, 0), 0.0625)
        self.assertEqual(benchmark.sign_test_p_value(4, 1), 0.375)
        self.assertEqual(benchmark.sign_test_p_value(3, 2), 1.0)

    def test_attribution_scope_denies_guidance_rule_causality(self) -> None:
        scope = benchmark.attribution_scope(repetitions=5)

        self.assertEqual(scope["designLimits"]["repetitionsPerVariant"], 5)
        self.assertEqual(
            scope["designLimits"]["smallestAttainableTwoSidedSignTestP"], 0.0625
        )
        self.assertTrue(
            any("guidance rule" in claim for claim in scope["unsupportedClaims"])
        )
        self.assertTrue(scope["toStrengthenAttribution"])

    def test_run_agent_traces_a_stub_agents_stream_end_to_end(self) -> None:
        """Drive the real event loop so the wiring, not just the helpers, is covered."""
        timing = {
            "schemaVersion": 26,
            "profileValid": True,
            "classificationComplete": True,
            "machineDurationNs": 7_000_000_000,
            "exclusive": {
                "modelOnlyNs": 6_000_000_000,
                "toolOnlyNs": 500_000_000,
                "modelPlusToolNs": 0,
                "orchestrationNs": 500_000_000,
                "finalizationNs": 0,
                "unclassifiedNs": 0,
            },
            "unions": {"modelStreamWaitUnionNs": 6_000_000_000},
            "counters": {
                "logicalGenerationCount": 2,
                "modelRequestCount": 2,
                "modelRetryCount": 0,
                "toolCallCount": 2,
            },
            "modelRequests": [
                _model_request(),
                _model_request(
                    generationIndex=1,
                    generationReason="tool_continuation",
                    generationPurpose="validation_interpretation",
                    isContinuation=True,
                    progressKinds=[],
                    unchangedRelevantState=True,
                    nextStructuredActionChanged=False,
                    dispatchMs=5_000,
                    samplingRequestId="req-1",
                ),
            ],
            "toolCalls": [
                _tool_call(),
                _tool_call(
                    callId="call-1",
                    generationIndex=1,
                    acceptedAtMs=5_100,
                    handlerEntryAtMs=5_100,
                    processSpawnedAtMs=5_100,
                    handlerExitAtMs=6_000,
                    outputModelVisibleAtMs=6_000,
                ),
            ],
            "toolCallTimingOverflow": 0,
        }
        events = [
            {"type": "thread.started", "thread_id": "thread_0"},
            {"type": "turn.started"},
            *[
                {
                    "type": phase,
                    "item": {
                        "id": item_id,
                        "type": "command_execution",
                        "command": 'bash -lc "python -m unittest -q"',
                        "aggregated_output": "",
                        "status": "completed"
                        if phase == "item.completed"
                        else "in_progress",
                        "exit_code": 0 if phase == "item.completed" else None,
                    },
                }
                for item_id in ("item_0", "item_1")
                for phase in ("item.started", "item.completed")
            ],
            {
                "type": "item.completed",
                "item": {"id": "item_2", "type": "agent_message", "text": "done"},
            },
            {"type": "turn.completed", "usage": {}, "timing": timing},
        ]

        with tempfile.TemporaryDirectory(prefix="kd4-live-stub-") as temp:
            temp_root = Path(temp)
            stub = temp_root / "stub_agent.py"
            stub.write_text(
                "import json, sys\n"
                f"for event in {events!r}:\n"
                "    print(json.dumps(event), flush=True)\n",
                encoding="utf-8",
                newline="\n",
            )
            auth = temp_root / "auth.json"
            auth.write_text("{}", encoding="utf-8")

            with mock.patch.object(
                benchmark,
                "build_agent_command",
                return_value=[sys.executable, str(stub)],
            ):
                run = benchmark.run_agent(
                    binary=Path(sys.executable),
                    label="stub",
                    repetition=1,
                    model="stub-model",
                    reasoning_effort="high",
                    personality="pragmatic",
                    code_mode="enabled",
                    auth_source=auth,
                    timeout_seconds=60,
                )

        self.assertEqual(run["terminalEvent"], "turn.completed")
        self.assertEqual(run["actualCommandCount"], 2)
        self.assertEqual(run["continuationCount"], 1)
        trace = run["turnTrace"]
        self.assertEqual(trace["status"], "complete")
        self.assertEqual(
            [
                command["requestLink"]["generationIndex"]
                for command in trace["commands"]
            ],
            [0, 1],
        )
        self.assertEqual(
            [command["kind"] for command in trace["commands"]],
            ["required_test", "required_test"],
        )
        self.assertEqual(len(run["continuationNarrative"]), 1)
        self.assertIn("redundant verification", run["continuationNarrative"][0])
        self.assertEqual(
            run["turnTraceSummary"]["byInterpretation"]["redundant_verification"], 1
        )
        # Harness-measured latency exists for every command regardless of build
        # instrumentation, which is what keeps the variants comparable.
        self.assertIsNotNone(trace["commands"][0]["nextObservedAction"]["latencyMs"])
        self.assertEqual(
            trace["censoring"]["observedFloors"]["commandExecutionsAtLeast"], 2
        )
        explanation = run["latencyExplanation"]
        self.assertEqual(explanation["status"], "instrumented")
        self.assertEqual(
            explanation["instrumentedRuntime"]["dominantOwner"], "modelOnlyMs"
        )
        self.assertEqual(
            explanation["instrumentedRuntime"]["counters"][
                "logicalGenerationCount"
            ],
            2,
        )

    def test_run_agent_retains_failed_cell_and_error_evidence_end_to_end(self) -> None:
        command_output = "command failed: " + "z" * (
            benchmark.MAX_MODEL_VISIBLE_EVIDENCE_CHARS + 250
        )
        events = [
            {"type": "thread.started", "thread_id": "thread_0"},
            {"type": "turn.started"},
            {
                "type": "item.started",
                "item": {
                    "id": "failed-item",
                    "type": "command_execution",
                    "command": "python -m unittest -q",
                    "status": "in_progress",
                    "cell_id": "cell-9",
                    "runtime_tool_call_id": "runtime-9",
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "failed-item",
                    "type": "command_execution",
                    "command": "python -m unittest -q",
                    "aggregated_output": command_output,
                    "status": "failed",
                    "exit_code": 7,
                    "cell_id": "cell-9",
                    "runtime_tool_call_id": "runtime-9",
                },
            },
            {"type": "turn.failed", "error": {"message": "model saw failure"}},
        ]

        with tempfile.TemporaryDirectory(prefix="kd4-live-failure-stub-") as temp:
            temp_root = Path(temp)
            stub = temp_root / "stub_agent.py"
            stub.write_text(
                "import json\n"
                f"for event in {events!r}:\n"
                "    print(json.dumps(event), flush=True)\n",
                encoding="utf-8",
                newline="\n",
            )
            auth = temp_root / "auth.json"
            auth.write_text("{}", encoding="utf-8")

            with mock.patch.object(
                benchmark,
                "build_agent_command",
                return_value=[sys.executable, str(stub)],
            ):
                run = benchmark.run_agent(
                    binary=Path(sys.executable),
                    label="failure-stub",
                    repetition=1,
                    model="stub-model",
                    reasoning_effort="high",
                    personality="pragmatic",
                    code_mode="enabled",
                    auth_source=auth,
                    timeout_seconds=60,
                )

        self.assertEqual(run["terminalEvent"], "turn.failed")
        trace = run["turnTrace"]
        command_failure = next(
            row for row in trace["failureEvidence"] if row["itemId"] == "failed-item"
        )
        self.assertEqual(command_failure["cellId"], "cell-9")
        self.assertEqual(command_failure["runtimeToolCallId"], "runtime-9")
        self.assertEqual(command_failure["exitCode"], 7)
        self.assertTrue(command_failure["modelVisibleText"]["textTruncated"])
        self.assertEqual(
            command_failure["modelVisibleText"]["textSha256"],
            benchmark.text_sha256(command_output),
        )
        self.assertTrue(
            any(
                row["eventType"] == "turn.failed"
                and row["modelVisibleText"]["textPrefix"] == "model saw failure"
                for row in trace["failureEvidence"]
            )
        )

    def test_run_agent_rejects_a_passing_suite_followed_by_a_file_change(self) -> None:
        events = [
            {"type": "thread.started", "thread_id": "thread_0"},
            {"type": "turn.started"},
            {
                "type": "item.started",
                "item": {
                    "id": "item_0",
                    "type": "command_execution",
                    "command": 'bash -lc "python -m unittest -q"',
                    "aggregated_output": "",
                    "status": "in_progress",
                    "exit_code": None,
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "item_0",
                    "type": "command_execution",
                    "command": 'bash -lc "python -m unittest -q"',
                    "aggregated_output": "",
                    "status": "completed",
                    "exit_code": 0,
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "item_1",
                    "type": "file_change",
                    "changes": [{"path": "duration.py", "kind": "update"}],
                },
            },
            {"type": "turn.completed", "usage": {}, "timing": {}},
        ]

        with tempfile.TemporaryDirectory(prefix="kd4-live-stub-mutation-") as temp:
            temp_root = Path(temp)
            stub = temp_root / "stub_agent.py"
            stub.write_text(
                "import json\n"
                f"for event in {events!r}:\n"
                "    print(json.dumps(event), flush=True)\n",
                encoding="utf-8",
                newline="\n",
            )
            auth = temp_root / "auth.json"
            auth.write_text("{}", encoding="utf-8")

            with mock.patch.object(
                benchmark,
                "build_agent_command",
                return_value=[sys.executable, str(stub)],
            ):
                run = benchmark.run_agent(
                    binary=Path(sys.executable),
                    label="stub-mutation",
                    repetition=1,
                    model="stub-model",
                    reasoning_effort="high",
                    personality="pragmatic",
                    code_mode="enabled",
                    auth_source=auth,
                    timeout_seconds=60,
                )

        contract = run["taskContract"]
        self.assertEqual(contract["lastSuccessfulTestSequence"], 4)
        self.assertEqual(contract["lastWorkspaceMutationSequence"], 5)
        self.assertFalse(contract["successfulTestObserved"])
        self.assertTrue(
            any(
                "final workspace state" in reason
                for reason in run["complianceFailureReasons"]
            )
        )

    def test_parse_args_rejects_identical_labels_even_for_self_test(self) -> None:
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "kd4_live_agent_benchmark.py",
                    "--self-test",
                    "--fork-label",
                    "same",
                    "--upstream-label",
                    "same",
                ],
            ),
            self.assertRaises(SystemExit),
        ):
            benchmark.parse_args()

    def test_wrapper_choice_never_changes_the_command_kind(self) -> None:
        """A read-only command stays `inspection` when run through a shell.

        `classify_command` decides `inspection` with `all()`, so offering it the
        wrapper text alongside the script counted `bash` itself as an
        unrecognized program and forced every wrapped command to `other`. Since
        `codex exec` surfaces shell commands as `bash -lc <script>`, that
        collapsed the whole category in practice.
        """
        expected = {
            "cat duration.py": "inspection",
            "ls -la": "inspection",
            "git status": "inspection",
            "rm -rf duration.py": "mutation",
            "echo x > duration.py": "mutation",
            "python -m unittest -q": "required_test",
            "python -m pytest": "test",
            "frobnicate --wat": "other",
        }
        wrappers = ("%s", "bash -lc '%s'", "sh -c '%s'", "pwsh -c '%s'")
        for script, kind in expected.items():
            for wrapper in wrappers:
                command = wrapper % script
                with self.subTest(command=command):
                    self.assertEqual(benchmark.classify_command(command), kind)

    def test_descriptor_prefixed_redirection_is_a_workspace_mutation(self) -> None:
        """`1>`/`2>` open a file; only `>&` duplicates a descriptor.

        Missing this fed `interveningWorkspaceMutation`, so a genuine write
        between two runs of the suite left the second one labelled a redundant
        verification.
        """
        for command in ("cat a > b", "cat a 1> b", "cmd 2> err.txt", "cat a >> b"):
            with self.subTest(command=command):
                self.assertTrue(benchmark.command_is_mutating(command))
        for command in ("echo hi 2>&1", "python -m unittest -q 2>&1", "cat a"):
            with self.subTest(command=command):
                self.assertFalse(benchmark.command_is_mutating(command))

    def test_required_test_allows_redirection_but_not_narrowing(self) -> None:
        """Redirecting output does not change which tests run; a selector does."""
        for command in (
            "python -m unittest -q",
            "python -m unittest -q 2>&1",
            "python -m unittest -q > out.txt",
            "python -m unittest -q >> log.txt",
            "python -m unittest -q 2>&1 | tail -5",
            "bash -lc 'python -m unittest -q 2>&1'",
        ):
            with self.subTest(command=command):
                self.assertTrue(benchmark.is_required_test_command(command))
        for command in (
            "python -m unittest -q -k nothing",
            "python -m unittest -q test_duration",
            "python -m unittest -q -p test_none.py",
            "python -m unittest -q -k nothing 2>&1",
            "python -m unittest -qq",
        ):
            with self.subTest(command=command):
                self.assertFalse(benchmark.is_required_test_command(command))

    def test_verifier_timeout_fails_the_run_instead_of_hanging(self) -> None:
        """A non-terminating `parse_duration` must not stall the benchmark.

        Both post-turn checks execute agent-authored code, and they run after
        `--timeout-seconds` has already been spent, so an unbounded wait here is
        unrecoverable.
        """
        with tempfile.TemporaryDirectory(prefix="kd4-live-verifier-hang-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                (root / relative).write_text(content, encoding="utf-8", newline="\n")
            (root / "duration.py").write_text(
                "def parse_duration(text):\n    while True:\n        pass\n",
                encoding="utf-8",
                newline="\n",
            )
            with mock.patch.object(benchmark, "VERIFIER_TIMEOUT_SECONDS", 2):
                passed, failures = benchmark.verify_fixture(root)

        self.assertFalse(passed)
        self.assertTrue(
            any("did not finish within" in failure for failure in failures), failures
        )

    def test_verifier_passes_a_timeout_to_every_check(self) -> None:
        processes = []

        def fake_popen(*_args, **_kwargs):
            process = mock.Mock()
            process.communicate.return_value = ("", "")
            process.returncode = 0
            process.poll.return_value = 0
            processes.append(process)
            return process

        with tempfile.TemporaryDirectory(prefix="kd4-live-verifier-timeout-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                (root / relative).write_text(content, encoding="utf-8", newline="\n")
            with (
                mock.patch.object(benchmark, "spawn_owned_process", side_effect=fake_popen),
                mock.patch.object(benchmark, "terminate_process"),
            ):
                benchmark.verify_fixture(root)

        self.assertEqual(len(processes), 2)
        for process in processes:
            self.assertEqual(
                process.communicate.call_args_list[0].kwargs["timeout"],
                benchmark.VERIFIER_TIMEOUT_SECONDS,
            )

    def test_added_file_fails_exact_fixture_verification(self) -> None:
        """A helper file cannot pass an outcome verifier for an exact fixture."""
        with tempfile.TemporaryDirectory(prefix="kd4-live-added-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                (root / relative).write_text(content, encoding="utf-8", newline="\n")
            (root / "duration.py").write_text(
                benchmark.CORRECT_IMPLEMENTATION, encoding="utf-8", newline="\n"
            )

            self.assertEqual(benchmark.added_workspace_files(root), [])

            # Suite byproducts are not authored files.
            (root / "__pycache__").mkdir()
            (root / "__pycache__" / "duration.cpython-313.pyc").write_bytes(b"")
            self.assertEqual(benchmark.added_workspace_files(root), [])

            (root / "helper.py").write_text("x = 1\n", encoding="utf-8", newline="\n")
            self.assertEqual(benchmark.added_workspace_files(root), ["helper.py"])
            self.assertEqual(benchmark.protected_fixture_failures(root), [])
            passed, failures = benchmark.verify_fixture(root)

            self.assertFalse(passed)
            self.assertIn(
                "helper.py was added; only duration.py may be modified", failures
            )

    def test_gitignore_is_part_of_the_protected_fixture(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-gitignore-") as temp:
            root = Path(temp)
            for relative, content in benchmark.FIXTURE_FILES.items():
                (root / relative).write_text(content, encoding="utf-8", newline="\n")
            (root / ".gitignore").write_text("*\n", encoding="utf-8", newline="\n")

            self.assertIn(
                ".gitignore was modified", benchmark.protected_fixture_failures(root)
            )

    def test_paired_comparison_keeps_censored_pairs_for_harness_metrics(self) -> None:
        """Only a metric that needs the terminal event may drop a censored pair.

        A killed run still has a real wall clock, and censoring correlates with
        being slow, so excluding those pairs everywhere removed exactly the slow
        runs and flattered whichever variant timed out more.
        """

        def pair(repetition: int, fork: float, censored: bool) -> dict[str, object]:
            return {
                "repetition": repetition,
                "currentFork": {
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "wallClockMs": fork,
                    "ttfoMs": fork / 10,
                    "actualCommandCount": fork / 20,
                    "modelWaitMs": None if censored else fork,
                    "turnTrace": {"censoring": {"rightCensored": censored}},
                },
                "upstreamC": {
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "wallClockMs": 100.0,
                    "ttfoMs": 10.0,
                    "actualCommandCount": 5.0,
                    "modelWaitMs": 100.0,
                    "turnTrace": {"censoring": {"rightCensored": False}},
                },
            }

        comparison = benchmark.paired_comparison(
            [pair(1, 120.0, False), pair(2, 140.0, False), pair(3, 600_000.0, True)],
            fork_label="currentFork",
            upstream_label="upstreamC",
        )

        wall_clock = comparison["metrics"]["wallClockMs"]
        self.assertFalse(wall_clock["censoringSensitive"])
        self.assertEqual(wall_clock["usablePairs"], 3)
        self.assertEqual(wall_clock["excludedPairs"], 0)
        self.assertEqual(wall_clock["censoredPairs"], 1)
        # The timed-out repetition carries the slow evidence, so it must count.
        self.assertEqual(wall_clock["medianDelta"], 40.0)
        self.assertEqual(wall_clock["forkHigherPairs"], 3)
        self.assertEqual(comparison["metrics"]["ttfoMs"]["usablePairs"], 3)
        self.assertEqual(
            comparison["metrics"]["actualCommandCount"]["usablePairs"], 3
        )

        model_wait = comparison["metrics"]["modelWaitMs"]
        self.assertTrue(model_wait["censoringSensitive"])
        self.assertEqual(model_wait["usablePairs"], 2)
        self.assertEqual(model_wait["excludedPairs"], 1)

    def test_paired_comparison_excludes_incorrect_or_noncompliant_fast_runs(self) -> None:
        pairs = [
            {
                "repetition": 1,
                "currentFork": {
                    "outcomeCorrect": False,
                    "taskContractCompliant": False,
                    "completionMs": 10.0,
                    "wallClockMs": 10.0,
                    "turnTrace": {"censoring": {"rightCensored": False}},
                },
                "upstreamC": {
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "completionMs": 100.0,
                    "wallClockMs": 100.0,
                    "turnTrace": {"censoring": {"rightCensored": False}},
                },
            }
        ]

        comparison = benchmark.paired_comparison(
            pairs, fork_label="currentFork", upstream_label="upstreamC"
        )

        self.assertEqual(comparison["metrics"]["completionMs"]["usablePairs"], 0)
        self.assertEqual(comparison["metrics"]["wallClockMs"]["usablePairs"], 0)
        self.assertEqual(comparison["jointSuccess"]["eligiblePerformancePairs"], 0)
        self.assertEqual(comparison["jointSuccess"]["bothOutcomeCorrectPairs"], 0)

    def test_trace_retention_caps_rows_and_reports_every_drop(self) -> None:
        """One pathological run must not make the report unbounded."""
        trace = {
            "modelRequests": [{"generationIndex": index} for index in range(600)],
            "toolCalls": [{"callId": str(index)} for index in range(530)],
            "commands": [
                {"itemId": str(index), "command": "x" * 5_000} for index in range(520)
            ],
        }

        overflow = benchmark.truncate_turn_trace(trace)

        self.assertEqual(
            len(trace["modelRequests"]), benchmark.MAX_RETAINED_MODEL_REQUESTS
        )
        self.assertEqual(len(trace["toolCalls"]), benchmark.MAX_RETAINED_TOOL_CALLS)
        self.assertEqual(len(trace["commands"]), benchmark.MAX_RETAINED_COMMANDS)
        self.assertEqual(
            overflow["modelRequests"], 600 - benchmark.MAX_RETAINED_MODEL_REQUESTS
        )
        self.assertEqual(overflow["toolCalls"], 530 - benchmark.MAX_RETAINED_TOOL_CALLS)
        self.assertEqual(overflow["commands"], 520 - benchmark.MAX_RETAINED_COMMANDS)
        self.assertEqual(
            overflow["truncatedCommandTexts"], benchmark.MAX_RETAINED_COMMANDS
        )
        for command in trace["commands"]:
            self.assertEqual(len(command["command"]), benchmark.MAX_COMMAND_TEXT_CHARS)
            self.assertTrue(command["commandTruncated"])

    def test_terminal_timing_does_not_duplicate_uncapped_trace_rows(self) -> None:
        timing = {
            "modelRequests": [_model_request(generationIndex=index) for index in range(2)],
            "toolCalls": [_tool_call(callId=f"call-{index}") for index in range(2)],
            "toolCallTimingOverflow": 0,
        }
        with (
            mock.patch.object(benchmark, "MAX_RETAINED_MODEL_REQUESTS", 1),
            mock.patch.object(benchmark, "MAX_RETAINED_TOOL_CALLS", 1),
        ):
            trace = _trace(timing=timing, commands=[], item_events=[])

        self.assertNotIn("modelRequests", trace["terminalTiming"])
        self.assertNotIn("toolCalls", trace["terminalTiming"])
        self.assertEqual(
            trace["terminalTiming"]["retainedRows"],
            {
                "modelRequests": "turnTrace.modelRequests",
                "toolCalls": "turnTrace.toolCalls",
            },
        )
        self.assertEqual(len(trace["modelRequests"]), 1)
        self.assertEqual(len(trace["toolCalls"]), 1)
        self.assertEqual(trace["retentionOverflow"]["modelRequests"], 1)
        self.assertEqual(trace["retentionOverflow"]["toolCalls"], 1)

    def test_long_command_is_classified_before_its_text_is_truncated(self) -> None:
        full_command = "x" * (benchmark.MAX_COMMAND_TEXT_CHARS + 50) + " > duration.py"
        records: dict[str, dict[str, object]] = {}
        order: list[str] = []
        record = benchmark.record_command_event(
            records=records,
            order=order,
            event_type="item.completed",
            item={
                "id": "long",
                "type": "command_execution",
                "command": full_command,
                "status": "completed",
                "exit_code": 0,
            },
            sequence=1,
            observed_ms=1.0,
            observed_at_unix_ms=1.0,
        )

        self.assertEqual(len(record["command"]), benchmark.MAX_COMMAND_TEXT_CHARS)
        self.assertEqual(record["commandChars"], len(full_command))
        self.assertTrue(record["commandTruncated"])
        self.assertTrue(record["mutating"])

    def test_bounded_text_lines_discards_an_oversized_logical_line(self) -> None:
        lines = list(benchmark.bounded_text_lines(StringIO("x" * 20 + "\nsmall\n"), 5))

        self.assertEqual(lines, [("xxxxx", True), ("small", False)])

    def test_binary_digest_change_aborts_identity_check(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-binary-") as temp:
            binary = Path(temp) / "codex"
            binary.write_bytes(b"first")
            expected = benchmark.sha256(binary)
            benchmark.require_binary_sha256(binary, expected, "candidate")
            binary.write_bytes(b"second")

            with self.assertRaisesRegex(RuntimeError, "changed during the benchmark"):
                benchmark.require_binary_sha256(binary, expected, "candidate")

    def test_terminate_process_kills_the_agents_children(self) -> None:
        """A leaked grandchild outlives the run and holds the inherited pipe."""
        script = (
            "import subprocess, sys, time\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])\n"
            "print(child.pid, flush=True)\n"
            "time.sleep(120)\n"
        )
        parent = subprocess.Popen(
            [sys.executable, "-c", script],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            start_new_session=True,
        )
        try:
            assert parent.stdout
            child_pid = int(parent.stdout.readline().strip())
            benchmark.terminate_process(parent)
            self.assertIsNotNone(parent.poll())

            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and _pid_is_running(child_pid):
                time.sleep(0.2)
            self.assertFalse(
                _pid_is_running(child_pid),
                f"grandchild {child_pid} survived terminate_process",
            )
        finally:
            if parent.poll() is None:
                parent.kill()
            if parent.stdout:
                parent.stdout.close()
            parent.wait(timeout=10)

    def test_native_process_owner_survives_root_exit(self) -> None:
        script = (
            "import subprocess, sys\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])\n"
            "print(child.pid, flush=True)\n"
        )
        parent = benchmark.spawn_owned_process(
            [sys.executable, "-c", script],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        try:
            assert parent.stdout
            child_pid = int(parent.stdout.readline().strip())
            parent.wait(timeout=10)
            self.assertTrue(_pid_is_running(child_pid))

            benchmark.terminate_process(parent)
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and _pid_is_running(child_pid):
                time.sleep(0.2)
            self.assertFalse(
                _pid_is_running(child_pid),
                f"grandchild {child_pid} survived after its root exited",
            )
        finally:
            benchmark.terminate_process(parent)
            if parent.stdout:
                parent.stdout.close()

    def test_run_agent_finally_kills_descendants_after_unexpected_exception(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-finally-") as temp:
            root = Path(temp)
            pid_file = root / "child.pid"
            stub = root / "stub.py"
            stub.write_text(
                "import pathlib, subprocess, sys, time\n"
                f"pid_file = pathlib.Path({str(pid_file)!r})\n"
                "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])\n"
                "pid_file.write_text(str(child.pid))\n"
                "print('{}', flush=True)\n"
                "time.sleep(120)\n",
                encoding="utf-8",
                newline="\n",
            )
            auth = root / "auth.json"
            auth.write_text("{}", encoding="utf-8")

            with (
                mock.patch.object(
                    benchmark,
                    "build_agent_command",
                    return_value=[sys.executable, str(stub)],
                ),
                mock.patch.object(
                    benchmark.json, "loads", side_effect=RuntimeError("decode fault")
                ),
                self.assertRaisesRegex(RuntimeError, "decode fault"),
            ):
                benchmark.run_agent(
                    binary=Path(sys.executable),
                    label="stub-finally",
                    repetition=1,
                    model="stub-model",
                    reasoning_effort="high",
                    personality="pragmatic",
                    code_mode="enabled",
                    auth_source=auth,
                    timeout_seconds=30,
                )

            child_pid = int(pid_file.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline and _pid_is_running(child_pid):
                time.sleep(0.2)
            self.assertFalse(_pid_is_running(child_pid))

    def test_pair_order_balance_reports_the_odd_repetition_residual(self) -> None:
        """Alternation cannot counterbalance an odd repetition count."""
        odd = benchmark.make_pair_order_balance(
            repetitions=5, fork_label="currentFork", upstream_label="upstreamC"
        )
        even = benchmark.make_pair_order_balance(
            repetitions=4, fork_label="currentFork", upstream_label="upstreamC"
        )

        self.assertEqual(odd["upstreamCFirst"], 3)
        self.assertEqual(odd["currentForkFirst"], 2)
        self.assertFalse(odd["balanced"])
        self.assertEqual(even["upstreamCFirst"], 2)
        self.assertEqual(even["currentForkFirst"], 2)
        self.assertTrue(even["balanced"])

    def test_parse_args_defaults_to_ten_and_rejects_odd_repetitions(self) -> None:
        with mock.patch.object(
            sys, "argv", ["kd4_live_agent_benchmark.py", "--self-test"]
        ):
            self.assertEqual(benchmark.parse_args().repetitions, 10)
        with (
            mock.patch.object(
                sys,
                "argv",
                [
                    "kd4_live_agent_benchmark.py",
                    "--self-test",
                    "--repetitions",
                    "5",
                ],
            ),
            self.assertRaises(SystemExit),
        ):
            benchmark.parse_args()

    def test_report_aborts_when_a_binary_changes_mid_benchmark(self) -> None:
        """A rebuild between runs must not be attributed to the measured binary.

        Hashing only after the last run left this undetectable whenever the
        rebuild landed on a clean commit: both source checks would still pass.
        """
        with tempfile.TemporaryDirectory(prefix="kd4-live-binary-") as temp:
            root = Path(temp)
            fork_root = root / "fork"
            upstream_root = root / "upstream"
            fork_root.mkdir()
            upstream_root.mkdir()
            fork_revision = benchmark.create_fixture(fork_root)
            upstream_revision = benchmark.create_fixture(upstream_root)

            fork_binary = root / "fork-codex.exe"
            upstream_binary = root / "upstream-codex.exe"
            fork_binary.write_bytes(b"fork-build-one")
            upstream_binary.write_bytes(b"upstream-build")
            auth_source = root / "auth.json"
            auth_source.write_text("{}", encoding="utf-8")
            output = root / "report.json"

            args = argparse.Namespace(
                fork_binary=fork_binary,
                upstream_binary=upstream_binary,
                fork_root=fork_root,
                fork_revision=fork_revision,
                fork_build_command="cargo build --release",
                fork_label="currentFork",
                upstream_root=upstream_root,
                upstream_revision=upstream_revision,
                upstream_label="upstreamC",
                auth_source=auth_source,
                output=output,
                model="test-model",
                reasoning_effort="high",
                personality="pragmatic",
                code_mode="enabled",
                repetitions=1,
                timeout_seconds=60,
            )

            def stub_run(*, binary: Path, label: str, repetition: int, **_: object):
                # Simulate a rebuild landing while the benchmark is running.
                if binary == fork_binary:
                    fork_binary.write_bytes(b"fork-build-two")
                return {
                    "variant": label,
                    "repetition": repetition,
                    "outcomeCorrect": True,
                    "taskContractCompliant": True,
                    "completionMs": 1.0,
                    "wallClockMs": 2.0,
                    "ttfoMs": 0.5,
                    "modelWaitMs": None,
                    "continuationCount": None,
                    "actualCommandCount": 1,
                    "failureReasons": [],
                    "diagnostics": [],
                }

            with mock.patch.object(benchmark, "run_agent", side_effect=stub_run):
                with self.assertRaisesRegex(RuntimeError, "changed during the benchmark"):
                    benchmark.make_report(args)

    def test_binary_identity_records_the_pre_run_hash(self) -> None:
        with tempfile.TemporaryDirectory(prefix="kd4-live-identity-") as temp:
            root = Path(temp)
            source_root = root / "src"
            source_root.mkdir()
            revision = benchmark.create_fixture(source_root)
            state = benchmark.exact_source_state(source_root, revision, "candidate")
            binary = root / "codex.exe"
            binary.write_bytes(b"build-one")
            before = benchmark.sha256(binary)

            # The binary is replaced after the runs it produced.
            binary.write_bytes(b"build-two")
            identity = benchmark.binary_identity(
                binary,
                source_root,
                state,
                "candidate",
                sha256_before_runs=before,
            )

        self.assertEqual(identity["binary"]["sha256"], before)
        self.assertTrue(identity["binary"]["sha256VerifiedBeforeAndAfterRuns"])

    def test_sign_test_reports_the_half_attainable_at_two_usable_pairs(self) -> None:
        """The docstring's enumeration must cover every attainable value."""
        attainable = {
            benchmark.sign_test_p_value(trials - k, k)
            for trials in range(1, 6)
            for k in range(trials + 1)
        }

        self.assertEqual(attainable, {1.0, 0.625, 0.5, 0.375, 0.25, 0.125, 0.0625})
        self.assertEqual(benchmark.sign_test_p_value(2, 0), 0.5)
        self.assertEqual(benchmark.sign_test_p_value(6, 0), 0.03125)

    def test_null_timing_rows_degrade_to_timing_unavailable(self) -> None:
        """`"modelRequests": null` must yield a degraded trace, not a crash."""
        commands, item_events = _record_commands(
            [("item_1", "python -m unittest -q", 0)]
        )

        trace = _trace(
            timing={"modelRequests": None, "toolCalls": None},
            commands=commands,
            item_events=item_events,
        )

        self.assertEqual(trace["status"], "timing_unavailable")
        self.assertIn("malformed", trace["censoring"]["timingMissingReason"])
        self.assertEqual(trace["modelRequests"], [])
        self.assertEqual(trace["toolCalls"], [])
        self.assertEqual(trace["commands"][0]["requestLink"]["status"], "unlinked")

    def test_diagnostic_text_keeps_the_newest_lines_on_overflow(self) -> None:
        """Failure needles land at the end of a transcript, so the tail survives."""
        with tempfile.TemporaryDirectory(prefix="kd4-live-tail-") as temp:
            temp_root = Path(temp)
            stub = temp_root / "stub_agent.py"
            stub.write_text(
                "import sys\n"
                "for index in range(6):\n"
                "    print(f'noise line {index}', flush=True)\n"
                "print('apply_patch verification failed', flush=True)\n"
                "for index in range(6):\n"
                "    print(f'stderr noise {index}', file=sys.stderr, flush=True)\n"
                "print('final stderr line', file=sys.stderr, flush=True)\n",
                encoding="utf-8",
                newline="\n",
            )
            auth = temp_root / "auth.json"
            auth.write_text("{}", encoding="utf-8")

            with (
                mock.patch.object(benchmark, "MAX_DIAGNOSTIC_TEXT_LINES", 4),
                mock.patch.object(benchmark, "MAX_STDERR_LINES", 4),
                mock.patch.object(
                    benchmark,
                    "build_agent_command",
                    return_value=[sys.executable, str(stub)],
                ),
            ):
                run = benchmark.run_agent(
                    binary=Path(sys.executable),
                    label="stub-tail",
                    repetition=1,
                    model="stub-model",
                    reasoning_effort="high",
                    personality="pragmatic",
                    code_mode="enabled",
                    auth_source=auth,
                    timeout_seconds=60,
                )

        # Seven stdout lines through a cap of four: the three oldest lines were
        # dropped and the needle-bearing tail was kept, so the diagnostic fires.
        self.assertEqual(run["diagnosticTextOverflow"], 3)
        categories = {diagnostic["category"] for diagnostic in run["diagnostics"]}
        self.assertIn("patch_mismatch", categories)
        # Seven stderr lines through a cap of four: the tail, not the head,
        # survives into the serialized stderr excerpt.
        self.assertEqual(run["stderrLineOverflow"], 3)
        self.assertIn("final stderr line", run["stderrTail"])


if __name__ == "__main__":
    unittest.main()
