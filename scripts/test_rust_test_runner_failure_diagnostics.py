"""Failure receipts should answer the first diagnostic question in one call."""

import contextlib
import hashlib
import io
import subprocess
from unittest import mock

from scripts import rust_test_runner as runner_module
from scripts.test_rust_test_runner import RunnerTestCase


class FailureDiagnosticTest(RunnerTestCase):
    def retained_result(self, text, *, stream="stderr"):
        path = self.temp_dir / f"{stream}.log"
        path.write_text(text, encoding="utf-8", newline="")
        result = subprocess.CompletedProcess([], 101, "", "")
        setattr(result, stream, text[-runner_module.MAX_FAILURE_STREAM_CHARS:])
        setattr(result, f"{stream}_path", path)
        return result, path

    def test_failed_test_and_assertion_arrive_with_first_failure_receipt(self):
        text = (
            "        FAIL [ 0.05s] codex-core replay::preserves_identity\n"
            "thread 'replay' panicked at core/src/read.rs:42:9:\n"
            "assertion `left == right` failed\n  left: 1\n right: 2\n"
            + "        PASS [ 0.01s] codex-core independent::test\n" * 400
            + "Summary: 1 failed\n"
        )
        result, path = self.retained_result(text)
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        self.assertNotIn("left: 1", result.stderr)  # Tail-only baseline needs recovery.
        runner, _ = self.runner()
        runner.executor = mock.Mock(return_value=result)
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(runner_module.RunnerError) as error:
            runner._checked(["cargo", "nextest", "run"], env={}, capture=runner_module.CAPTURE_NONE)
        diagnostic = str(error.exception)
        for expected in ("replay::preserves_identity", "core/src/read.rs:42:9", "left: 1", "right: 2", "Summary: 1 failed", str(path)):
            self.assertIn(expected, diagnostic)
        self.assertIn("partial", diagnostic)
        self.assertEqual(runner.executor.call_count, 1)  # No retry/recovery process.
        self.assertIs(error.exception.result, result)
        self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)
        self.assertLess(len(diagnostic), 5000)

    def test_linker_arguments_cannot_bury_the_actionable_error(self):
        result, path = self.retained_result(
            "error: linking with `lld-link.exe` failed\n"
            + "  = note: " + "library.lib " * 180000 + "\n"
            + "  = note: lld-link: error: failed to write executable: Permission denied\n"
            + "warning: unrelated detail\n" * 800
            + "error: could not compile `codex-core`\n"
        )
        raw = path.read_bytes()
        runner, _ = self.runner()
        detail = runner._failure_detail(result)
        self.assertIn("Permission denied", detail)
        self.assertIn("could not compile", detail)
        self.assertIn(str(path), detail)
        self.assertLess(len(detail), 4500)
        self.assertEqual(path.read_bytes(), raw)

    def test_many_long_unicode_assertions_are_explicitly_partial_and_bounded(self):
        result, path = self.retained_result((
            "\x1b[31mthread 'a' panicked at file.rs:1:1:\x1b[0m\n"
            + "left: " + "λ" * 6000 + "\nright: 7\n"
        ) * 80 + "terminal status\n")
        excerpt = runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr)
        self.assertLessEqual(len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS)
        self.assertIn("[line shortened]", excerpt)
        self.assertIn("remaining log omitted", excerpt)
        self.assertIn("terminal status", excerpt)
        self.assertIn("λ" * 6000, path.read_text(encoding="utf-8"))

    def test_no_diagnostic_keeps_the_existing_tail(self):
        result, _ = self.retained_result("ordinary log\n" * 1000)
        self.assertEqual(runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr), result.stderr)

    def test_short_logs_and_missing_artifacts_keep_original_evidence(self):
        result, path = self.retained_result("error: original short failure\n")
        self.assertEqual(runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr), result.stderr)
        path.unlink()
        self.assertEqual(runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr), result.stderr)
        del result.stderr_path
        self.assertEqual(runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr), result.stderr)

    def test_stdout_selection_obeys_capture_policy_and_does_not_become_test_proof(self):
        result, path = self.retained_result("thread 'fake' panicked at diagnostic only\n" + "noise\n" * 3000, stream="stdout")
        runner, _ = self.runner()
        self.assertNotIn("panicked at", runner._failure_detail(result, include_stdout=False))
        self.assertIn("panicked at", runner._failure_detail(result, include_stdout=True))
        self.assertEqual(list(runner_module._nextest_results(result)), [])
        self.assertIn(str(path), runner._failure_detail(result))

    def test_missing_log_during_selection_preserves_tail(self):
        result, _ = self.retained_result("noise\n" * 1000)
        with mock.patch.object(runner_module, "_output_lines", side_effect=OSError("unavailable")):
            self.assertEqual(runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr), result.stderr)
