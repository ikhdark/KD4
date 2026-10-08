#!/usr/bin/env python3
"""Exercise the no-model benchmark through its real command-line entrypoint."""

import hashlib
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.build_tooling_test_support import REPO_ROOT


@unittest.skipUnless(shutil.which("node"), "Node is required")
class CodeModeHandoffsBenchmarkTest(unittest.TestCase):
    def run_benchmark(self, script, *args):
        return subprocess.run(
            [shutil.which("node"), str(script), *args],
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=15,
            check=False,
        )

    def test_real_fixture_retains_hashes_timings_and_exclusive_output(self):
        script = REPO_ROOT / "scripts" / "benchmark_code_mode_handoffs.mjs"
        runtime = REPO_ROOT / "codex-rs" / "code-mode" / "src" / "runtime"
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "report.json"
            result = self.run_benchmark(script, "--runs", "2", "--output", str(output))
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads(result.stdout)
            self.assertEqual(json.loads(output.read_text(encoding="utf-8")), report)
            self.assertTrue(report["passed"])
            self.assertEqual(report["uncertaintyEvaluation"]["status"], "unmeasured")
            self.assertFalse(report["uncertaintyEvaluation"]["accepted"])
            self.assertEqual(report["runs"], 2)
            self.assertEqual(len(report["wallMs"]), 2)
            self.assertTrue(all(value >= 0 for value in report["wallMs"]))
            self.assertEqual(report["medianMs"], sum(report["wallMs"]) / 2)
            self.assertEqual(
                [source["path"] for source in report["sources"]],
                ["dependency_graph.js", "orchestration.js", "orchestration_tests.js"],
            )
            for source in report["sources"]:
                raw = (runtime / source["path"]).read_bytes()
                self.assertEqual(source["bytes"], len(raw))
                self.assertEqual(source["sha256"], hashlib.sha256(raw).hexdigest())
            saved = output.read_bytes()
            rejected = self.run_benchmark(
                script, "--runs", "1", "--output", str(output)
            )
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("EEXIST", rejected.stderr)
            self.assertEqual(output.read_bytes(), saved)

    def test_critical_path_profile_retains_ten_scoped_measurements(self):
        script = REPO_ROOT / "scripts" / "benchmark_code_mode_handoffs.mjs"
        result = self.run_benchmark(script, "--profile", "critical-path", "--runs", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["profile"], "critical-path")
        self.assertEqual(len(report["scenarios"]), 10)
        self.assertEqual(len({row["name"] for row in report["scenarios"]}), 10)
        self.assertFalse(report["uncertaintyEvaluation"]["accepted"])
        samples = {row["name"]: row["samples"][0] for row in report["scenarios"]}
        self.assertEqual(samples["graph_race_subscriptions"]["subscriptions"], 0)
        self.assertEqual(samples["graph_dependency_rescans"]["dependencyChecks"], 0)
        self.assertEqual(samples["ranked_writer_convoy"]["completionMs"], 115)
        self.assertEqual(samples["full_read_utf8_accounting"]["verifiedBytes"], 8 * 1024 * 1024)
        self.assertEqual(samples["command_deadline_floor"]["overshootMs"], 0)
        # The measured production owner deliberately uses the captured native
        # serializer for escape detection (faster than the regex probe). One
        # body plus its key is two primitive checks, not repeated body encoding.
        self.assertEqual(samples["projection_escape_detection"]["stringSerializations"], 2)
        for row in report["scenarios"]:
            self.assertEqual(len(row["wallMs"]), 1)
            self.assertGreaterEqual(row["medianMs"], 0)
        runtime = REPO_ROOT / "codex-rs" / "code-mode" / "src" / "runtime"
        for source in report["sources"]:
            raw = (runtime / source["path"]).read_bytes()
            self.assertEqual(source["sha256"], hashlib.sha256(raw).hexdigest())

    def test_critical_path_snapshot_identity_failure_does_not_publish_a_report(self):
        script = REPO_ROOT / "scripts" / "benchmark_code_mode_handoffs.mjs"
        runtime = REPO_ROOT / "codex-rs" / "code-mode" / "src" / "runtime"
        sources = []
        for name in ("dependency_graph.js", "orchestration.js", "output_projection.rs"):
            raw = (runtime / name).read_bytes()
            sources.append({"path": name, "bytes": len(raw),
                            "sha256": hashlib.sha256(raw).hexdigest(), "text": raw.decode("utf-8")})
        sources[0]["text"] += "corrupt"
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory) / "snapshot.json"
            output = Path(directory) / "report.json"
            snapshot.write_text(json.dumps({"kind": "retained_direct_file_read", "sources": sources}),
                                encoding="utf-8")
            result = self.run_benchmark(script, "--profile", "critical-path", "--runs", "1",
                                        "--source-snapshot", str(snapshot), "--output", str(output))
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("source snapshot identity mismatch", result.stderr)
            self.assertFalse(output.exists())

    def test_uncertainty_gate_rejects_faster_unsupported_answers_and_missing_cases(self):
        fixture = json.loads((REPO_ROOT / "scripts/fixtures/uncertainty_evaluation.json").read_text())
        trials = []
        for case in fixture["cases"]:
            for mode, variant in ((mode, variant) for mode in case.get("modes", [None])
                                  for variant in ("baseline", "candidate")):
                trials.append({
                    "caseId": case["id"], "pair": 0, "variant": variant,
                    **({"compactionMode": mode} if mode is not None else {}),
                    "model": "test-only", "provider": "scripted-test", "revision": variant,
                    "reviewer": "unit-test-not-live-evaluation", "transcriptSha256": "a" * 64,
                    "inputSha256": "b" * 64, "correct": True, "complete": True,
                    "unsupportedConclusions": 0,
                    "assertions": {key: True for key in case["assertions"]},
                    "wallMs": 100 if variant == "baseline" else 50,
                    "modelRequests": 2, "toolCalls": 3, "validationMs": 10,
                    "recoveries": 1, "retries": 0,
                })
        script = REPO_ROOT / "scripts/benchmark_code_mode_handoffs.mjs"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "trials.json"
            path.write_text(json.dumps(trials))
            passed = self.run_benchmark(script, "--runs", "1", "--model-evaluations", str(path))
            self.assertEqual(passed.returncode, 0, passed.stderr)
            self.assertTrue(json.loads(passed.stdout)["uncertaintyEvaluation"]["accepted"])
            # Equal/faster wall time cannot hide extra inference, validation,
            # recovery, or retry work. Check each dimension independently.
            for metric in ("wallMs", "modelRequests", "toolCalls", "validationMs", "recoveries", "retries"):
                with self.subTest(metric=metric):
                    original = trials[1][metric]
                    trials[1][metric] = trials[0][metric] + 1
                    path.write_text(json.dumps(trials))
                    regressed = self.run_benchmark(script, "--runs", "1", "--model-evaluations", str(path))
                    self.assertEqual(regressed.returncode, 0, regressed.stderr)
                    evaluation = json.loads(regressed.stdout)["uncertaintyEvaluation"]
                    self.assertFalse(evaluation["accepted"])
                    self.assertEqual(evaluation["comparisons"][0]["regressions"], [metric])
                    trials[1][metric] = original
            trials[1]["unsupportedConclusions"] = 1
            path.write_text(json.dumps(trials))
            failed = self.run_benchmark(script, "--runs", "1", "--model-evaluations", str(path))
            self.assertEqual(failed.returncode, 0, failed.stderr)
            self.assertFalse(json.loads(failed.stdout)["uncertaintyEvaluation"]["accepted"])
            path.write_text(json.dumps(trials[:-2]))
            missing = self.run_benchmark(script, "--runs", "1", "--model-evaluations", str(path))
            self.assertNotEqual(missing.returncode, 0)
            self.assertIn("missing required uncertainty case", missing.stderr)

    def test_async_stall_fails_without_publishing_success(self):
        # A copy supplies a stalled fixture without mutating production sources.
        # It would sleep for 60s without an async deadline, despite vm's timeout.
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "scripts" / "benchmark_code_mode_handoffs.mjs"
            script.parent.mkdir()
            shutil.copyfile(
                REPO_ROOT / "scripts" / "benchmark_code_mode_handoffs.mjs", script
            )
            runtime = root / "codex-rs" / "code-mode" / "src" / "runtime"
            runtime.mkdir(parents=True)
            for name in ("dependency_graph.js", "orchestration.js"):
                shutil.copyfile(
                    REPO_ROOT / runtime.relative_to(root) / name, runtime / name
                )
            (runtime / "orchestration_tests.js").write_text(
                "await new Promise(resolve => setTimeout(resolve, 60000));\n"
                "text('orchestration scenarios passed');\n",
                encoding="utf-8",
            )
            output = root / "report.json"
            result = self.run_benchmark(script, "--runs", "2", "--output", str(output))
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("fixture run 1 timed out after 5000 ms", result.stderr)
            self.assertEqual(result.stdout, "")
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
