#!/usr/bin/env python3

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_root_maintenance_module
from scripts.build_tooling_test_support import powershell


class RootMaintenanceTest(unittest.TestCase):
    @contextlib.contextmanager
    def temporary_script_tree(self, maintenance):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            for name in ("readme_toc.py", "test_readme_toc.py", "unmapped_helper.py"):
                (scripts / name).write_text("", encoding="utf-8")
            with (
                mock.patch.object(maintenance, "REPO_ROOT", root),
                mock.patch.object(maintenance, "SCRIPTS_ROOT", scripts),
                mock.patch.object(maintenance, "SCRIPT_AUDIT_ROOTS", (scripts,)),
            ):
                yield scripts

    def test_root_maintenance_covers_current_script_tooling_tests(self) -> None:
        root_maintenance = load_root_maintenance_module()

        source_paths = []
        for root in root_maintenance.SCRIPT_AUDIT_ROOTS:
            for directory, dirs, files in os.walk(root):
                dirs[:] = [
                    name for name in dirs if name not in {".venv", "__pycache__"}
                ]
                source_paths.extend(
                    Path(directory) / name for name in files if name.endswith(".py")
                )
        expected_ruff_targets = sorted(
            path.relative_to(REPO_ROOT).as_posix() for path in source_paths
        )
        expected_unittest_targets = sorted(
            path.relative_to(REPO_ROOT).with_suffix("").as_posix().replace("/", ".")
            if root_maintenance.SCRIPTS_ROOT in path.parents
            else path.relative_to(REPO_ROOT).as_posix()
            for path in source_paths
            if path.name.startswith("test_")
        )

        self.assertEqual(
            root_maintenance.python_source_targets(), expected_ruff_targets
        )
        self.assertEqual(
            root_maintenance.python_unittest_targets(), expected_unittest_targets
        )
        self.assertEqual(
            root_maintenance.python_test_targets(
                ["scripts.test_build_tooling_policy"], []
            ),
            ["scripts.test_build_tooling_policy"],
        )
        self.assertEqual(
            root_maintenance.python_test_targets([], ["scripts/root_maintenance.py"]),
            [
                "scripts.test_report_script_regressions.ScriptReportRegressions.test_changed_tests_and_adjacent_production_across_owned_roots",
                "scripts.test_report_script_regressions.ScriptReportRegressions.test_report_regressions_follow_their_changed_owners_once",
                "scripts.test_root_maintenance",
            ],
        )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/root_maintenance.py", "docs/example.md"],
        ):
            self.assertEqual(
                root_maintenance.expand_changed_paths([None]),
                ["scripts/root_maintenance.py", "docs/example.md"],
            )
        with mock.patch.object(
            root_maintenance,
            "git_changed_paths",
            return_value=["scripts/root_maintenance.py"],
        ):
            self.assertEqual(
                root_maintenance.python_test_targets(
                    [], root_maintenance.expand_changed_paths([None])
                ),
                [
                    "scripts.test_report_script_regressions.ScriptReportRegressions.test_changed_tests_and_adjacent_production_across_owned_roots",
                    "scripts.test_report_script_regressions.ScriptReportRegressions.test_report_regressions_follow_their_changed_owners_once",
                    "scripts.test_root_maintenance",
                ],
            )
        self.assertEqual(
            root_maintenance.test_module_for_changed_path("docs/example.md"),
            None,
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/publish-local-codex.ps1"
            ),
            (
                "scripts.test_publish_local_codex",
                "scripts.test_publish_local_codex_apply",
                "scripts.test_publish_local_codex_build",
                "scripts.test_publish_local_codex_dry_run",
                "scripts.test_publish_local_codex_freshness",
            ),
        )
        self.assertEqual(root_maintenance.python_lint_targets(["docs/example.md"]), [])
        self.assertEqual(
            root_maintenance.python_test_targets([], ["docs/example.md"]), []
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "Scripts/Test_Asciicheck.PY"
            ),
            ("scripts.test_asciicheck",),
        )

    def test_root_maintenance_routes_aggregate_python_script_tests(self) -> None:
        root_maintenance = load_root_maintenance_module()

        self.assertEqual(
            root_maintenance.python_test_targets(
                [],
                [
                    "scripts/generated_output_lock.py",
                    "scripts/kd4_model_attempt_analysis.py",
                ],
            ),
            [
                "scripts.test_dev_environment",
                "scripts.test_kd4_model_inference",
                "scripts.test_kd4_perf_snapshot",
                "scripts.test_report_script_regressions.Report26ValidationRegressions.test_lock_waiter_never_writes_to_an_empty_owned_file",
                "scripts.test_report_script_regressions.Report26ValidationRegressions.test_lock_waits_for_release_and_distinguishes_timeout_from_io_error",
            ],
        )
        self.assertEqual(
            root_maintenance.python_test_targets(
                [], ["scripts/kd4_perf_snapshot.py"]
            ),
            ["scripts.test_kd4_model_inference", "scripts.test_kd4_perf_snapshot"],
        )

    def test_script_test_entrypoints_have_explicit_validation_declarations(self):
        maintenance = load_root_maintenance_module()
        config = json.loads((REPO_ROOT / ".codex/test-runners.json").read_text())
        declarations = [row for row in config["runners"]
                        if any("scripts/root_maintenance.py" in prefix
                               for prefix in row["prefixes"])]
        # Classify only the public test subcommand, not audit-scripts (whose
        # --quick mode omits tests) or arbitrary scripts through run-python.js.
        self.assertEqual(declarations, [
            {
                "programs": ["python", "python3", "py"],
                "prefixes": [["scripts/root_maintenance.py", "test-python"]],
                "options": {"-B": 0, "-u": 0},
                "allow_extra_args": True, "operations": ["test"],
            },
            {
                "programs": ["node"],
                "prefixes": [["scripts/run-python.js", "scripts/root_maintenance.py", "test-python"]],
                "allow_extra_args": True, "operations": ["test"],
            },
        ])
        for option, code in (("--help", 0), ("--dry-run", 2), ("--list", 2),
                             ("--no-run", 2), ("--quick", 2)):
            with (self.subTest(option=option),
                  mock.patch.object(maintenance, "run") as run,
                  contextlib.redirect_stdout(io.StringIO()),
                  contextlib.redirect_stderr(io.StringIO()),
                  self.assertRaises(SystemExit) as exit_status):
                maintenance.main(["test-python", option])
            self.assertEqual(exit_status.exception.code, code)
            run.assert_not_called()
        self.assertIn(
            "scripts.test_root_maintenance.RootMaintenanceTest.test_script_test_entrypoints_have_explicit_validation_declarations",
            maintenance.python_test_targets([], [".codex/test-runners.json"]),
        )

    def test_runner_changes_select_split_regressions_once_through_cli(self):
        maintenance = load_root_maintenance_module()
        # These modules exercise runner dispatch, admission, log diagnostics,
        # metrics and scheduling; adjacency alone cannot discover split tests.
        expected = {
            "scripts.test_rust_test_runner",
            "scripts.test_rust_test_admission",
            "scripts.test_rust_test_runner_failure_diagnostics",
            "scripts.test_validation_metrics",
            "scripts.test_validation_scheduling",
        }
        with mock.patch.object(maintenance, "run", return_value=0) as run:
            self.assertEqual(maintenance.main([
                "test-python", "--changed", "scripts/rust_test_runner.py",
                "--changed", "scripts/rust_build_status.py",
                "--changed", "scripts/test_validation_scheduling.py",
            ]), 0)
        command = run.call_args.args[0]
        for module in expected:
            self.assertEqual(command.count(module), 1, module)
        self.assertIn("scripts.test_build_tooling_storage", command)

    def test_changed_script_validation_runs_focused_tests_and_skips_retired_scripts(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()
        # Each script is executed or asserted by this focused test module.
        focused_tests = {
            "codex-cli/scripts/build_npm_package.py": "scripts.test_stage_npm_packages",
            "codex-rs/config/scripts/generate-proto.ps1": (
                "scripts.test_generate_config_proto"
            ),
            "codex-rs/scripts/nextest_windows_stack.py": (
                "scripts.test_build_tooling_policy"
            ),
            "codex-rs/scripts/setup-windows.ps1": "scripts.test_build_tooling",
            "scripts/benchmark_code_mode_handoffs.mjs": (
                "scripts.test_benchmark_code_mode_handoffs"
            ),
            "scripts/cargo-lane-patterns.ps1": "scripts.test_cargo_lane",
            "scripts/cargo-workspace-analyzer.ps1": "scripts.test_build_tooling_policy",
            "scripts/run-python.js": "scripts.test_build_tooling_policy",
            "codex-cli/bin/codex.js": "scripts.test_build_tooling_policy",
            "justfile": "scripts.test_root_maintenance",
            "scripts/build_tooling_test_support.py": "scripts.test_build_tooling",
            "scripts/root_maintenance.py": "scripts.test_root_maintenance",
        }
        for path, module in focused_tests.items():
            with (
                self.subTest(path=path),
                mock.patch.object(root_maintenance, "run", return_value=0) as run,
            ):
                self.assertEqual(
                    root_maintenance.main(["test-python", "--changed", path]), 0
                )
                self.assertIn(module, run.call_args.args[0])

        # Retiring a script and its test leaves nothing to verify or import.
        retired = [
            "scripts/retired_example_tool.py",
            "scripts/test_retired_example_tool.py",
        ]
        self.assertFalse(any((REPO_ROOT / path).exists() for path in retired))
        with (
            mock.patch.object(root_maintenance, "run", return_value=0) as run,
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", *(f"--changed={path}" for path in retired)]
                ),
                0,
            )
        run.assert_not_called()

    def test_root_maintenance_script_audit_plan_covers_every_script_type(self) -> None:
        root_maintenance = load_root_maintenance_module()
        tools = {
            "uv": "uv",
            "pwsh": "pwsh",
            "node": "node",
        }

        commands, missing = root_maintenance.script_audit_commands(
            include_tests=True,
            test_targets=["scripts.test_asciicheck"],
            resolve_tool=tools.get,
        )

        self.assertEqual(missing, [])
        labels = [label for label, _command in commands]
        self.assertIn("Python format", labels)
        self.assertIn("Python lint", labels)
        self.assertIn("PowerShell syntax", labels)
        self.assertIn("justfile PowerShell syntax", labels)
        self.assertIn("justfile Python syntax", labels)
        self.assertIn(
            "Parser]::ParseInput",
            dict(commands)["justfile PowerShell syntax"][-1],
        )
        javascript_targets = [
            target
            for target, kind in root_maintenance.script_kind_map().items()
            if kind == "javascript"
        ]
        self.assertEqual(
            any(label.startswith("JavaScript syntax:") for label in labels),
            bool(javascript_targets),
        )
        self.assertTrue(javascript_targets, "fixture must exercise Node syntax checks")
        self.assertEqual(
            [(label, command) for label, command in commands
             if label.startswith("JavaScript syntax:")],
            [(f"JavaScript syntax: {target}", ("node", "--check", target))
             for target in javascript_targets],
        )
        self.assertIn("script unit tests", labels)
        unit_command = dict(commands)["script unit tests"]
        self.assertIn("scripts.test_asciicheck", unit_command)
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/common-rust-env.ps1"
            ),
            ("scripts.test_build_tooling_performance",),
        )
        self.assertEqual(
            root_maintenance.test_modules_for_changed_path(
                "scripts/rust_build_status.py"
            ),
            (
                "scripts.test_build_tooling_storage",
                "scripts.test_report_script_regressions.ScriptReportRegressions.test_recent_overflow_survives_expired_base",
                "scripts.test_rust_test_admission",
            ),
        )
        commands_without_tests, _missing = root_maintenance.script_audit_commands(
            include_tests=True,
            test_targets=[],
            resolve_tool=tools.get,
        )
        self.assertNotIn(
            "script unit tests",
            [label for label, _command in commands_without_tests],
        )

    def test_root_maintenance_parses_every_just_recipe_as_powershell(self) -> None:
        ps = powershell()
        if ps is None:
            self.skipTest("PowerShell is required for justfile syntax validation")
        root_maintenance = load_root_maintenance_module()
        commands, missing = root_maintenance.script_audit_commands(
            include_tests=False,
            resolve_tool={"uv": "uv", "pwsh": ps, "node": "node"}.get,
        )
        self.assertEqual(missing, [])
        result = subprocess.run(
            dict(commands)["justfile PowerShell syntax"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_justfile_script_recipes_are_checked_by_their_own_interpreter(self) -> None:
        root_maintenance = load_root_maintenance_module()

        powershell_sources = root_maintenance.just_powershell_sources()
        python_sources = root_maintenance.just_python_sources()

        self.assertTrue(any("cargo run" in source for _, source in powershell_sources))
        self.assertFalse(
            any("import runpy" in source for _, source in powershell_sources)
        )
        self.assertTrue(any("import runpy" in source for _, source in python_sources))
        for name, source in python_sources:
            compile(source, name, "exec")

    def test_root_maintenance_script_inventory_covers_owned_script_roots(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        expected_kinds = {
            "codex-cli/bin/codex.js": "javascript",
            "codex-cli/scripts/build_npm_package.py": "python",
            "codex-rs/app-server-test-client/scripts/live_elicitation_hold.ps1": "powershell",
            "codex-rs/config/scripts/generate-proto.ps1": "powershell",
            "codex-rs/responses-api-proxy/npm/bin/codex-responses-api-proxy.js": "javascript",
            "codex-rs/scripts/nextest_windows_stack.py": "python",
            "codex-rs/skills/src/assets/samples/imagegen/scripts/image_gen.py": "python",
            "sdk/python/scripts/update_sdk_artifacts.py": "python",
            "tools/argument-comment-lint/run.py": "python",
        }
        kind_by_target = root_maintenance.script_kind_map()

        for target, expected_kind in expected_kinds.items():
            with self.subTest(target=target):
                self.assertEqual(kind_by_target.get(target), expected_kind)
        self.assertIn(
            "tools/argument-comment-lint/test_wrapper_common.py",
            root_maintenance.python_unittest_targets(),
        )

    def test_root_maintenance_does_not_route_retired_task_continuity_paths(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        for target in (
            ".codex/hooks.json",
            ".codex/hooks/task-continuity-entry.ps1",
            ".codex/hooks/task-continuity-fast-basic.ps1",
            ".codex/hooks/task-continuity-fast-compact.ps1",
            ".codex/hooks/task-continuity-fast-session.ps1",
            ".codex/hooks/task-continuity.ps1",
        ):
            with self.subTest(target=target):
                self.assertNotIn(target, root_maintenance.SCRIPT_TEST_MODULES)

    def test_root_maintenance_script_audit_current_tree_has_no_hard_findings(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        errors, _advisories = root_maintenance.script_audit_findings()

        self.assertEqual(errors, [])

    def test_root_maintenance_script_audit_context_matches_current_routes(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()

        self.assertEqual(root_maintenance.script_audit_context_issues(), [])

    def test_root_maintenance_script_audit_has_no_platform_skips(self) -> None:
        root_maintenance = load_root_maintenance_module()

        targets = root_maintenance.script_audit_test_targets()

        self.assertNotIn("scripts.install.test_install_sh", targets)
        self.assertFalse(any(target.endswith("_sh") for target in targets))

    def test_root_maintenance_script_audit_success_has_no_stale_skip_summary(
        self,
    ) -> None:
        root_maintenance = load_root_maintenance_module()
        stdout = io.StringIO()

        with (
            mock.patch.object(
                root_maintenance,
                "script_source_targets",
                return_value=["scripts/example.py"],
            ),
            mock.patch.object(
                root_maintenance,
                "script_kind_map",
                return_value={"scripts/example.py": "python"},
            ),
            mock.patch.object(
                root_maintenance, "script_audit_context_issues", return_value=[]
            ),
            mock.patch.object(
                root_maintenance, "script_audit_findings", return_value=([], [])
            ),
            mock.patch.object(
                root_maintenance, "script_audit_test_targets", return_value=[]
            ),
            mock.patch.object(
                root_maintenance, "script_audit_commands", return_value=([], [])
            ),
            mock.patch.object(
                root_maintenance, "git_context_label", return_value="test"
            ),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(
                root_maintenance.run_script_audit(
                    include_tests=True,
                    strict=False,
                ),
                0,
            )

        self.assertIn(
            "SCRIPT AUDIT PASSED: 1 script artifact(s), 0 command group(s), "
            "0 advisory item(s).",
            stdout.getvalue(),
        )
        self.assertNotIn("platform test skip", stdout.getvalue())

    def test_root_maintenance_git_paths_use_nul_delimiters(self) -> None:
        root_maintenance = load_root_maintenance_module()
        tracked = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/line\nbreak.py\0",
            stderr="",
        )
        untracked = subprocess.CompletedProcess(
            ["git"],
            0,
            stdout="scripts/ trailing .py\0scripts/line\nbreak.py\0",
            stderr="",
        )

        with mock.patch.object(
            root_maintenance.subprocess, "run", side_effect=[tracked, untracked]
        ) as run:
            paths = root_maintenance.git_changed_paths()

        self.assertEqual(
            paths,
            ["scripts/line\nbreak.py", "scripts/ trailing .py"],
        )
        self.assertEqual(run.call_count, 2)
        self.assertIn("-z", run.call_args_list[0].args[0])
        self.assertIn("--diff-filter=ACDMRTUXB", run.call_args_list[0].args[0])
        self.assertIn("--others", run.call_args_list[1].args[0])
        self.assertIn("-z", run.call_args_list[1].args[0])

    def test_changed_production_script_without_tests_is_unverified(self) -> None:
        root_maintenance = load_root_maintenance_module()

        with (
            self.temporary_script_tree(root_maintenance) as scripts,
            mock.patch.object(root_maintenance, "run") as run,
            mock.patch.object(
                root_maintenance, "script_inventory",
                side_effect=AssertionError("broad discovery"),
            ),
        ):
            self.assertEqual(
                root_maintenance.test_modules_for_changed_path("scripts/readme_toc.py"),
                ("scripts.test_readme_toc",),
            )
            for extra in ([], ["--changed", "scripts/readme_toc.py"]):
                with (self.subTest(extra=extra),
                      contextlib.redirect_stderr(io.StringIO()) as stderr):
                    self.assertEqual(
                        root_maintenance.main([
                            "test-python", "--changed", "scripts/unmapped_helper.py", *extra,
                        ]),
                        2,
                    )
                self.assertEqual(
                    stderr.getvalue(),
                    "Changed production script validation is unverified: no focused test route for "
                    "scripts/unmapped_helper.py\n",
                )
                run.assert_not_called()
            (scripts / "unmapped_helper.py").unlink()
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", "--changed", "scripts/unmapped_helper.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(["test-python", "--changed", "docs/example.md"]),
                0,
            )

        run.assert_not_called()

    def test_root_maintenance_missing_command_is_reported(self) -> None:
        root_maintenance = load_root_maintenance_module()
        stderr = io.StringIO()

        with (
            mock.patch.object(
                root_maintenance.subprocess,
                "run",
                side_effect=FileNotFoundError("missing"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(root_maintenance.run(["missing-tool"]), 127)

        self.assertIn("Could not run missing-tool", stderr.getvalue())

    def test_root_maintenance_does_not_duplicate_formatter_commands(self) -> None:
        root_maintenance = load_root_maintenance_module()
        subcommands = root_maintenance.build_parser()._subparsers._group_actions[0]

        self.assertNotIn("format-prettier", subcommands.choices)
        self.assertNotIn("format-python", subcommands.choices)

    def test_python_tests_route_harbor_and_preserve_failures(self):
        maintenance = load_root_maintenance_module()
        harbor = "scripts.test_harbor_windows_codex.TerminalTests"
        ordinary = "scripts.test_asciicheck"
        for codes, expected in (([0, 0], 0), ([7, 0], 7), ([0, 9], 9)):
            with (
                self.subTest(codes=codes),
                mock.patch.object(maintenance, "harbor_python", return_value=Path("harbor-python")),
                mock.patch.object(maintenance, "run", side_effect=codes) as run,
            ):
                self.assertEqual(
                    maintenance.main(["test-python", "--module", ordinary, "--module", harbor]),
                    expected,
                )
                self.assertEqual(run.call_args_list, [
                    mock.call([*maintenance.UV_RUN_SCRIPTS, "python", "-m", "unittest", ordinary, "-v"]),
                    mock.call(["harbor-python", "-m", "unittest", harbor, "-v"]),
                ])

    def test_missing_harbor_environment_fails_without_skipping_ordinary_tests(self):
        maintenance = load_root_maintenance_module()
        with (
            mock.patch.object(maintenance, "harbor_python", side_effect=ValueError("missing Harbor")),
            mock.patch.object(maintenance, "run", return_value=0) as run,
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            self.assertEqual(maintenance.run_python_tests([
                "scripts.test_asciicheck", "scripts.test_harbor_windows_codex",
            ]), 2)
        run.assert_called_once()
        self.assertIn("Could not run Harbor script tests", stderr.getvalue())

    def test_script_audit_uses_shared_test_environment_routing(self):
        maintenance = load_root_maintenance_module()
        targets = ["scripts.test_asciicheck", "scripts.test_harbor_windows_codex"]
        commands, _missing = maintenance.script_audit_commands(
            include_tests=True, test_targets=targets, resolve_tool=lambda name: name,
        )
        self.assertEqual(dict(commands)["script unit tests"], (
            *maintenance.UV_RUN_SCRIPTS, "python", "scripts/root_maintenance.py",
            "test-python", "--module", targets[0], "--module", targets[1],
        ))

    def test_root_maintenance_uv_commands_use_frozen_lock(self) -> None:
        root_maintenance = load_root_maintenance_module()
        calls: list[tuple[str, ...]] = []

        def fake_run(command: list[str]) -> int:
            calls.append(tuple(command))
            return 0

        with mock.patch.object(root_maintenance, "run", side_effect=fake_run):
            self.assertEqual(
                root_maintenance.main(
                    ["lint-python", "--changed", "scripts/root_maintenance.py"]
                ),
                0,
            )
            self.assertEqual(
                root_maintenance.main(
                    ["test-python", "--module", "scripts.test_build_tooling_policy"]
                ),
                0,
            )

        self.assertEqual(
            calls,
            [
                (
                    "uv",
                    "run",
                    "--frozen",
                    "--project",
                    "scripts",
                    "ruff",
                    "check",
                    "scripts/root_maintenance.py",
                ),
                (
                    "uv",
                    "run",
                    "--frozen",
                    "--project",
                    "scripts",
                    "python",
                    "-m",
                    "unittest",
                    "scripts.test_build_tooling_policy",
                    "-v",
                ),
            ],
        )


class StructuredMaintenanceTest(unittest.TestCase):
    def setUp(self):
        self.owner = load_root_maintenance_module()
        self.output = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.errors = self.enterContext(contextlib.redirect_stderr(io.StringIO()))
        self.directory = self.enterContext(tempfile.TemporaryDirectory())
        self.root = Path(self.directory)
        self.enterContext(mock.patch.object(self.owner, "REPO_ROOT", self.root))

    def test_json_worker_uses_file_backed_transport_and_rejects_invalid_output(self):
        code, report = self.owner._run_json_worker(
            [sys.executable, "-c", "import json,sys; print(json.dumps(json.load(sys.stdin)))"],
            request={"text": "λ😀"}, timeout=5,
        )
        self.assertEqual((code, report), (0, {"text": "λ😀"}))
        with self.assertRaises(ValueError):
            self.owner._run_json_worker([sys.executable, "-c", "print('not JSON')"])
        with mock.patch.object(self.owner, "JSON_WORKER_MAX_BYTES", 16):
            with self.assertRaisesRegex(ValueError, "request exceeds"):
                self.owner._run_json_worker(["must-not-start"], request={"x": "a" * 30})
            with self.assertRaisesRegex(ValueError, "report exceeds"):
                self.owner._run_json_worker([sys.executable, "-c", "print('x'*32)"])

    def test_json_worker_timeout_reaps_a_stalled_child(self):
        ready = self.root / "ready"
        command = [sys.executable, "-c", (
            f"import pathlib,time; pathlib.Path({str(ready)!r}).write_text('ready'); time.sleep(60)"
        )]
        with self.assertRaises(subprocess.TimeoutExpired):
            self.owner._run_json_worker(command, timeout=2)
        self.assertTrue(ready.exists(), "the fixture must start before timeout")

    def write_test(self, name, body="pass"):
        scripts = self.root / "scripts"
        scripts.mkdir(exist_ok=True)
        (scripts / "__init__.py").write_text("", encoding="utf-8")
        (scripts / f"{name}.py").write_text(
            "import unittest\nclass Example(unittest.TestCase):\n"
            f"    def test_example(self):\n        {body}\n", encoding="utf-8",
        )
        return f"scripts.{name}"

    def run_json(self, targets, *, missing_harbor=False):
        original = self.owner._run_json_worker
        commands = []

        def execute(command, **kwargs):
            commands.append(command)
            # Only substitute the environment launcher; execute the actual
            # reporter and temporary tests with this test interpreter.
            if command[:2] == ["fixture-uv", "python"]:
                command = [sys.executable, *command[2:]]
            return original(command, **kwargs)

        with (
            mock.patch.object(self.owner, "UV_RUN_SCRIPTS", ["fixture-uv"]),
            mock.patch.object(self.owner, "_run_json_worker", side_effect=execute),
            mock.patch.object(self.owner, "harbor_python", return_value=Path(sys.executable),
                              side_effect=ValueError("missing Harbor") if missing_harbor else None),
        ):
            code = self.owner.main(["test-python", "--json", *(
                arg for target in targets for arg in ("--module", target)
            )])
        return code, json.loads(self.output.getvalue()), commands

    def test_json_mode_aggregates_real_workers_and_preserves_failure_details(self):
        ordinary = self.write_test("test_ordinary", "self.fail('assertion λ detail')")
        harbor = self.write_test("test_harbor_windows_codex")
        code, report, commands = self.run_json([ordinary, harbor])
        self.assertEqual(code, 1)
        self.assertTrue(report["complete"])
        self.assertFalse(report["successful"])
        self.assertEqual(report["tests_run"], 2)
        self.assertFalse(report["reusable_validation_receipt"])
        self.assertEqual([item["environment"] for item in report["environments"]], ["scripts", "harbor"])
        self.assertEqual(commands[0][:2], ["fixture-uv", "python"])
        self.assertEqual(commands[1][0], sys.executable)
        failed = report["environments"][0]["report"]["tests"][0]
        self.assertEqual(failed["outcome"], "failure")
        self.assertIn("assertion λ detail", json.dumps(failed, ensure_ascii=False))
        self.assertIn("Traceback", json.dumps(failed))
        self.assertEqual(report["environments"][1]["report"]["tests"][0]["outcome"], "success")

    def test_json_mode_missing_harbor_preserves_ordinary_result(self):
        ordinary = self.write_test("test_ordinary")
        harbor = self.write_test("test_harbor_windows_codex")
        code, report, commands = self.run_json([ordinary, harbor], missing_harbor=True)
        self.assertEqual(code, 2)
        self.assertFalse(report["complete"])
        self.assertFalse(report["successful"])
        self.assertEqual(report["tests_run"], 1)
        self.assertEqual(len(commands), 1)
        self.assertIn("missing Harbor", report["errors"][0])

    def test_json_mode_empty_selection_and_broken_worker_fail_closed(self):
        with mock.patch.object(self.owner, "python_test_targets", return_value=[]):
            self.assertEqual(self.owner.main(["test-python", "--json"]), 2)
        report = json.loads(self.output.getvalue())
        self.assertEqual(report["tests_run"], 0)
        self.assertFalse(report["successful"])
        self.output.seek(0)
        self.output.truncate()
        with mock.patch.object(self.owner, "_run_json_worker", return_value=(0, {})):
            self.assertEqual(self.owner.main(["test-python", "--json", "--module", "missing"]), 2)
        self.assertFalse(json.loads(self.output.getvalue())["complete"])

    def oracle_audit(self, sources, *, strict=True):
        for path, text in sources.items():
            target = self.root / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text, encoding="utf-8", newline="")
        with (
            mock.patch.object(self.owner, "script_source_targets", return_value=list(sources)),
            mock.patch.object(self.owner, "script_kind_map", return_value=dict.fromkeys(sources, "python")),
            mock.patch.object(self.owner, "git_context_label", return_value="fixture"),
            mock.patch.object(self.owner, "script_audit_context_issues", return_value=[]),
            mock.patch.object(self.owner, "test_modules_for_changed_path", return_value=("fixture",)),
            mock.patch.object(self.owner, "script_audit_commands", return_value=([], [])),
        ):
            return self.owner.main(["audit-scripts", "--quick", "--oracles", *(["--strict"] if strict else [])])

    def test_oracles_use_source_only_and_remain_advisory_under_strict(self):
        marker = self.root / "must-not-execute"
        source = (f"from pathlib import Path\nPath({str(marker)!r}).write_text('bad')\n"
                  "import unittest\ndef report(): return {'executed': False}\n"
                  "class Case(unittest.TestCase):\n"
                  "    def test_literal(self): self.assertEqual(report()['executed'], False)\n")
        self.assertEqual(self.oracle_audit({"scripts/test_fixture.py": source}), 0)
        self.assertFalse(marker.exists())
        self.assertIn("[ORACLE ADVISORY]", self.output.getvalue())
        self.assertIn("analysis_complete=False", self.output.getvalue())
        self.assertIn("SCRIPT AUDIT PASSED", self.output.getvalue())

    def test_oracle_worker_isolated_from_source_import_shadowing(self):
        marker = self.root / "imported-source"
        shadow = (f"from pathlib import Path\nPath({str(marker)!r}).write_text('bad')\n"
                  "raise RuntimeError('analyzed source was imported')\n")
        source = ("import unittest\nVALUE = 7\nclass Case(unittest.TestCase):\n"
                  "    def test_literal(self): self.assertEqual(VALUE, 7)\n")
        with mock.patch.dict(os.environ, {"PYTHONPATH": str(self.root)}):
            self.assertEqual(self.oracle_audit({"ast.py": shadow, "test_fixture.py": source}), 0)
        self.assertFalse(marker.exists())
        self.assertIn("parse_complete=True", self.output.getvalue())
        self.assertIn("constant_only_oracle", self.output.getvalue())

    def test_oracle_incomplete_input_and_invalid_reports_are_unverified(self):
        with mock.patch.object(self.owner, "_run_json_worker") as worker:
            self.owner._print_oracle_advisories({}, {"missing.py"})
        worker.assert_not_called()
        self.assertIn("incomplete Python input snapshot", self.output.getvalue())
        source = "VALUE = 7\n"
        for change in ({"analysis_complete": True}, {"diagnostics": ["not a diagnostic"]}):
            with self.subTest(change=change):
                self.output.seek(0)
                self.output.truncate()
                report = {"schema_version": 1, "language": "python", "advisory_only": True,
                          "parse_complete": True, "analysis_complete": False,
                          "diagnostics": [], "unknown": [], "limitations": [], **change}
                with mock.patch.object(self.owner, "_run_json_worker", return_value=(0, report)):
                    self.assertEqual(self.oracle_audit({"fixture.py": source}), 0)
                self.assertIn("Analysis unverified", self.output.getvalue())

    def test_oracles_reject_changed_inputs_and_worker_failures_without_gating(self):
        source = "def test_literal():\n    assert 1 == 1\n"
        original = self.owner._run_json_worker

        def change_source(*args, **kwargs):
            result = original(*args, **kwargs)
            (self.root / "scripts/test_fixture.py").write_text(source + "# changed\n", encoding="utf-8")
            return result

        with mock.patch.object(self.owner, "_run_json_worker", side_effect=change_source):
            self.assertEqual(self.oracle_audit({"scripts/test_fixture.py": source}), 0)
        self.assertIn("source changed during oracle analysis", self.output.getvalue())
        self.output.seek(0)
        self.output.truncate()
        with mock.patch.object(self.owner, "_run_json_worker", side_effect=subprocess.TimeoutExpired("oracle", 30)):
            self.assertEqual(self.oracle_audit({"scripts/test_fixture.py": source}), 0)
        self.assertIn("Analysis unverified", self.output.getvalue())
        self.assertIn("SCRIPT AUDIT PASSED", self.output.getvalue())


class SyntaxSchedulingTest(unittest.TestCase):
    def setUp(self):
        self.owner = load_root_maintenance_module()
        self.output = self.enterContext(contextlib.redirect_stdout(io.StringIO()))

    def commands(self, count):
        return [(f"JavaScript syntax: {i}", (str(i),)) for i in range(count)]

    def test_bounded_overlap_preserves_output_order_and_serial_barriers(self):
        rendezvous = threading.Barrier(4, timeout=5)
        lock = threading.Lock()
        active = peak = 0
        completed = []
        serial = []

        def parser(command, **kwargs):
            nonlocal active, peak
            i = int(command[0])
            with lock:
                active += 1
                peak = max(peak, active)
            try:
                if i < 4:
                    rendezvous.wait()
                kwargs["stdout"].write(f"diagnostic-{i}\n".encode())
                return subprocess.CompletedProcess(command, i == 2)
            finally:
                with lock:
                    active -= 1
                    completed.append(i)

        def exclusive(command):
            self.assertEqual(active, 0)
            serial.append(command[0])
            if command[0] == "after":
                self.assertEqual(sorted(completed), list(range(7)))
            return 0

        commands = [("unknown", ("before",)), *self.commands(7),
                    ("script unit tests", ("after",))]
        with (mock.patch.object(self.owner, "run_owned", side_effect=parser),
              mock.patch.object(self.owner, "run", side_effect=exclusive)):
            results = list(self.owner._audit_command_results(commands))
        self.assertEqual(peak, 4)
        self.assertEqual(active, 0)
        self.assertEqual(serial, ["before", "after"])
        self.assertEqual([label for label, _ in results], [label for label, _ in commands])
        self.assertEqual([code for _, code in results], [0, 0, 0, 1, 0, 0, 0, 0, 0])
        diagnostics = [line for line in self.output.getvalue().splitlines()
                       if line.startswith("diagnostic-")]
        self.assertEqual(diagnostics, [f"diagnostic-{i}" for i in range(7)])

    def test_interrupt_cancels_and_joins_started_sibling(self):
        from scripts import process_owner

        started = threading.Event()
        stopped = threading.Event()

        def parser(command, **kwargs):
            if command[0] == "0":
                self.assertTrue(started.wait(5))
                raise KeyboardInterrupt()
            with process_owner.operation() as operation:
                started.set()
                try:
                    self.assertTrue(operation.cancelled.wait(5))
                    process_owner.check_operation()
                finally:
                    stopped.set()

        with (mock.patch.object(self.owner, "run_owned", side_effect=parser),
              self.assertRaises(KeyboardInterrupt)):
            list(self.owner._audit_command_results(self.commands(2)))
        self.assertTrue(stopped.is_set())

    def test_python_checks_share_one_worker_without_blocking_parsers(self):
        self.enterContext(mock.patch.object(self.owner, "which", return_value=None))
        rendezvous = threading.Barrier(4, timeout=5)
        format_finished = threading.Event()
        completed = []
        commands = [("Python format", ("format",)), ("Python lint", ("lint",)),
                    *self.commands(3), ("script unit tests", ("tests",))]

        def execute(command, **kwargs):
            name = command[0]
            if name == "lint":
                self.assertTrue(format_finished.is_set(), "uv checks overlapped")
            else:
                rendezvous.wait()
            kwargs["stdout"].write(f"diagnostic-{name}\n".encode())
            completed.append(name)
            if name == "format":
                format_finished.set()
            return subprocess.CompletedProcess(command, int(name == "format"))

        def tests(command):
            self.assertCountEqual(completed, ["format", "lint", "0", "1", "2"])
            return 0

        with (mock.patch.object(self.owner, "run_owned", side_effect=execute),
              mock.patch.object(self.owner, "run", side_effect=tests)):
            results = list(self.owner._audit_command_results(commands))
        self.assertEqual(results, [(label, int(label == "Python format")) for label, _ in commands])
        diagnostics = [line for line in self.output.getvalue().splitlines()
                       if line.startswith("diagnostic-")]
        self.assertEqual(diagnostics, [f"diagnostic-{name}" for name in ["format", "lint", "0", "1", "2"]])

    def test_parser_interrupt_cancels_python_chain_without_starting_lint(self):
        from scripts import process_owner

        self.enterContext(mock.patch.object(self.owner, "which", return_value=None))
        started = threading.Event()
        stopped = threading.Event()
        calls = []

        def execute(command, **kwargs):
            calls.append(command[0])
            kwargs["stdout"].write(f"started-{command[0]}\n".encode())
            if command[0] == "parser":
                self.assertTrue(started.wait(5))
                raise KeyboardInterrupt()
            self.assertEqual(command[0], "format")
            with process_owner.operation() as operation:
                started.set()
                try:
                    self.assertTrue(operation.cancelled.wait(5))
                    process_owner.check_operation()
                finally:
                    kwargs["stdout"].write(b"format-cleaned-up\n")
                    stopped.set()

        commands = [("Python format", ("format",)), ("Python lint", ("lint",)),
                    ("PowerShell syntax", ("parser",))]
        with (mock.patch.object(self.owner, "run_owned", side_effect=execute),
              self.assertRaises(KeyboardInterrupt)):
            list(self.owner._audit_command_results(commands))
        self.assertTrue(stopped.is_set())
        self.assertCountEqual(calls, ["format", "parser"])
        for marker in ("started-format", "started-parser", "format-cleaned-up"):
            self.assertEqual(self.output.getvalue().count(marker), 1)

    def test_closing_results_cancels_held_sibling_and_drains_its_log(self):
        from scripts import process_owner

        self.enterContext(mock.patch.object(self.owner, "which", return_value=None))
        started = threading.Event()
        stopped = threading.Event()

        def execute(command, **kwargs):
            if command[0] == "first":
                self.assertTrue(started.wait(5))
                kwargs["stdout"].write(b"first-result\n")
                return subprocess.CompletedProcess(command, 0)
            with process_owner.operation() as operation:
                kwargs["stdout"].write(b"held-started\n")
                started.set()
                try:
                    self.assertTrue(operation.cancelled.wait(5))
                    process_owner.check_operation()
                finally:
                    kwargs["stdout"].write(b"held-cleaned-up\n")
                    stopped.set()

        commands = [("Python format", ("first",)), ("PowerShell syntax", ("held",)),
                    ("script unit tests", ("not-started",))]
        with (mock.patch.object(self.owner, "run_owned", side_effect=execute),
              mock.patch.object(self.owner, "run") as serial):
            results = self.owner._audit_command_results(commands)
            self.assertEqual(next(results), ("Python format", 0))
            results.close()
        serial.assert_not_called()
        self.assertTrue(stopped.is_set())
        for marker in ("first-result", "held-started", "held-cleaned-up"):
            self.assertEqual(self.output.getvalue().count(marker), 1)

    def test_slow_output_delivery_does_not_hold_a_worker_or_python_cache_lane(self):
        self.enterContext(mock.patch.object(self.owner, "which", return_value=None))
        delivering = threading.Event()
        parser_finished = threading.Event()
        original_write = self.output.write

        def write(text):
            if text == "python-diagnostic\n":
                delivering.set()
                self.assertTrue(parser_finished.wait(5), "output delivery blocked the parser")
            return original_write(text)

        def execute(command, **kwargs):
            if command[0] == "python":
                kwargs["stdout"].write(b"python-diagnostic\n")
            else:
                self.assertTrue(delivering.wait(5))
                kwargs["stdout"].write(b"parser-diagnostic\n")
                parser_finished.set()
            return subprocess.CompletedProcess(command, 0)

        commands = [("Python format", ("python",)), ("PowerShell syntax", ("parser",))]
        with (mock.patch.object(self.output, "write", side_effect=write),
              mock.patch.object(self.owner, "run_owned", side_effect=execute)):
            self.assertEqual(list(self.owner._audit_command_results(commands)),
                             [(label, 0) for label, _ in commands])
        self.assertTrue(self.output.getvalue().endswith("python-diagnostic\nparser-diagnostic\n"))

    def test_missing_parser_preserves_sibling_and_unicode_diagnostics(self):
        body = "x" * 65535 + "λ😀\n"

        def parser(command, **kwargs):
            if command[0] == "0":
                raise FileNotFoundError("missing parser")
            kwargs["stdout"].write(body.encode())
            return subprocess.CompletedProcess(command, 0)

        with mock.patch.object(self.owner, "run_owned", side_effect=parser):
            results = list(self.owner._audit_command_results(self.commands(2)))
        self.assertEqual([code for _, code in results], [127, 0])
        self.assertIn("missing parser", self.output.getvalue())
        self.assertIn(body, self.output.getvalue())

    def test_unknown_commands_and_uv_checks_remain_serial(self):
        commands = [(label, (label,)) for label in
                    ("Python format", "Python lint", "unknown", "script unit tests")]
        with (mock.patch.object(self.owner, "run", side_effect=[0, OSError("no tool"), 0, 0]) as run,
              mock.patch.object(self.owner, "run_owned") as parallel):
            results = list(self.owner._audit_command_results(commands))
        self.assertEqual(run.call_count, 4)
        parallel.assert_not_called()
        self.assertEqual([code for _, code in results], [0, 127, 0, 0])

    def test_real_parser_exit_codes_and_audit_cli_failure_summary(self):
        commands = [(f"JavaScript syntax: {i}",
                     (sys.executable, "-c", f"print('child-{i}'); raise SystemExit({i})"))
                    for i in (0, 1)]
        with (
            mock.patch.object(self.owner, "script_source_targets", return_value=["fixture.js"]),
            mock.patch.object(self.owner, "script_kind_map", return_value={"fixture.js": "javascript"}),
            mock.patch.object(self.owner, "script_audit_context_issues", return_value=[]),
            mock.patch.object(self.owner, "script_audit_findings", return_value=([], [])),
            mock.patch.object(self.owner, "script_audit_commands", return_value=(commands, [])),
            mock.patch.object(self.owner, "git_context_label", return_value="fixture"),
        ):
            self.assertEqual(self.owner.main(["audit-scripts", "--quick"]), 1)
        output = self.output.getvalue()
        self.assertIn("child-0", output)
        self.assertIn("child-1", output)
        self.assertIn("[PASS] JavaScript syntax: 0", output)
        self.assertIn("[FAIL] JavaScript syntax: 1: exit 1", output)
        self.assertIn("1 command failure(s)", output)


if __name__ == "__main__":
    unittest.main()
