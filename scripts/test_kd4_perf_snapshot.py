from __future__ import annotations

import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[1]
PERF_SNAPSHOT = REPO_ROOT / "scripts" / "kd4_perf_snapshot.py"


class Kd4PerfSnapshotIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._launcher_temp = tempfile.TemporaryDirectory()
        launcher_root = Path(cls._launcher_temp.name)
        source = launcher_root / "launcher.rs"
        source.write_text(
            textwrap.dedent(
                r"""
                use std::env;
                use std::path::Path;
                use std::process::Command;

                fn main() {
                    let python = env::var_os("KD4_PERF_FAKE_PYTHON")
                        .expect("KD4_PERF_FAKE_PYTHON is required");
                    let script = env::var_os("KD4_PERF_FAKE_SCRIPT")
                        .expect("KD4_PERF_FAKE_SCRIPT is required");
                    let argv: Vec<String> = env::args().collect();
                    let tool = Path::new(&argv[0])
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .expect("launcher must have a file name");
                    let status = Command::new(python)
                        .arg(script)
                        .arg(tool)
                        .args(argv.iter().skip(1))
                        .status()
                        .expect("failed to launch the fake tool");
                    std::process::exit(status.code().unwrap_or(1));
                }
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )
        cls.launcher = launcher_root / (
            "launcher.exe" if os.name == "nt" else "launcher"
        )
        compiled = subprocess.run(
            ["rustc", str(source), "-o", str(cls.launcher)],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        if compiled.returncode != 0:
            raise RuntimeError(
                "could not compile the native fake-tool launcher:\n"
                f"{compiled.stdout}\n{compiled.stderr}"
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._launcher_temp.cleanup()

    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.install_dir = self.root / "install"
        self.install_dir.mkdir()
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.fake_script = self.root / "fake_tool.py"
        self.fake_script.write_text(
            textwrap.dedent(
                r"""
                from __future__ import annotations

                import os
                import sys
                import time
                from pathlib import Path


                def run_codex() -> int:
                    state_path = Path(os.environ["KD4_PERF_FAKE_STATE"])
                    try:
                        invocation = int(state_path.read_text(encoding="utf-8"))
                    except FileNotFoundError:
                        invocation = 0
                    state_path.write_text(str(invocation + 1), encoding="utf-8")
                    delays = [
                        int(value)
                        for value in os.environ.get("KD4_PERF_FAKE_DELAYS_MS", "0").split(",")
                    ]
                    time.sleep(delays[min(invocation, len(delays) - 1)] / 1000)
                    stdout_prefix = os.environ.get("KD4_PERF_FAKE_STDOUT_PREFIX", "codex-test")
                    stderr_prefix = os.environ.get("KD4_PERF_FAKE_STDERR_PREFIX", "")
                    stdout_fill = int(os.environ.get("KD4_PERF_FAKE_STDOUT_FILL", "0"))
                    stderr_fill = int(os.environ.get("KD4_PERF_FAKE_STDERR_FILL", "0"))
                    sys.stdout.buffer.write(stdout_prefix.encode("utf-8") + b"a" * stdout_fill)
                    sys.stderr.buffer.write(stderr_prefix.encode("utf-8") + b"b" * stderr_fill)
                    return int(os.environ.get("KD4_PERF_FAKE_EXIT", "0"))


                def run_git() -> int:
                    args = sys.argv[2:]
                    if args[:2] == ["status", "--porcelain=v2"]:
                        return 129
                    if args[:2] == ["status", "--porcelain=v1"]:
                        print(" M tracked")
                        print("?? new")
                        return 0
                    if args == ["rev-parse", "HEAD"]:
                        print("legacy-head")
                        return 0
                    if args == ["branch", "--show-current"]:
                        print("legacy-main")
                        return 0
                    return 2


                tool = sys.argv[1].lower()
                if tool == "codex":
                    raise SystemExit(run_codex())
                if tool == "git":
                    raise SystemExit(run_git())
                raise SystemExit(f"unsupported fake tool: {tool}")
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )
        self.state_path = self.root / "fake-state.txt"
        self.env = os.environ.copy()
        self.env.update(
            {
                "KD4_PERF_FAKE_PYTHON": sys.executable,
                "KD4_PERF_FAKE_SCRIPT": str(self.fake_script),
                "KD4_PERF_FAKE_STATE": str(self.state_path),
            }
        )

    def tearDown(self) -> None:
        self._temp.cleanup()

    def _copy_launcher(self, target: Path) -> None:
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(self.launcher, target)
        target.chmod(target.stat().st_mode | 0o111)

    def _install_fake_codex(self) -> Path:
        target = self.install_dir / "codex.exe"
        self._copy_launcher(target)
        return target

    def _install_fake_git(self) -> Path:
        target = self.bin_dir / ("git.exe" if os.name == "nt" else "git")
        self._copy_launcher(target)
        return target

    def _run_cli(
        self,
        *args: str | Path,
        expected: int = 0,
        env: dict[str, str] | None = None,
        parse_json: bool = True,
    ) -> tuple[dict[str, Any] | None, subprocess.CompletedProcess[str]]:
        completed = subprocess.run(
            [sys.executable, str(PERF_SNAPSHOT), *(str(arg) for arg in args)],
            cwd=REPO_ROOT,
            env=self.env if env is None else env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        if not parse_json:
            return None, completed
        lines = [line for line in completed.stdout.splitlines() if line.strip()]
        self.assertTrue(lines, msg=f"stderr:\n{completed.stderr}")
        return json.loads(lines[-1]), completed

    def _run_installed(
        self,
        *,
        iterations: int = 1,
        extra: tuple[str | Path, ...] = (),
        expected: int = 0,
        env: dict[str, str] | None = None,
    ) -> tuple[dict[str, Any], subprocess.CompletedProcess[str]]:
        payload, completed = self._run_cli(
            "--repo-root",
            self.repo,
            "--install-dir",
            self.install_dir,
            "--scenario",
            "installed-codex-version",
            "--iterations",
            str(iterations),
            *extra,
            "--json",
            expected=expected,
            env=env,
        )
        assert payload is not None
        return payload, completed

    def _git(self, repo: Path, *args: str) -> str:
        completed = subprocess.run(
            ["git", *args],
            cwd=repo,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            0,
            msg=f"git {' '.join(args)}:\n{completed.stdout}\n{completed.stderr}",
        )
        return completed.stdout.strip()

    def _init_committed_repo(self, repo: Path) -> str:
        self._git(repo, "init")
        self._git(repo, "config", "user.email", "kd4-test@example.invalid")
        self._git(repo, "config", "user.name", "KD4 Test")
        (repo / "tracked.txt").write_text("one\n", encoding="utf-8")
        self._git(repo, "add", "tracked.txt")
        self._git(repo, "commit", "-m", "fixture")
        return self._git(repo, "rev-parse", "HEAD")

    @staticmethod
    def _percentile(values: list[float], quantile: float) -> float:
        ordered = sorted(values)
        position = (len(ordered) - 1) * quantile
        lower = int(position)
        upper = min(lower + 1, len(ordered) - 1)
        fraction = position - lower
        return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction

    @staticmethod
    def _attempt(
        request_id: str, attempt_id: str, wait: int, **overrides: object
    ) -> dict[str, object]:
        record: dict[str, object] = {
            "event.name": "codex.model_attempt",
            "turn_id": "turn-1",
            "generation_index": 2,
            "generation_purpose": "implementation",
            "generation_disposition": "decision_bearing",
            "relevant_state_fingerprint": "state-1",
            "sampling_request_id": request_id,
            "attempt_id": attempt_id,
            "retry_index": 0,
            "outcome": "success",
            "model": "gpt-test",
            "transport": "responses_http",
            "request_kind": "initial",
            "dispatch_ready_us": 10,
            "first_model_output_us": 11,
            "first_actionable_output_us": 10 + wait,
            "completed_us": 10 + wait,
            "input_token_count": 10,
            "cached_input_token_count": 4,
            "uncached_input_token_count": 6,
            "reconciliation_residual_bytes": 2,
            "logical_request_bytes": 102,
            "base_instructions_bytes": 0,
            "tool_schemas_bytes": 100,
            "conversation_history_bytes": 0,
            "current_input_bytes": 0,
            "repository_context_bytes": 0,
            "memory_bytes": 0,
            "skills_bytes": 0,
            "other_injected_context_bytes": 0,
            "envelope_overhead_bytes": 0,
        }
        record.update(overrides)
        return record

    def _write_jsonl(self, name: str, records: list[object]) -> Path:
        path = self.root / name
        path.write_text(
            "".join(
                value + "\n" if isinstance(value, str) else json.dumps(value) + "\n"
                for value in records
            ),
            encoding="utf-8",
        )
        return path

    def _run_analysis(
        self,
        paths: list[Path],
        *,
        report: Path | None = None,
    ) -> dict[str, Any]:
        args: list[str | Path] = ["--allow-incomplete"]
        for path in paths:
            args.extend(("--model-attempt-jsonl", path))
        if report is not None:
            args.extend(("--model-attempt-report", report))
        payload, _ = self._run_installed(extra=tuple(args))
        return payload["modelAttemptAnalysis"]

    def test_cli_help_routes_rollout_analysis_to_turn_latency_audit(self) -> None:
        _, completed = self._run_cli("--help", parse_json=False)

        self.assertNotIn("--rollout-jsonl", completed.stdout)
        self.assertNotIn("--first-useful-action-report", completed.stdout)
        self.assertIn("--model-attempt-jsonl", completed.stdout)

    def test_cli_json_reports_interpolated_percentiles_from_executed_samples(
        self,
    ) -> None:
        self._install_fake_codex()
        env = dict(self.env)
        env["KD4_PERF_FAKE_DELAYS_MS"] = "5,15,25,35"

        payload, _ = self._run_installed(iterations=4, env=env)

        result = payload["results"][0]
        samples = [sample["elapsed_ms"] for sample in result["samples"]]
        self.assertEqual(len(samples), 4)
        self.assertAlmostEqual(
            result["p50_ms"], round(self._percentile(samples, 0.5), 3), places=3
        )
        self.assertAlmostEqual(
            result["p95_ms"], round(self._percentile(samples, 0.95), 3), places=3
        )

    def test_cli_statistics_match_the_same_executed_sample_set(self) -> None:
        self._install_fake_codex()
        env = dict(self.env)
        env["KD4_PERF_FAKE_DELAYS_MS"] = "30,5,20,10"

        payload, _ = self._run_installed(iterations=4, env=env)

        result = payload["results"][0]
        samples = [sample["elapsed_ms"] for sample in result["samples"]]
        self.assertEqual(result["min_ms"], min(samples))
        self.assertEqual(result["max_ms"], max(samples))
        self.assertEqual(result["cold_ms"], samples[0])
        self.assertAlmostEqual(
            result["warm_p50_ms"], round(statistics.median(samples[1:]), 3), places=3
        )

    def test_cli_runs_cold_and_warm_samples_through_installed_binary(self) -> None:
        installed = self._install_fake_codex()

        payload, _ = self._run_installed(iterations=3)

        result = payload["results"][0]
        self.assertEqual(result["status"], "passed")
        self.assertEqual(result["command"], [str(installed), "--version"])
        self.assertEqual(len(result["samples"]), 3)
        self.assertEqual(self.state_path.read_text(encoding="utf-8"), "3")
        self.assertGreater(result["samples"][0]["stdout_bytes"], 0)
        self.assertIsNotNone(result["cold_ms"])
        self.assertIsNotNone(result["warm_p50_ms"])

    def test_cli_bounds_real_child_failure_diagnostics(self) -> None:
        self._install_fake_codex()
        env = dict(self.env)
        env.update(
            {
                "KD4_PERF_FAKE_EXIT": "7",
                "KD4_PERF_FAKE_STDOUT_PREFIX": "discarded-stdout-prefix",
                "KD4_PERF_FAKE_STDERR_PREFIX": "discarded-stderr-prefix",
                "KD4_PERF_FAKE_STDOUT_FILL": "5000",
                "KD4_PERF_FAKE_STDERR_FILL": "5000",
            }
        )

        payload, _ = self._run_installed(extra=("--allow-failures",), env=env)

        result = payload["results"][0]
        sample = result["samples"][0]
        self.assertEqual(result["status"], "failed")
        self.assertGreater(sample["stdout_bytes"], 4096)
        self.assertGreater(sample["stderr_bytes"], 4096)
        self.assertIn("command exited 7", result["reason"])
        self.assertNotIn("discarded-stdout-prefix", result["reason"])
        self.assertNotIn("discarded-stderr-prefix", result["reason"])
        self.assertLess(len(result["reason"].encode("utf-8")), 25_000)

    def test_cli_marks_missing_installed_binary_skipped(self) -> None:
        payload, _ = self._run_installed(expected=1)

        result = payload["results"][0]
        self.assertEqual(result["status"], "skipped")
        self.assertTrue(result["required"])
        self.assertFalse(payload["complete"])
        self.assertEqual(payload["skippedRequiredScenarios"], [result["name"]])

    def test_cli_install_dir_override_is_independent_of_repo_root(self) -> None:
        installed = self._install_fake_codex()
        unrelated = self.root / "unrelated" / "checkout"
        unrelated.mkdir(parents=True)

        payload, _ = self._run_cli(
            "--repo-root",
            unrelated,
            "--install-dir",
            self.install_dir,
            "--scenario",
            "installed-codex-version",
            "--iterations",
            "1",
            "--json",
        )

        assert payload is not None
        result = payload["results"][0]
        self.assertEqual(Path(result["command"][0]), installed)
        self.assertEqual(Path(result["cwd"]), unrelated)

    def _phase0_payload(self) -> dict[str, Any]:
        empty_path = self.root / "empty-path"
        empty_path.mkdir()
        env = dict(self.env)
        env["PATH"] = str(empty_path)
        payload, _ = self._run_cli(
            "--repo-root",
            self.repo,
            "--install-dir",
            self.install_dir,
            "--profile",
            "phase0",
            "--iterations",
            "1",
            "--allow-failures",
            "--allow-incomplete",
            "--json",
            env=env,
        )
        assert payload is not None
        return payload

    def test_cli_phase0_selects_required_baseline_categories(self) -> None:
        payload = self._phase0_payload()

        self.assertEqual(len(payload["results"]), 8)
        categories = {result["category"] for result in payload["results"]}
        self.assertTrue(
            {
                "startup",
                "repository",
                "validation",
                "test",
                "build",
                "app-server",
                "desktop-publish",
            }
            <= categories
        )

    def test_cli_phase0_exposes_the_named_focused_core_target(self) -> None:
        payload = self._phase0_payload()

        focused = next(
            result
            for result in payload["results"]
            if result["name"] == "focused-core-test"
        )
        self.assertEqual(
            focused["command"],
            [
                "just",
                "core-test-fast",
                "core_lib",
                "-E",
                "test(typed_agents_inherit_every_non_root_tool_class)",
            ],
        )

    def test_cli_output_atomically_replaces_previous_snapshot(self) -> None:
        target = self.root / "snapshot.json"
        first, _ = self._run_installed(extra=("--allow-incomplete", "--output", target))
        self.assertEqual(json.loads(target.read_text(encoding="utf-8")), first)
        self._install_fake_codex()

        second, _ = self._run_installed(extra=("--output", target))

        self.assertNotEqual(first["complete"], second["complete"])
        self.assertEqual(json.loads(target.read_text(encoding="utf-8")), second)
        self.assertEqual(list(target.parent.glob(f".{target.name}.*.tmp")), [])

    def test_cli_output_failure_leaves_no_temporary_snapshot(self) -> None:
        target = self.root / "snapshot.json"
        target.mkdir()

        _, completed = self._run_cli(
            "--repo-root",
            self.repo,
            "--install-dir",
            self.install_dir,
            "--scenario",
            "installed-codex-version",
            "--iterations",
            "1",
            "--allow-incomplete",
            "--output",
            target,
            "--json",
            expected=1,
            parse_json=False,
        )

        self.assertTrue(target.is_dir())
        self.assertEqual(list(target.parent.glob(f".{target.name}.*.tmp")), [])
        self.assertIn("Error", completed.stderr)

    def test_cli_non_git_repo_distinguishes_metadata_failure(self) -> None:
        payload, _ = self._run_installed(extra=("--allow-incomplete",))

        environment = payload["environment"]
        self.assertIsNone(environment["head"])
        self.assertIsNone(environment["branch"])
        self.assertIsNone(environment["dirtyPaths"])

    def test_cli_clean_git_repo_reports_zero_dirty_paths(self) -> None:
        head = self._init_committed_repo(self.repo)

        payload, _ = self._run_installed(extra=("--allow-incomplete",))

        environment = payload["environment"]
        self.assertEqual(environment["head"], head)
        self.assertEqual(environment["dirtyPaths"], 0)
        self.assertIsNotNone(environment["branch"])

    def test_cli_real_git_reports_dirty_detached_and_unborn_states(self) -> None:
        head = self._init_committed_repo(self.repo)
        self._git(self.repo, "checkout", "--detach")
        (self.repo / "untracked file.txt").write_text("dirty\n", encoding="utf-8")

        detached, _ = self._run_installed(extra=("--allow-incomplete",))

        self.assertEqual(detached["environment"]["head"], head)
        self.assertIsNone(detached["environment"]["branch"])
        self.assertEqual(detached["environment"]["dirtyPaths"], 1)

        unborn = self.root / "unborn"
        unborn.mkdir()
        self._git(unborn, "init")
        self._git(unborn, "symbolic-ref", "HEAD", "refs/heads/feature/unborn")
        payload, _ = self._run_cli(
            "--repo-root",
            unborn,
            "--install-dir",
            self.install_dir,
            "--scenario",
            "installed-codex-version",
            "--iterations",
            "1",
            "--allow-incomplete",
            "--json",
        )
        assert payload is not None
        self.assertIsNone(payload["environment"]["head"])
        self.assertEqual(payload["environment"]["branch"], "feature/unborn")
        self.assertEqual(payload["environment"]["dirtyPaths"], 0)

    def test_cli_legacy_git_fallback_reports_repository_metadata(self) -> None:
        self._install_fake_git()
        env = dict(self.env)
        env["PATH"] = str(self.bin_dir)

        payload, _ = self._run_installed(extra=("--allow-incomplete",), env=env)

        self.assertEqual(payload["environment"]["head"], "legacy-head")
        self.assertEqual(payload["environment"]["branch"], "legacy-main")
        self.assertEqual(payload["environment"]["dirtyPaths"], 2)

    def test_cli_model_attempt_analysis_filters_groups_and_reconciles(self) -> None:
        records = [
            "not-json",
            self._attempt("clean-1", "a", 100),
            self._attempt(
                "clean-2",
                "b",
                200,
                cached_input_token_count=5,
                uncached_input_token_count=5,
            ),
            self._attempt("retry", "c", 300, outcome="failed"),
            self._attempt("retry", "d", 400, retry_index=1),
            self._attempt("failed", "e", 500, outcome="failed"),
            self._attempt("cancelled", "f", 600, outcome="cancelled"),
            self._attempt("missing", "g", 700, first_actionable_output_us=None),
        ]
        path = self._write_jsonl("attempts.jsonl", records)
        report = self.root / "reports" / "model-attempts.txt"

        analysis = self._run_analysis([path], report=report)

        self.assertEqual(analysis["totalPhysicalAttempts"], 7)
        self.assertEqual(analysis["includedPhysicalAttempts"], 4)
        self.assertEqual(analysis["includedLogicalRequests"], 3)
        self.assertEqual(
            analysis["outcomeCounts"], {"success": 4, "failed": 2, "cancelled": 1}
        )
        self.assertEqual(analysis["exclusionCounts"]["malformed_json"], 1)
        self.assertEqual(analysis["exclusionCounts"]["no_terminal_success"], 2)
        self.assertEqual(
            analysis["exclusionCounts"]["missing_first_actionable_output_us"], 1
        )
        group = analysis["groups"][0]
        self.assertEqual(group["sampleCount"], 3)
        self.assertEqual(group["generationPurpose"], "implementation")
        self.assertEqual(group["generationDisposition"], "decision_bearing")
        self.assertEqual(group["decisionLatencyUs"]["p50"], 200.0)
        bins = group["predictors"]["cached_input_token_count"]["quantileBins"]
        self.assertEqual(sum(item["count"] for item in bins), 3)
        self.assertEqual(analysis["componentReconciliation"]["coveredCount"], 3)
        self.assertEqual(analysis["componentReconciliation"]["withinToleranceCount"], 3)
        retry_row = next(
            row for row in analysis["rows"] if row["sampling_request_id"] == "retry"
        )
        self.assertEqual(retry_row["retry_count"], 1)
        self.assertEqual(retry_row["retry_overhead_us"], 300.0)
        self.assertEqual(retry_row["decision_latency_us"], 700.0)
        human = report.read_text(encoding="utf-8")
        self.assertIn("tokens p50/p95", human)
        self.assertIn("spearman=", human)
        self.assertIn("reconciliation:", human)

    def test_cli_model_attempt_report_exposes_spearman_tie_result(self) -> None:
        path = self._write_jsonl(
            "ties.jsonl",
            [
                self._attempt(
                    "one",
                    "a",
                    100,
                    cached_input_token_count=1,
                    uncached_input_token_count=9,
                ),
                self._attempt(
                    "two",
                    "b",
                    100,
                    cached_input_token_count=1,
                    uncached_input_token_count=9,
                ),
                self._attempt(
                    "three",
                    "c",
                    300,
                    cached_input_token_count=2,
                    uncached_input_token_count=8,
                ),
            ],
        )
        report = self.root / "ties.txt"

        analysis = self._run_analysis([path], report=report)

        predictors = analysis["groups"][0]["predictors"]
        self.assertEqual(predictors["cached_input_token_count"]["spearmanRho"], 1.0)
        self.assertIsNone(predictors["tool_schemas_bytes"]["spearmanRho"])
        self.assertIn("spearman=1.0", report.read_text(encoding="utf-8"))

    def test_cli_model_attempt_jsonl_flags_load_nested_records(self) -> None:
        path = self._write_jsonl(
            "nested.jsonl",
            [
                "not-json",
                {"event.name": "something.else"},
                {"fields": self._attempt("nested", "a", 100)},
            ],
        )
        report = self.root / "nested-report.txt"

        analysis = self._run_analysis([path], report=report)

        self.assertEqual(analysis["totalPhysicalAttempts"], 1)
        self.assertEqual(analysis["includedPhysicalAttempts"], 1)
        self.assertEqual(analysis["exclusionCounts"]["malformed_json"], 1)
        self.assertEqual(analysis["exclusionCounts"]["not_model_attempt"], 1)
        self.assertTrue(report.is_file())

    def test_cli_model_attempt_jsonl_deduplicates_overlapping_files(self) -> None:
        attempt = self._attempt("request", "attempt", 100)
        conflicting = {**attempt, "outcome": "failed"}
        first = self._write_jsonl("first.jsonl", [attempt])
        second = self._write_jsonl("second.jsonl", [attempt, conflicting])

        analysis = self._run_analysis([first, second])

        self.assertEqual(analysis["totalPhysicalAttempts"], 2)
        self.assertEqual(
            analysis["exclusionCounts"]["duplicate_physical_attempt_collapsed"], 1
        )
        self.assertEqual(
            analysis["exclusionCounts"]["conflicting_physical_attempt_duplicate"],
            1,
        )

    def test_cli_joins_stable_context_components_to_physical_attempts(self) -> None:
        attempt = {
            "event.name": "codex.model_attempt",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "outcome": "success",
            "provider_baseline": "fresh_full_replay",
            "fresh_response_id_established": True,
            "wire_request_bytes": 1200,
            "input_token_count": 1000,
            "cached_input_token_count": 750,
        }
        component = {
            "event.name": "codex.model_context_component",
            "sampling_request_id": "request",
            "attempt_id": "attempt",
            "retry_index": 0,
            "component_kind": "repository",
            "contract_version": 1,
            "semantic_id": "repository:v1:opaque",
            "content_hash": "abcdef012345",
            "serialized_bytes": 4000,
            "approx_tokens": 1000,
            "active": True,
            "local_reused": True,
        }
        path = self._write_jsonl("context.jsonl", [attempt, component])

        stable = self._run_analysis([path])["stableContext"]

        self.assertEqual(stable["averageActiveContextTokens"], 1000.0)
        self.assertEqual(stable["peakActiveContextTokens"], 1000.0)
        self.assertEqual(stable["localReusedBytes"], 4000.0)
        self.assertEqual(stable["providerCachedShare"], 0.75)
        self.assertEqual(stable["successfulRebases"], 1)
        self.assertEqual(stable["componentVersions"][0]["requestAppearances"], 1)
        self.assertEqual(
            stable["componentVersions"][0]["cumulativeLogicalExposureTokens"],
            1000.0,
        )

    def test_cli_counts_retry_context_independent_of_provider_cache(self) -> None:
        records: list[dict[str, object]] = []
        for index in range(10):
            request_id = "request" if index < 2 else f"request-{index}"
            attempt_id = f"attempt-{index}"
            retry_index = index if index < 2 else 0
            records.append(
                {
                    "event.name": "codex.model_attempt",
                    "sampling_request_id": request_id,
                    "attempt_id": attempt_id,
                    "retry_index": retry_index,
                    "outcome": "success",
                    "input_token_count": 1000,
                    "cached_input_token_count": 900,
                }
            )
            records.append(
                {
                    "event.name": "codex.model_context_component",
                    "sampling_request_id": request_id,
                    "attempt_id": attempt_id,
                    "retry_index": retry_index,
                    "component_kind": "repository",
                    "contract_version": 1,
                    "semantic_id": "repository:v1:opaque",
                    "content_hash": "abcdef012345",
                    "serialized_bytes": 4000,
                    "approx_tokens": 1000,
                    "active": True,
                    "local_reused": index > 0,
                }
            )
        path = self._write_jsonl("retry-context.jsonl", records)

        analysis = self._run_analysis([path])
        stable = analysis["stableContext"]

        self.assertEqual(analysis["totalPhysicalAttempts"], 10)
        self.assertEqual(analysis["totalLogicalRequests"], 9)
        self.assertEqual(stable["cumulativeLogicalContextTokens"], 10_000.0)
        self.assertEqual(
            stable["componentVersions"][0]["cumulativeLogicalExposureTokens"],
            10_000.0,
        )
        self.assertEqual(stable["providerCachedShare"], 0.9)
        self.assertEqual(stable["localConstructedBytes"], 4000.0)
        self.assertEqual(stable["localReusedBytes"], 36_000.0)
        self.assertEqual(stable["componentCacheHits"], 9.0)

    def test_cli_stable_context_tolerates_missing_provider_cache_fields(self) -> None:
        path = self._write_jsonl(
            "missing-cache.jsonl",
            [
                {
                    "event.name": "codex.model_attempt",
                    "sampling_request_id": "request",
                    "attempt_id": "attempt",
                    "retry_index": 0,
                    "outcome": "failed",
                    "input_token_count": None,
                    "cached_input_token_count": None,
                }
            ],
        )

        stable = self._run_analysis([path])["stableContext"]

        self.assertIsNone(stable["providerCachedShare"])
        self.assertEqual(stable["wireRequestBytes"], 0.0)


if __name__ == "__main__":
    unittest.main()
