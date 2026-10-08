"""Focused scheduling and completion regressions; no Cargo subprocesses."""

import contextlib
import copy
import io
import subprocess
from unittest import mock

from scripts import rust_build_status, rust_test_runner as runner_module
from scripts.test_rust_test_runner import FakeExecutor, MANIFEST_DATA, RunnerTestCase


class ValidationSchedulingTest(RunnerTestCase):
    def setUp(self):
        super().setUp()
        self.enterContext(contextlib.redirect_stderr(io.StringIO()))
        self.enterContext(contextlib.redirect_stdout(io.StringIO()))

    def pure_runner(self, *, output=None, returncode=0):
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["helpers"] = []
        runner, executor = self.runner(manifest=runner_module.Manifest.from_data(data))
        if output is not None:
            runner.executor = mock.Mock(return_value=subprocess.CompletedProcess(
                [], returncode, output, ""
            ))
        return runner, executor

    def test_unknown_plan_rejects_before_metadata(self):
        with (
            mock.patch.object(runner_module.Manifest, "load", return_value=self.manifest()),
            mock.patch.object(runner_module, "load_metadata") as metadata,
        ):
            self.assertEqual(runner_module.main(["plan", "unknown"]), 2)
            metadata.assert_not_called()

    def test_invalid_cargo_profile_rejects_before_metadata(self):
        for profile in ("../bad", "", "a/b", "a\\b"):
            with (
                self.subTest(profile=profile),
                mock.patch.object(runner_module, "load_metadata") as metadata,
            ):
                self.assertEqual(runner_module.main([
                    "--cargo-profile", profile, "run-target", "core_all"
                ]), 2)
                metadata.assert_not_called()

    def test_missing_cargo_target_rejects_before_admission(self):
        metadata = self.metadata()
        metadata.packages["codex-core"]["targets"] = []
        for selection in (["run-target", "core_all"], ["run-gate", "demo-gate"],
                          ["check-gates", "demo-gate"]):
            with (
                self.subTest(selection=selection),
                mock.patch.object(runner_module.Manifest, "load", return_value=self.manifest()),
                mock.patch.object(runner_module, "load_metadata", return_value=metadata),
                mock.patch.object(rust_build_status, "reserve_rust_test_target") as admission,
                mock.patch.object(runner_module, "execution_dependency_manifest") as provenance,
            ):
                self.assertEqual(runner_module.main(selection), 2)
                admission.assert_not_called()
                provenance.assert_not_called()

    def test_empty_filter_rejects_before_helpers(self):
        for args in (["-E", ""], ["--filterset", " \t"], ["--filterset= "]):
            with self.subTest(args=args):
                runner, executor = self.runner()
                with self.assertRaisesRegex(runner_module.RunnerError, "requires a value"):
                    runner.run_target("core_all", args)
                self.assertEqual(executor.calls, [])

    def test_empty_exact_selection_rejects_before_any_child_work(self):
        for tail in (["--skip", "alpha"], ["--skip=alpha"], ["beta"],
                     ["--exact", "alp"], ["--skip", ""]):
            with self.subTest(tail=tail):
                args = ["-E", "test(=alpha)", "--", *tail]
                runner, executor = self.runner()
                with self.assertRaisesRegex(runner_module.RunnerError, "zero tests"):
                    runner.run_target("core_all", args)
                self.assertEqual(executor.calls, [])
                with mock.patch.object(runner_module, "load_metadata") as metadata:
                    self.assertEqual(runner_module.main(["run-target", "core_all", *args]), 2)
                    metadata.assert_not_called()

    def test_exact_libtest_narrowing_needs_no_discovery_or_excluded_helpers(self):
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["helpers"] = []
        data["targets"]["core_lib"]["helpers_by_test_prefix"] = {"process::": ["codex"]}
        for expression, tail in (
            ("test(=pure::alpha)", ["--exact"]),
            ("test(=pure::alpha) | test(=process::beta)", ["--skip", "process::"]),
            ("test(=pure::alpha) | test(=process::beta)", ["--exact", "pure::alpha"]),
            ("test(=pure::alpha) | test(=process::beta)", ["pure::"]),
        ):
            with self.subTest(expression=expression, tail=tail):
                runner, executor = self.runner(
                    manifest=runner_module.Manifest.from_data(data),
                    executor=FakeExecutor(default_listing={"pure::alpha": False}),
                )
                args = ["-E", expression, "--", *tail]
                receipts = runner.run_target("core_lib", args)
                self.assertEqual(receipts, {"codex-core": ["pure::alpha"]})
                self.assertEqual(executor.commands(["cargo", "build"]), [])
                self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
                command, = executor.commands(["cargo", "nextest", "run"])
                self.assertEqual(command[-len(args):], args)
                self.assertEqual(receipts.required, receipts.executed)

    def test_success_exit_without_completion_receipts_is_not_proof(self):
        for output in ("", "Summary: 1 passed", "PASS [0.01s] wrong-binary alpha",
                       "SKIP [0.01s] codex-core alpha"):
            with self.subTest(output=output):
                runner, _ = self.pure_runner(output=output)
                with self.assertRaises(runner_module.RunnerError) as error:
                    runner.run_target("core_lib", ["-E", "all()"])
                self.assertEqual(error.exception.outcome, "not_executed")
                self.assertEqual(error.exception.completed_tests, {})
                runner.executor.assert_called_once()

    def test_success_exit_with_conflicting_results_preserves_only_independent_proof(self):
        for statuses in (("PASS", "PASS"), ("PASS", "FAIL"), ("FAIL",), ("TIMEOUT",)):
            with self.subTest(statuses=statuses):
                output = "PASS [0.01s] codex-core good\n" + "".join(
                    f"{status} [0.01s] codex-core bad\n" for status in statuses
                )
                runner, _ = self.pure_runner(output=output)
                with self.assertRaises(runner_module.RunnerError) as error:
                    runner.run_target("core_lib", ["-E", "all()"])
                self.assertEqual(error.exception.outcome, "not_executed")
                self.assertEqual(error.exception.completed_tests, {"codex-core": ["good"]})
                runner.executor.assert_called_once()

    def test_staging_failure_preserves_proof_and_only_blocks_dependent_batch(self):
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"] = {
            "before": {"steps": [{"target": "core_lib", "tests": ["alpha"], "helpers": []}]},
            "broken": {"steps": [{"target": "core_lib", "tests": ["alpha"], "helpers": ["codex"]}]},
            "after": {"steps": [{"target": "core_all", "tests": ["alpha"], "helpers": []}]},
        }
        for outcome in ("failed", "cancelled", "timed_out", "cleanup_failed"):
            with self.subTest(outcome=outcome):
                runner, executor = self.runner(
                    manifest=runner_module.Manifest.from_data(data),
                    executor=FakeExecutor(default_listing={"alpha": False}, artifacts={
                        "codex": self.helper_executable("codex")
                    }),
                )
                original = runner._helper_environment

                def stage(targets, helpers, artifacts):
                    if helpers:
                        raise runner_module.RunnerError("staging failed", outcome=outcome)
                    return original(targets, helpers, artifacts)

                with (
                    mock.patch.object(runner, "_helper_environment", side_effect=stage),
                    self.assertRaises(runner_module.RunnerError) as error,
                ):
                    runner.run_gates(["before", "broken", "after"], quiet=True)
                expected = {"before": ["alpha"]}
                if outcome == "failed":
                    expected["after"] = ["alpha"]
                self.assertEqual(error.exception.completed_gates, expected)
                self.assertEqual(error.exception.outcome, outcome)
                self.assertEqual(len(executor.commands(["cargo", "build"])), 1)
                self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), len(expected))

    def test_changed_runner_selects_scheduling_regressions_once(self):
        from scripts import root_maintenance

        targets = root_maintenance.python_test_targets([], [
            "scripts/rust_test_runner.py", "scripts/test_validation_scheduling.py"
        ])
        self.assertEqual(targets.count("scripts.test_validation_scheduling"), 1)
        self.assertIn("scripts.test_rust_test_runner", targets)

    def test_known_selection_failure_does_not_rescan_count(self):
        runner, _ = self.pure_runner(output="FAIL [0.01s] codex-core alpha", returncode=100)
        with (
            mock.patch.object(runner_module, "_nextest_selected_count") as count,
            self.assertRaises(runner_module.RunnerError),
        ):
            runner.run_target("core_lib", ["-E", "test(=alpha)"])
        count.assert_not_called()
        runner, _ = self.pure_runner(output="FAIL [0.01s] codex-core alpha", returncode=100)
        with (
            mock.patch.object(runner_module, "_nextest_selected_count", return_value=1) as count,
            self.assertRaises(runner_module.RunnerError),
        ):
            runner.run_target("core_lib", ["-E", "all()"])
        count.assert_called_once()
