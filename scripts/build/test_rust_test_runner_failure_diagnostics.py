"""Failure receipts should answer the first diagnostic question in one call."""

import contextlib
import hashlib
import io
import json
import os
import shlex
import subprocess
from unittest import mock

from scripts.build import rust_test_runner as runner_module
from scripts.build.test_rust_test_runner import RunnerTestCase


class FailureDiagnosticTest(RunnerTestCase):
    def test_emitted_failed_only_command_reaches_runner_dispatch(self):
        runner, _ = self.runner()
        runner.base_env["NEXTEST_PROFILE"] = "fast"
        failed = ["suite::first", "suite::second"]
        output = io.StringIO()
        with contextlib.redirect_stderr(output):
            runner._report_failed_tests(
                "core_lib",
                ["suite::passed"],
                failed,
                ["suite::passed", *failed, "suite::not_run"],
            )
        command = (
            output.getvalue()
            .split("Rerun failed only (from repository root):\n", 1)[1]
            .strip()
        )
        # All fixture arguments are single-quoted and contain no apostrophes;
        # shlex decodes this common PowerShell/POSIX subset without execution.
        if os.name == "nt":
            self.assertTrue(command.startswith("& "))
            command = command[2:]
        argv = shlex.split(command)[2:]
        with (
            mock.patch.object(
                runner_module, "load_metadata", return_value=runner.metadata
            ),
            mock.patch.object(runner_module, "RustTestRunner", return_value=runner),
            mock.patch.object(
                runner_module, "_dispatch_with_admission", return_value=0
            ) as dispatch,
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(runner_module.main(argv), 0)
        args, _, _, _, filters, allow_all = dispatch.call_args.args
        self.assertEqual(args.profile, "fast")
        self.assertTrue(args.no_fail_fast)
        self.assertFalse(allow_all)
        self.assertEqual(
            filters,
            [
                "--run-ignored",
                "all",
                "--ignore-default-filter",
                "-E",
                "test(=suite::first) | test(=suite::second)",
            ],
        )

    def test_final_output_replay_is_not_execution_evidence(self):
        execution = (
            "\n".join(
                [
                    "PASS [0.01s] codex-core once",
                    "LEAK [0.01s] codex-core leaked",
                    "FAIL [0.01s] codex-core failed",
                    "PASS [0.01s] codex-core duplicate",
                    "PASS [0.01s] codex-core duplicate",
                ]
            )
            + "\n"
        )
        text = (
            execution + "Summary [0.1s] 5 tests run: 4 passed, 1 failed\n" + execution
        )
        # Machine events preserve actual completions, including duplicates;
        # human LEAK is an `ok` event and replay lives only in diagnostics.
        expected = [
            (status, "codex-core", name)
            for status, name in [
                ("PASS", "once"),
                ("PASS", "leaked"),
                ("FAIL", "failed"),
                ("PASS", "duplicate"),
                ("PASS", "duplicate"),
            ]
        ]
        events = [
            {
                "type": "suite",
                "event": "started",
                "test_count": 5,
                "nextest": {
                    "crate": "codex-core",
                    "test_binary": "codex_core",
                    "kind": "lib",
                },
            }
        ]
        events.extend(
            {
                "type": "test",
                "event": "ok" if status == "PASS" else "failed",
                "name": "codex-core::codex_core$" + name,
            }
            for status, _, name in expected
        )
        result, path = self.retained_result(text)
        result.stdout = "\n".join(json.dumps(event) for event in events)
        before = path.read_bytes()
        self.assertEqual(list(runner_module._nextest_results(result)), expected)
        self.assertEqual(path.read_bytes(), before)
        result.stdout = text  # Human-only output cannot certify any execution.
        self.assertEqual(list(runner_module._nextest_results(result)), [])

        # A completion can put large, nested output before its identity. String
        # projection must preserve the status, not interpret payload delimiters.
        payload = "x" * 2_000_000 + '\\"\r\u2028é' * 20_000
        record = json.dumps(
            {
                "stdout": payload,
                "nested": {"stderr": payload},
                "type": "test",
                "event": "ok",
                "name": "codex-core::codex_core$once",
            },
            ensure_ascii=False,
        )
        suite = json.dumps(events[0]) + "\n"
        followup = json.dumps(
            {"type": "test", "event": "ok", "name": "codex-core::codex_core$after"}
        )
        for label, candidate, first in [
            ("valid", record, [("PASS", "codex-core", "once")]),
            ("invalid-escape", record.replace("x" * 10, "x" * 9 + "\\q", 1), []),
            (
                "invalid-late-escape",
                record.replace("x" * 2_000_000, "x" * 2_000_000 + "\\q", 1),
                [],
            ),
            (
                "invalid-unicode",
                record.replace("x" * 2_000_000, "x" * 2_000_000 + "\\u00xz", 1),
                [],
            ),
            (
                "control",
                record.replace("x" * 2_000_000, "x" * 2_000_000 + "\x01", 1),
                [],
            ),
            ("unclosed", record[:-2], []),
            ("oversized-identity", record.replace("$once", "$" + "y" * 70_000), []),
        ]:
            with self.subTest(record=label):
                result, path = self.retained_result(
                    suite + candidate + "\n" + followup, stream="stdout"
                )
                before = hashlib.sha256(path.read_bytes()).hexdigest()
                self.assertEqual(
                    list(runner_module._nextest_results(result)),
                    first + [("PASS", "codex-core", "after")],
                )
                self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), before)

    def retained_result(self, text, *, stream="stderr"):
        path = self.temp_dir / f"{stream}.log"
        path.write_text(text, encoding="utf-8", newline="")
        result = subprocess.CompletedProcess([], 101, "", "")
        setattr(result, stream, text[-runner_module.MAX_FAILURE_STREAM_CHARS :])
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
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(runner_module.RunnerError) as error,
        ):
            runner._checked(
                ["cargo", "nextest", "run"], env={}, capture=runner_module.CAPTURE_NONE
            )
        diagnostic = str(error.exception)
        for expected in (
            "replay::preserves_identity",
            "core/src/read.rs:42:9",
            "left: 1",
            "right: 2",
            "Summary: 1 failed",
            str(path),
        ):
            self.assertIn(expected, diagnostic)
        self.assertIn("partial", diagnostic)
        self.assertEqual(runner.executor.call_count, 1)  # No retry/recovery process.
        self.assertIs(error.exception.result, result)
        self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), digest)
        self.assertLess(len(diagnostic), 5000)

    def test_linker_arguments_cannot_bury_the_actionable_error(self):
        result, path = self.retained_result(
            "error: linking with `lld-link.exe` failed\n"
            + "  = note: "
            + "library.lib " * 180000
            + "\n"
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

    def test_rustc_async_bound_and_await_spans_survive_failure_projection(self):
        # rustc's rendered non-Send-future diagnostic: the type and causal
        # await/bound spans follow the first four lines of primary context.
        text = (
            "error: future cannot be sent between threads safely\n"
            " --> guard.rs:8:26\n"
            "  |\n"
            "8 | fn main() { require_send(holds_guard(&Mutex::new(0))); }\n"
            "  |                          ^^^^^^^^^^^^^^^^^^^^^^^^^^^ future is not `Send`\n"
            "  |\n"
            "  = help: `Send` is not implemented for `std::sync::MutexGuard<'_, u8>`\n"
            "note: future is not `Send` as this value is used across an await\n"
            " --> guard.rs:5:14\n"
            "  |\n"
            "4 |     let guard = lock.lock().unwrap();\n"
            "  |         ----- has type `std::sync::MutexGuard<'_, u8>`\n"
            "5 |     async {}.await;\n"
            "  |              ^^^^^ await occurs here, with `guard` maybe used later\n"
            "note: required by a bound in `require_send`\n"
            " --> guard.rs:2:20\n"
            "  |\n"
            "2 | fn require_send<T: Send>(_: T) {}\n"
            "  |                    ^^^^ required by this bound\n\n"
            + "warning: unrelated downstream warning\n" * 300
            + "error: could not compile fixture\n"
        )
        result, path = self.retained_result(text)
        before = path.read_bytes()
        runner, _ = self.runner()
        runner.executor = mock.Mock(return_value=result)
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(runner_module.RunnerError) as error,
        ):
            runner._checked(
                ["cargo", "nextest", "list"],
                env={},
                capture=runner_module.CAPTURE_STDOUT,
            )
        for expected in (
            "MutexGuard",
            "guard.rs:5:14",
            "await occurs here",
            "guard.rs:2:20",
            "required by this bound",
            str(path),
        ):
            self.assertIn(expected, str(error.exception))
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(runner.executor.call_count, 1)
        self.assertEqual(list(runner_module._nextest_results(result)), [])
        self.assertLess(len(str(error.exception)), 5000)

    def test_rustc_wrapped_child_messages_keep_trait_and_type_constraints(self):
        # These bodies are emitted by rustc for `let _: String = 42.into()`
        # and `let _: Result<u8, String> = Ok::<bool, i32>(false)` respectively.
        for code, child, expected in (
            (
                "E0277",
                "  = help: `String` implements trait `From<T>`:\n"
                "            From<&String>\n"
                "            From<&mut str>\n"
                "            From<&str>\n"
                "            From<Box<str>>\n"
                "            From<Cow<'_, str>>\n"
                "            From<char>\n"
                "  = note: required for `{integer}` to implement `Into<String>`\n",
                (
                    "From<&String>",
                    "From<char>",
                    "required for `{integer}` to implement `Into<String>`",
                ),
            ),
            (
                "E0308",
                "  = note: expected enum `Result<u8, String>`\n"
                "             found enum `Result<bool, i32>`\n",
                (
                    "expected enum `Result<u8, String>`",
                    "found enum `Result<bool, i32>`",
                ),
            ),
        ):
            with self.subTest(child=child):
                result, path = self.retained_result(
                    f"error[{code}]: compiler constraint\n"
                    " --> constraint.rs:1:27\n  |\n1 | expression\n  | ^^^^ rejected\n  |\n"
                    + child
                    + "\n"
                    + "warning: unrelated downstream warning\n" * 300
                    + "error: terminal compilation failure\n"
                )
                before = path.read_bytes()
                runner, _ = self.runner()
                runner.executor = mock.Mock(return_value=result)
                with (
                    contextlib.redirect_stderr(io.StringIO()),
                    self.assertRaises(runner_module.RunnerError) as error,
                ):
                    runner._checked(
                        ["cargo", "nextest", "list"],
                        env={},
                        capture=runner_module.CAPTURE_STDOUT,
                    )
                for text in (*expected, str(path), "partial"):
                    self.assertIn(text, str(error.exception))
                self.assertEqual(path.read_bytes(), before)
                self.assertEqual(runner.executor.call_count, 1)
                self.assertEqual(list(runner_module._nextest_results(result)), [])
                self.assertLess(len(str(error.exception)), 5000)

    def test_rustc_child_continuation_stops_before_unrelated_output(self):
        for boundary in (
            "warning: unrelated warning\n",
            "   Compiling unrelated v1.0\n",
            "\n",
        ):
            with self.subTest(boundary=boundary):
                result, _ = self.retained_result(
                    "error[E0277]: compiler constraint\n"
                    " --> constraint.rs:1:27\n  |\n1 | expression\n  | ^^^^ rejected\n  |\n"
                    "  = help: permitted implementation:\n"
                    "            From<char>\n"
                    + boundary
                    + "            UNRELATED_INDENTED_DETAIL\n"
                    + "ordinary output\n" * 1000
                    + "terminal status\n"
                )
                excerpt = runner_module._failure_diagnostic_excerpt(
                    result, "stderr", result.stderr
                )
                self.assertIn("From<char>", excerpt)
                self.assertNotIn("UNRELATED_INDENTED_DETAIL", excerpt)
                self.assertLessEqual(
                    len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS
                )

    def test_rustc_wrapped_body_stays_bounded_with_exact_recovery(self):
        result, path = self.retained_result(
            "error[E0277]: compiler constraint\n"
            " --> constraint.rs:1:27\n  |\n1 | expression\n  | ^^^^ rejected\n  |\n"
            "  = help: implementation candidates:\n"
            + ("            " + "λ" * 6000 + "\n") * 80
            + "error: terminal compilation failure\n"
        )
        before = path.read_bytes()
        excerpt = runner_module._failure_diagnostic_excerpt(
            result, "stderr", result.stderr
        )
        self.assertLessEqual(len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS)
        self.assertIn("[line shortened]", excerpt)
        self.assertIn("remaining log omitted", excerpt)
        self.assertIn("terminal compilation failure", excerpt)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(list(runner_module._nextest_results(result)), [])

    def test_warning_notes_are_not_promoted_to_error_constraints(self):
        result, _ = self.retained_result(
            "error[E0308]: mismatched types\n"
            " --> types.rs:1:1\n  |\n1 | value\n  | ^^^^^ expected u8, found bool\n"
            "warning: unrelated warning\n"
            "  = note: unrelated warning constraint\n"
            + "ordinary output\n" * 1000
            + "terminal status\n"
        )
        excerpt = runner_module._failure_diagnostic_excerpt(
            result, "stderr", result.stderr
        )
        self.assertIn("expected u8, found bool", excerpt)
        self.assertNotIn("unrelated warning constraint", excerpt)
        self.assertLessEqual(len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS)

    def test_long_rustc_note_chain_stays_partial_and_retains_exact_evidence(self):
        result, path = self.retained_result(
            "error[E0277]: trait bound not satisfied\n"
            + ("note: required because of bound " + "λ" * 6000 + "\n") * 80
            + "error: terminal compilation failure\n"
        )
        before = path.read_bytes()
        excerpt = runner_module._failure_diagnostic_excerpt(
            result, "stderr", result.stderr
        )
        self.assertLessEqual(len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS)
        self.assertIn("[line shortened]", excerpt)
        self.assertIn("remaining log omitted", excerpt)
        self.assertIn("terminal compilation failure", excerpt)
        self.assertEqual(path.read_bytes(), before)
        self.assertEqual(list(runner_module._nextest_results(result)), [])

    def test_many_long_unicode_assertions_are_explicitly_partial_and_bounded(self):
        result, path = self.retained_result(
            (
                "\x1b[31mthread 'a' panicked at file.rs:1:1:\x1b[0m\n"
                + "left: "
                + "λ" * 6000
                + "\nright: 7\n"
            )
            * 80
            + "terminal status\n"
        )
        excerpt = runner_module._failure_diagnostic_excerpt(
            result, "stderr", result.stderr
        )
        self.assertLessEqual(len(excerpt), runner_module.MAX_FAILURE_STREAM_CHARS)
        self.assertIn("[line shortened]", excerpt)
        self.assertIn("remaining log omitted", excerpt)
        self.assertIn("terminal status", excerpt)
        self.assertNotIn("\x1b", excerpt)
        self.assertIn("λ" * 6000, path.read_text(encoding="utf-8"))

    def test_no_diagnostic_keeps_the_existing_tail(self):
        result, _ = self.retained_result("ordinary log\n" * 1000)
        self.assertEqual(
            runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr),
            result.stderr,
        )

    def test_short_logs_and_missing_artifacts_keep_original_evidence(self):
        result, path = self.retained_result("error: original short failure\n")
        self.assertEqual(
            runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr),
            result.stderr,
        )
        path.unlink()
        self.assertEqual(
            runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr),
            result.stderr,
        )
        del result.stderr_path
        self.assertEqual(
            runner_module._failure_diagnostic_excerpt(result, "stderr", result.stderr),
            result.stderr,
        )

    def test_stdout_selection_obeys_capture_policy_and_does_not_become_test_proof(self):
        result, path = self.retained_result(
            "thread 'fake' panicked at diagnostic only\n" + "noise\n" * 3000,
            stream="stdout",
        )
        runner, _ = self.runner()
        self.assertNotIn(
            "panicked at", runner._failure_detail(result, include_stdout=False)
        )
        self.assertIn(
            "panicked at", runner._failure_detail(result, include_stdout=True)
        )
        self.assertEqual(list(runner_module._nextest_results(result)), [])
        self.assertIn(str(path), runner._failure_detail(result))

    def test_missing_log_during_selection_preserves_tail(self):
        result, _ = self.retained_result("noise\n" * 1000)
        with mock.patch.object(
            runner_module, "_output_lines", side_effect=OSError("unavailable")
        ):
            self.assertEqual(
                runner_module._failure_diagnostic_excerpt(
                    result, "stderr", result.stderr
                ),
                result.stderr,
            )
