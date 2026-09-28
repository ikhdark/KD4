#!/usr/bin/env python3

import contextlib
import io
import os
import subprocess
import tempfile
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

    def test_mixed_changed_scripts_report_uncovered_path(self):
        maintenance = load_root_maintenance_module()
        with (
            self.temporary_script_tree(maintenance),
            mock.patch.object(maintenance, "run") as run,
        ):
            self.assertEqual(
                maintenance.main(
                    [
                        "test-python",
                        "--changed",
                        "scripts/readme_toc.py",
                        "--changed",
                        "scripts/unmapped_helper.py",
                    ]
                ),
                2,
            )
            run.assert_not_called()
        with mock.patch.object(
            maintenance,
            "script_inventory",
            side_effect=AssertionError("broad discovery"),
        ):
            self.assertIn(
                "scripts.test_readme_toc",
                maintenance.test_modules_for_changed_path("scripts/readme_toc.py"),
            )

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
                "scripts.test_kd4_perf_snapshot",
                "scripts.test_report_script_regressions.Report26ValidationRegressions.test_lock_waiter_never_writes_to_an_empty_owned_file",
                "scripts.test_report_script_regressions.Report26ValidationRegressions.test_lock_waits_for_release_and_distinguishes_timeout_from_io_error",
            ],
        )

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
            "DO-NOT-CHANGE/responses-api-proxy/npm/bin/codex-responses-api-proxy.js": "javascript",
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
            stdout="scripts/ trailing .py\0",
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

    def test_changed_production_script_without_tests_is_unverified(self) -> None:
        root_maintenance = load_root_maintenance_module()

        with (
            self.temporary_script_tree(root_maintenance) as scripts,
            mock.patch.object(root_maintenance, "run") as run,
        ):
            self.assertEqual(
                root_maintenance.main(
                    [
                        "test-python",
                        "--changed",
                        "scripts/unmapped_helper.py",
                    ]
                ),
                2,
            )
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


if __name__ == "__main__":
    unittest.main()
