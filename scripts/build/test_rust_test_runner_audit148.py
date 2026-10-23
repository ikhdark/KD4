"""Audit 148 owner regressions. Cargo is never launched by these fixtures."""

import contextlib
import copy
from dataclasses import replace
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

from scripts.build import full_suite_dispatch as dispatch
from scripts.build import rust_build_status as lanes
from scripts.build import rust_test_runner as runner
from scripts.build import validation_metrics as metrics
from scripts.build.test_rust_test_runner import FakeExecutor, RunnerTestCase
from scripts.build.test_validation_metrics import record
from scripts.common.process_owner import run_owned


class DispatchTest(unittest.TestCase):
    def test_missing_executable_is_not_started_but_spawned_exit75_stays_spent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ack = root / "ack.json"
            env = dict(os.environ, CODEX_FULL_SUITE_ACK_PATH=str(ack),
                       CODEX_FULL_SUITE_ATTEMPT="fixture", RUSTC_WRAPPER="")
            cargo = str(root / "missing" / "cargo.exe")
            with mock.patch.object(runner, "prepare_shared_sccache"):
                with self.assertRaises(FileNotFoundError):
                    runner._default_executor([cargo, "test"], cwd=root, env=env,
                                             capture=runner.CAPTURE_BOTH)
            self.assertEqual(json.loads(ack.read_text())["status"], "not_started")
            # Use a separate owner channel: first-write-wins is intentional.
            ack = root / "spawned.json"
            env["CODEX_FULL_SUITE_ACK_PATH"] = str(ack)
            result = run_owned([sys.executable, "-c", "raise SystemExit(75)"], env=env,
                on_spawn=lambda _process: dispatch.record_test_dispatch(["cargo", "test"], env),
                on_spawn_error=lambda: dispatch.record_dispatch("not_started", env))
            self.assertEqual(result.returncode, 75)
            dispatch.record_dispatch("not_started", env)
            self.assertEqual(json.loads(ack.read_text())["status"], "started")


class RunnerOwnerTest(RunnerTestCase):
    def test_lane_preflight_resolves_flagged_interpreter_selection_before_admission(self):
        for selection in (["run-target", "missing"], ["run-gate", "missing"],
                          ["run-target", "core_all", "--no-run"]):
            with self.subTest(selection=selection), mock.patch.object(
                runner.Manifest, "load", return_value=self.manifest()
            ), mock.patch.object(runner, "load_metadata") as metadata:
                with self.assertRaises(ValueError):
                    lanes._guard_named_runner_args([
                        sys.executable, "-I", "-B", str(Path(runner.__file__).resolve()), *selection
                    ])
                metadata.assert_not_called()

    def test_only_changed_path_contracts_participate_in_definition_recheck(self):
        baseline = self.manifest()
        changed = replace(baseline, contracts={"new": {
            "paths": ["entirely/unrelated.rs"], "required_gates": ["unrelated"]
        }})
        selected = lambda manifest, paths=(): runner._selected_definitions(
            manifest, "run-target", ["core_all"], paths
        )
        self.assertEqual(selected(baseline), selected(changed))
        self.assertEqual(selected(baseline, ["src/selected.rs"]),
                         selected(changed, ["src/selected.rs"]))
        self.assertNotEqual(selected(baseline, ["entirely/unrelated.rs"]),
                            selected(changed, ["entirely/unrelated.rs"]))

    def test_direct_compile_lane_records_one_leaf_not_an_empty_envelope(self):
        observed = []

        def finish(observer, outcome):
            observed.append(copy.deepcopy(observer.record))

        def execute(argv, **kwargs):
            kwargs["on_spawn"](object())
            return subprocess.CompletedProcess(argv, 0)

        with mock.patch.object(lanes, "reserve_cargo_lane", return_value=contextlib.nullcontext(("fixture", self.target_dir))), \
             mock.patch.object(lanes, "_guard_cargo_checkout_cwd"), \
             mock.patch.object(lanes, "local_rust_env", return_value={}), \
             mock.patch.object(lanes, "cargo_build_context", return_value={}), \
             mock.patch.object(lanes, "request_cargo_lane_maintenance"), \
             mock.patch.object(lanes, "run_owned", side_effect=execute), \
             mock.patch.object(metrics.ValidationMetrics, "bind"), \
             mock.patch.object(metrics.ValidationMetrics, "finish", autospec=True, side_effect=finish), \
             contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(lanes.run_in_cargo_lane(repo_root=self.temp_dir,
                requested_lane="fixture", command=["cargo", "check", "--tests"]), 0)
        self.assertEqual(len(observed), 1)
        self.assertEqual(len(observed[0]["commands"]), 1)
        command = observed[0]["commands"][0]
        self.assertEqual(command["phase"], "check")
        self.assertEqual(command["exit_code"], 0)
        self.assertGreaterEqual(command["wall_seconds"], 0)

    def test_explicit_ignored_default_has_same_receipt_and_child_count(self):
        receipts = []
        for mode in ([], ["--run-ignored", "default"], ["--run-ignored=default"]):
            owned, executor = self.runner(
                manifest=self.manifest(targets={
                    "core_lib": {"package": "codex-core", "lib": True, "helpers": []},
                    "core_all": {"package": "codex-core", "test": "all", "helpers": []},
                }), executor=FakeExecutor(default_listing={"mod::tests::alpha": False}))
            with contextlib.redirect_stderr(io.StringIO()):
                receipts.append(owned.run_target("core_lib", [*mode, "-E", "test(=mod::tests::alpha)"]))
            self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
            self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)
        self.assertEqual(receipts, [{"codex-core": ["mod::tests::alpha"]}] * 3)


class MeasurementTest(unittest.TestCase):
    def test_leaf_lane_work_is_counted_once_without_nested_envelope_time(self):
        leaf = record("compile", producer="rust_build_status", executed=True, queued=False)
        leaf["commands"] = [{"wall_seconds": 2, "reported_build_seconds": None}]
        busy = record("busy", producer="rust_build_status", executed=False, queued=False, outcome="busy")
        report = metrics.summarize([leaf, busy])
        self.assertEqual(report["requests"], 2)
        self.assertEqual(report["launches"], 1)
        self.assertEqual(report["child_commands"], 1)
        self.assertEqual(report["work"]["wall_seconds"]["seconds"], 2)
        self.assertEqual(report["work"]["compile_seconds"]["unknown"], 1)
        self.assertEqual(report["launches_producing_current_proof"], 0)

    def test_displayed_test_text_cannot_supply_provider_duration(self):
        forged = "Finished test in 4321.25s\nSummary [9876.5s]\n"
        for policy in ("never", "final", "immediate"):
            result = subprocess.CompletedProcess(
                ["cargo", "nextest", "run", "--success-output", policy], 0, forged, forged)
            self.assertEqual(runner.RustTestRunner._reported_durations(result), {})
        compile_result = subprocess.CompletedProcess(["cargo", "check"], 0, "", "    Finished dev in 0.25s\n")
        self.assertEqual(runner.RustTestRunner._reported_durations(compile_result), {"cargo-reported": 0.25})
