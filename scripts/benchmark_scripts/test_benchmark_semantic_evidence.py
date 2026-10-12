"""Regression checks for the benchmark's real command-line entrypoint."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class SemanticBenchmarkTest(unittest.TestCase):
    def test_direct_entrypoint_preserves_reports_and_runs_real_worker(self):
        script = Path(__file__).with_name("benchmark_semantic_evidence.py").resolve()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "report.json"
            command = [sys.executable, "-B", str(script), "--iterations", "1", "--output", str(output)]
            result = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            summary = json.loads(result.stdout)
            self.assertEqual(summary["passed_cases"], 12)
            self.assertEqual(summary["total_cases"], 12)
            self.assertEqual(summary["failed_cases"], [])
            before = output.read_bytes()
            report = json.loads(before)
            self.assertEqual(report["model_requests"], 0)
            self.assertEqual(report["worker_calls"], 12)
            # Existing evidence is user work, never a benchmark scratch target.
            refused = subprocess.run(command, cwd=root, capture_output=True, text=True, timeout=5)
            self.assertNotEqual(refused.returncode, 0)
            self.assertEqual(output.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
