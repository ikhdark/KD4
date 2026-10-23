"""Audit 140: bounded interpretation, optional telemetry, and phase accounting."""

import contextlib
import io
import json
import subprocess
from unittest import mock

from scripts.build import rust_test_runner as module
from scripts.build.test_rust_test_runner import FakeExecutor, RunnerTestCase
from scripts.build.validation_metrics import ValidationMetrics


class Audit140RegressionTest(RunnerTestCase):
    def helper_free_runner(self, executor=None):
        manifest = self.manifest(
            targets={"core_lib": {"package": "codex-core", "lib": True, "helpers": []}},
            gates={
                "audit": {
                    "steps": [{"target": "core_lib", "tests": ["mod::tests::alpha"]}]
                }
            },
        )
        return self.runner(manifest=manifest, executor=executor)[0]

    def test_projected_string_skips_character_iteration_without_skipping_syntax(self):
        class SyntaxFreeChunk(str):
            def __iter__(self):
                raise AssertionError("projected payload must not be iterated in Python")

        chunks = ['{"stdout":"', SyntaxFreeChunk("x" * 65_536)]
        chunks.extend(SyntaxFreeChunk("x" * 65_536) for _ in range(3))
        chunks.append('","type":"test","event":"ok","name":"owner$test"}\n')
        event = json.loads(next(iter(module._nextest_json_lines(chunks))))
        self.assertEqual(
            event, {"stdout": None, "type": "test", "event": "ok", "name": "owner$test"}
        )

    def test_projection_preserves_split_escape_validation_and_record_recovery(self):
        following = '{"type":"test","event":"ok","name":"owner$after"}'
        for suffix, valid in [
            ("\\u0041", True),
            ("\\q", False),
            ("\\u00xz", False),
            ("\x01", False),
        ]:
            candidate = '{"stdout":"' + "x" * 70_000 + suffix + '","event":"ok"}'
            for split in [1, 65_536, len(candidate) - 4]:
                with self.subTest(suffix=suffix, split=split):
                    chunks = [candidate[:split], candidate[split:], "\n", following]
                    actual = [
                        json.loads(line) for line in module._nextest_json_lines(chunks)
                    ]
                    expected = [{"stdout": None, "event": "ok"}] if valid else []
                    self.assertEqual(actual, expected + [json.loads(following)])

    def test_duration_search_tries_only_the_first_finished_prefix(self):
        line = "Finished diagnostic " * 12_000
        pattern = module.re.compile(r"Finished .* in ((?:[\d.]+(?:ms|[hms])\s*)+)")
        matcher = mock.Mock(wraps=pattern)
        result = subprocess.CompletedProcess([], 0, "", line)
        with mock.patch.object(module.re, "compile", return_value=matcher):
            self.assertEqual(module.RustTestRunner._reported_durations(result), {})
        self.assertEqual(matcher.match.call_count, 1)
        self.assertEqual(matcher.match.call_args.args[0], line)
        if len(matcher.match.call_args.args) > 1:
            self.assertEqual(matcher.match.call_args.args[1], 0)

    def test_duration_values_keep_units_and_last_matching_summary(self):
        result = subprocess.CompletedProcess(
            [], 0, "", "Finished build in 3m 37s\nSummary [1.527s]\n"
        )
        self.assertEqual(
            module.RustTestRunner._reported_durations(result),
            {"cargo-reported": 217.0, "tests-reported": 1.527},
        )

    def test_malformed_timing_cannot_erase_or_create_native_receipts(self):
        for timing in ["Finished build in 1..2s", "Summary [1..2s]"]:
            for captured in [False, True]:
                for native in [False, True]:
                    with self.subTest(timing=timing, captured=captured, native=native):

                        class TimingExecutor(FakeExecutor):
                            def __call__(inner, args, **kwargs):
                                result = super().__call__(args, **kwargs)
                                if args[:3] == ["cargo", "nextest", "run"]:
                                    events = [
                                        json.loads(line)
                                        for line in result.stdout.splitlines()
                                    ]
                                    if captured:
                                        events[2]["stdout"] = timing
                                    result.stdout = (
                                        "".join(
                                            json.dumps(event) + "\n" for event in events
                                        )
                                        if native
                                        else timing
                                    )
                                    result.stderr = "" if captured else timing
                                return result

                        runner = self.helper_free_runner(TimingExecutor())
                        with contextlib.redirect_stderr(io.StringIO()):
                            if native:
                                receipts = runner.run_target(
                                    "core_lib", ["-E", "test(=mod::tests::alpha)"]
                                )
                                self.assertEqual(
                                    receipts, {"codex-core": ["mod::tests::alpha"]}
                                )
                            else:
                                with self.assertRaises(module.RunnerError) as failure:
                                    runner.run_target(
                                        "core_lib", ["-E", "test(=mod::tests::alpha)"]
                                    )
                                self.assertFalse(failure.exception.completed_tests)

    def test_native_interpretation_and_receipt_publication_are_reconciled(self):
        for gates in [False, True]:
            with self.subTest(gates=gates):
                runner = self.helper_free_runner()
                clock = [0.0]
                runner.metrics = ValidationMetrics([], clock=lambda: clock[0])
                original = module._nextest_results
                retain = runner._retain_text

                def parse(*args, **kwargs):
                    clock[0] += 5.0
                    return original(*args, **kwargs)

                def publish(text, *, prefix):
                    if prefix == "completed-tests-":
                        clock[0] += 2.0
                    return retain(text, prefix=prefix)

                with (
                    mock.patch.object(module, "_nextest_results", side_effect=parse),
                    mock.patch.object(runner, "_retain_text", side_effect=publish),
                    contextlib.redirect_stderr(io.StringIO()),
                ):
                    if gates:
                        receipts = runner.run_gates(["audit"])
                        self.assertEqual(receipts, {"audit": ["mod::tests::alpha"]})
                    else:
                        receipts = runner.run_target(
                            "core_lib", ["-E", "test(=mod::tests::alpha)"]
                        )
                        self.assertEqual(
                            receipts, {"codex-core": ["mod::tests::alpha"]}
                        )
                self.assertEqual(
                    runner.metrics.record["phases"]["reconciliation"],
                    5.0 if gates else 7.0,
                )
