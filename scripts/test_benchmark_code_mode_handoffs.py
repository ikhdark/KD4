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
