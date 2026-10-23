"""Regression cases for Audit 141; fixtures never invoke Cargo."""

import contextlib
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.build import rust_build_status as lanes
from scripts.build import rust_test_runner as runner_module
from scripts.build.test_rust_test_runner import FakeExecutor, RunnerTestCase
from scripts.build.validation_metrics import ValidationMetrics


class SourceOwnershipTest(RunnerTestCase):
    def test_literal_includes_name_all_owners_and_direct_tests_without_narrowing_gates(
        self,
    ):
        crate = self.temp_dir / "core"
        sources = {
            "src/lib.rs": "mod owner; mod decoy;\n",
            "src/owner.rs": 'mod tests { include!("shared.rs"); }',
            "src/decoy.rs": '// include!("orphan.rs");\nconst S: &str = r#"include!("orphan.rs");"#;',
            "src/shared.rs": "#[test] fn covered() {} #[test] fn omitted() {} fn helper() {}",
            "src/orphan.rs": "#[test] fn fake() {}",
            "tests/core_shard.rs": 'mod suite { include!("../src/shared.rs"); }',
            "tests/core_shard_two.rs": 'include!(concat!("../src/", "shared.rs"));',
        }
        for name, source in sources.items():
            path = crate / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source, encoding="utf-8")
        runner, executor = self.runner(
            manifest=self.manifest(
                gates={
                    "broad": {
                        "steps": [
                            {
                                "target": "core_lib",
                                "tests": [
                                    "owner::tests::covered",
                                    "unrelated::retained",
                                ],
                            }
                        ]
                    },
                }
            )
        )
        package = runner.metadata.packages["codex-core"]
        package["manifest_path"] = str(crate / "Cargo.toml")
        package["targets"] = [
            {"name": name, "kind": [kind], "src_path": str(crate / source)}
            for name, kind, source in [
                ("codex_core", "lib", "src/lib.rs"),
                ("core_shard", "test", "tests/core_shard.rs"),
                ("core_shard_two", "test", "tests/core_shard_two.rs"),
            ]
        ]
        shared, orphan = (
            str(crate / "src" / name) for name in ("shared.rs", "orphan.rs")
        )
        advice = runner.gates_for([shared, orphan])
        self.assertEqual(advice["target_paths"][shared], ["core_lib", "core_shard"])
        self.assertEqual(advice["gates"], ["broad"])
        self.assertEqual(
            advice["direct_test_candidates"][shared],
            {
                "core_lib": ["owner::tests::covered", "owner::tests::omitted"],
                "core_shard": ["suite::covered", "suite::omitted"],
            },
        )
        self.assertEqual(advice["unresolved_targets"], [orphan])
        self.assertEqual(
            runner.gate("broad").steps[0].tests,
            (
                "owner::tests::covered",
                "unrelated::retained",
            ),
        )
        self.assertEqual(executor.calls, [])

        # Removed includes and declaration edits are visible next query.
        (crate / "src/owner.rs").write_text("", encoding="utf-8")
        (crate / "tests/core_shard.rs").write_text(
            'include!("../src/shared.rs");', encoding="utf-8"
        )
        (crate / "src/shared.rs").write_text(
            "#[test] fn changed() {}", encoding="utf-8"
        )
        advice = runner.gates_for([shared])
        self.assertEqual(advice["target_paths"][shared], ["core_shard"])
        self.assertEqual(
            advice["direct_test_candidates"][shared], {"core_shard": ["changed"]}
        )

    def test_include_cycles_and_attribute_decoys_do_not_create_test_candidates(self):
        crate = self.temp_dir / "core"
        crate.mkdir()
        root, shared = crate / "lib.rs", crate / "shared.rs"
        root.write_text('include!(r#"shared.rs"#);', encoding="utf-8")
        shared.write_text(
            'include!("lib.rs"); // #[test] fn comment() {}\n'
            'const S: &str = "#[test] fn literal() {}";\n'
            "#[tokio::test] async fn actual() {}\n"
            "#[test] fn duplicate() {} #[test] fn duplicate() {}\n"
            "mod nested { #[test] fn child() {} }",
            encoding="utf-8",
        )
        routes = runner_module._rust_module_routes(root, crate, {})
        self.assertEqual(routes[shared.resolve()], [""])
        self.assertEqual(routes[root.resolve()], [""])
        self.assertEqual(
            sorted(
                runner_module._rust_direct_test_candidates(shared.read_text(), "", {})
            ),
            ["actual", "nested::child"],
        )


class HelperSelectionTest(RunnerTestCase):
    def test_pure_modules_need_no_helpers_but_unknown_mixed_selection_keeps_fallback(
        self,
    ):
        manifest = runner_module.Manifest.load(runner_module.DEFAULT_MANIFEST)
        pure = [
            "tools::shell_output_summary_projection_tests::rustc_stderr_diagnostics_keep_constraints_through_both_output_paths",
            "workspace_operation_gate::tests::same_workspace_operations_are_serialized",
        ]
        for platform in ("linux", "windows"):
            for selected in (pure, [*pure, "unknown::needs_helpers"]):
                with self.subTest(platform=platform, selected=selected):
                    executor = FakeExecutor(
                        default_listing=dict.fromkeys(selected, False)
                    )
                    runner, _ = self.runner(
                        manifest=manifest, executor=executor, platform=platform
                    )
                    runner.metadata.packages["codex-windows-sandbox"]["targets"].append(
                        {
                            "name": "codex-windows-sandbox-setup",
                            "kind": ["bin"],
                        }
                    )

                    # Observe the real selection path before helper preparation;
                    # no fabricated binary artifacts or execution are needed.
                    class SelectionObserved(Exception):
                        pass

                    captured = []

                    def build(helpers):
                        captured.extend(helper.name for helper in helpers)
                        raise SelectionObserved

                    with mock.patch.object(runner, "_build_helpers", side_effect=build):
                        with self.assertRaises(SelectionObserved):
                            runner.run_target(
                                "core_lib",
                                [
                                    "-E",
                                    " | ".join(f"test(={name})" for name in selected),
                                ],
                            )
                    expected = (
                        []
                        if selected == pure
                        else [
                            helper.name
                            for helper in runner.active_helpers(["core_lib"])
                            if helper.name in manifest.target("core_lib").helpers
                        ]
                    )
                    self.assertEqual(captured, expected)
                    self.assertEqual(executor.calls, [])


class BusyLaneDiagnosticsTest(unittest.TestCase):
    def test_busy_siblings_report_actual_live_holders_without_creating_base(self):
        for count, capacity in ((1, None), (2, 2)):
            with self.subTest(count=count), tempfile.TemporaryDirectory() as temporary:
                repo = Path(temporary)
                root = repo / "codex-rs/target/lanes"
                lanes.initialize_cargo_lanes_root(repo, root)
                paths = [root / f"core-tests-{index + 2}" for index in range(count)]
                with contextlib.ExitStack() as stack:
                    for path in paths:
                        for part in ("deps", ".fingerprint", "incremental"):
                            (path / "debug" / part).mkdir(parents=True)
                        metrics = ValidationMetrics(["run-target", path.name])
                        metrics.bind(path)
                        stack.callback(metrics._release_owner)
                        stack.enter_context(
                            lanes.reserve_rust_test_target(
                                path,
                                holder={
                                    "run_id": metrics.record["run_id"],
                                    "ledger": str(metrics.path.resolve()),
                                },
                            )
                        )
                        handle = lanes._try_acquire_binary_file_lock(
                            path / ".lane-active.lock"
                        )
                        self.assertIsNotNone(handle)
                        stack.callback(handle.close)
                        stack.callback(lanes._release_binary_file_lock, handle)
                    with self.assertRaises(lanes.RustAdmissionBusy) as raised:
                        with lanes.reserve_cargo_lane(
                            repo_root=repo,
                            lane_root=root,
                            requested_lane="core-tests",
                            command=["cargo", "nextest", "run"],
                            max_active_lanes=capacity,
                        ):
                            self.fail("occupied family must not be admitted")
                    report = raised.exception.report()
                    self.assertIn(
                        report["resource"], [str(path.resolve()) for path in paths]
                    )
                    self.assertIsNotNone(report["holder"])
                    self.assertEqual(
                        {entry["resource"] for entry in report["blocking_lanes"]},
                        {str(path.resolve()) for path in paths},
                    )
                    self.assertTrue(
                        all(entry["holder"] for entry in report["blocking_lanes"])
                    )
                    self.assertFalse(report["executed"])
                    self.assertFalse(report["queued"])
                    self.assertFalse((root / "core-tests").exists())
                # Old descriptors and ledgers must not identify released holders.
                self.assertTrue(
                    all(lanes.lane_runner_holder(path) is None for path in paths)
                )


class PosixProcessDiscoveryTest(unittest.TestCase):
    def test_dispatch_and_bounded_scan_without_windows_creation_flags(self):
        with (
            mock.patch.object(lanes.os, "name", "posix"),
            mock.patch.object(
                lanes, "active_rust_processes_posix", return_value=[]
            ) as scan,
        ):
            self.assertEqual(lanes.active_rust_processes(), [])
            scan.assert_called_once_with()
        with mock.patch.object(lanes.subprocess, "run") as run:
            run.return_value.stdout = "123 /usr/bin/cargo cargo check --target-dir /tmp/lanes/core-tests\n124 sleep sleep 10\n"
            self.assertEqual(
                [process.pid for process in lanes.active_rust_processes_posix()], [123]
            )
            self.assertNotIn("creationflags", run.call_args.kwargs)
            self.assertEqual(run.call_args.kwargs["env"]["LC_ALL"], "C")
        for error in (OSError("unavailable"), subprocess.TimeoutExpired("ps", 10)):
            with mock.patch.object(lanes.subprocess, "run", side_effect=error):
                with self.assertRaises(lanes.RustProcessScanError):
                    lanes.active_rust_processes_posix()
        for snapshot in ("malformed snapshot\n", ""):
            with mock.patch.object(lanes.subprocess, "run") as run:
                run.return_value.stdout = snapshot
                with self.assertRaises(lanes.RustProcessScanError):
                    lanes.active_rust_processes_posix()
