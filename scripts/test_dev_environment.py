#!/usr/bin/env python3

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path, PureWindowsPath
from unittest import mock

from scripts import (
    app_server_schema_runtime_check,
    config_schema_check,
    dev_env_doctor,
    generated_output_lock,
    git_doctor,
    tool_versions,
    vscode_runtime_proof,
)


class DevEnvironmentDoctorTest(unittest.TestCase):
    def test_pnpm_pin_distinguishes_prerelease(self):
        for actual, expected, ok in (
            ("10.1.0-rc.1", "10.1.0", False),
            ("10.1.0-rc.1", "10.1.0-rc.1", True),
        ):
            with (
                mock.patch.object(dev_env_doctor.shutil, "which", return_value="pnpm"),
                mock.patch.object(dev_env_doctor, "run_version", return_value=actual),
            ):
                self.assertEqual(
                    dev_env_doctor.check_tool(
                        "pnpm",
                        ["pnpm", "--version"],
                        required=True,
                        guidance="pin",
                        required_version=expected,
                    ).ok,
                    ok,
                )

    def test_tool_probes_overlap_and_keep_declared_order(self):
        import threading

        barrier = threading.Barrier(4, timeout=5)
        first = {"python", "git", "cargo", "rustfmt"}

        def check(name, command, **kwargs):
            if name in first:
                barrier.wait()
            return name

        with mock.patch.object(dev_env_doctor, "check_tool", side_effect=check):
            checks = dev_env_doctor.collect_checks()
        self.assertEqual(
            checks,
            [
                "python",
                "git",
                "cargo",
                "rustfmt",
                "clippy",
                "just",
                "cargo-nextest",
                "uv",
                "node",
                "pnpm",
                "pwsh",
            ],
        )

    def test_collect_checks_covers_required_workflow_tools(self) -> None:
        with (
            mock.patch.object(
                dev_env_doctor, "package_manager_pin", return_value="pnpm@10.0.0"
            ),
            mock.patch.object(
                dev_env_doctor,
                "check_tool",
                side_effect=lambda name, command, **kwargs: name,
            ),
        ):
            checks = dev_env_doctor.collect_checks()

        self.assertIn("uv", checks)
        self.assertIn("rustfmt", checks)
        self.assertIn("clippy", checks)
        self.assertIn("pwsh", checks)

    def test_node_major_parses_version_prefix(self) -> None:
        self.assertEqual(dev_env_doctor.node_major("v22.13.1"), 22)
        self.assertEqual(dev_env_doctor.node_major("node 23.0.0"), 23)
        self.assertIsNone(dev_env_doctor.node_major("not a version"))

    def test_package_manager_pin_strips_integrity_suffix(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_json = Path(temp_dir) / "package.json"
            with mock.patch.object(dev_env_doctor, "PACKAGE_JSON", package_json):
                package_json.write_text(
                    '{"packageManager":"pnpm@1.2.3+sha512.deadbeef"}',
                    encoding="utf-8",
                )
                self.assertEqual(dev_env_doctor.package_manager_pin(), "pnpm@1.2.3")

                package_json.write_text(
                    '{"packageManager":"pnpm@4.5.6"}',
                    encoding="utf-8",
                )
                self.assertEqual(dev_env_doctor.package_manager_pin(), "pnpm@4.5.6")

    def test_malformed_package_json_reports_clean_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_json = Path(temp_dir) / "package.json"
            package_json.write_text('{"packageManager":', encoding="utf-8")
            stderr = io.StringIO()
            with (
                mock.patch.object(dev_env_doctor, "PACKAGE_JSON", package_json),
                contextlib.redirect_stderr(stderr),
            ):
                code = dev_env_doctor.main([])

        self.assertEqual(code, 1)
        self.assertIn("Development environment check failed:", stderr.getvalue())
        self.assertIn(str(package_json), stderr.getvalue())
        self.assertNotIn("Traceback", stderr.getvalue())

    def test_run_version_prefers_stdout_over_stderr_warning(self) -> None:
        completed = subprocess.CompletedProcess(
            ["pnpm"], 0, stdout="10.34.0\n", stderr="Corepack download warning\n"
        )
        with mock.patch.object(
            dev_env_doctor.subprocess, "run", return_value=completed
        ) as run:
            self.assertEqual(
                dev_env_doctor.run_version(["pnpm", "--version"]), "10.34.0"
            )

        self.assertEqual(run.call_args.kwargs["stderr"], subprocess.PIPE)

    def test_run_version_uses_stderr_when_stdout_is_empty(self) -> None:
        completed = subprocess.CompletedProcess(
            ["python"], 0, stdout="", stderr="Python 3.11.9\n"
        )
        with mock.patch.object(
            dev_env_doctor.subprocess, "run", return_value=completed
        ):
            self.assertEqual(
                dev_env_doctor.run_version(["python", "--version"]), "Python 3.11.9"
            )

    def test_version_checks_enforce_python_floor_and_exact_pnpm_pin(self) -> None:
        self.assertEqual(dev_env_doctor.numeric_version("Python 3.11.9"), (3, 11, 9))
        self.assertEqual(
            dev_env_doctor.package_manager_version("pnpm@10.12.4"), "10.12.4"
        )

        with (
            mock.patch.object(dev_env_doctor.shutil, "which", return_value="/tool"),
            mock.patch.object(
                dev_env_doctor, "run_version", return_value="Python 3.10.9"
            ),
        ):
            check = dev_env_doctor.check_tool(
                "python",
                ["python", "--version"],
                required=True,
                guidance="upgrade",
                min_version=(3, 11),
            )
        self.assertFalse(check.ok)

        with (
            mock.patch.object(dev_env_doctor.shutil, "which", return_value="/tool"),
            mock.patch.object(dev_env_doctor, "run_version", return_value="10.12.3"),
        ):
            check = dev_env_doctor.check_tool(
                "pnpm",
                ["pnpm", "--version"],
                required=True,
                guidance="pin",
                required_version="10.12.4",
            )
        self.assertFalse(check.ok)
        stdout = io.StringIO()
        with contextlib.redirect_stdout(stdout):
            dev_env_doctor.print_text([check])
        self.assertIn("- pnpm: mismatch (10.12.3)", stdout.getvalue())


class GitDoctorTest(unittest.TestCase):
    def test_path_kind_is_windows(self) -> None:
        self.assertEqual(
            git_doctor.path_kind(PureWindowsPath(r"C:\Users\kuh\repo")),
            "windows",
        )

    def test_recommendations_include_git_tuning_when_unset(self) -> None:
        recs = "\n".join(git_doctor.recommendations("windows", None, None))
        self.assertIn("core.fsmonitor", recs)
        self.assertIn("core.untrackedCache", recs)

    def test_recommendations_accept_fsmonitor_hook_path(self) -> None:
        recs = "\n".join(
            git_doctor.recommendations(
                "windows", r"C:\Program Files\Git\query-watchman.exe", "true"
            )
        )
        self.assertNotIn("core.fsmonitor", recs)

    def test_unreadable_pytest_cache_recommendation_is_local_state(self) -> None:
        recs = "\n".join(
            git_doctor.recommendations(
                "windows",
                "true",
                "true",
                (".pytest_cache/", "sdk/python/.pytest_cache/"),
            )
        )
        self.assertIn("delete the cache directories", recs)
        self.assertIn("not source dirt", recs)

    def test_unreadable_pytest_cache_dirs_are_scoped_to_known_caches(self) -> None:
        def fake_readable(path: Path) -> bool:
            return path.as_posix() != "/repo/sdk/python/.pytest_cache"

        with mock.patch.object(
            git_doctor, "directory_is_readable", side_effect=fake_readable
        ):
            self.assertEqual(
                git_doctor.unreadable_pytest_cache_dirs(Path("/repo")),
                ("sdk/python/.pytest_cache/",),
            )

    def test_run_git_decodes_output_as_utf8(self) -> None:
        completed = subprocess.CompletedProcess(
            ["git"], 0, stdout="C:/Users/Jos\u00e9/repo\n", stderr=""
        )
        with mock.patch.object(
            git_doctor.subprocess, "run", return_value=completed
        ) as run:
            self.assertEqual(
                git_doctor.run_git(["rev-parse", "--show-toplevel"]).stdout,
                "C:/Users/Jos\u00e9/repo\n",
            )

        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")


class VscodeRuntimeProofTest(unittest.TestCase):
    def test_default_target_matches_publisher_bin_directory(self) -> None:
        with mock.patch.dict(os.environ, {"CODEX_LOCAL_PUBLISH_DIR": ""}):
            self.assertEqual(
                Path(vscode_runtime_proof.desktop_target()),
                Path.home() / "Desktop" / "LOCAL-KD" / "bin" / "codex.exe",
            )

    def test_expected_binary_checks_only_path(self) -> None:
        target = str(Path("codex.exe").resolve())
        for actual, expected_rc in ((target, 0), (None, 1)):
            with (
                self.subTest(actual=actual),
                mock.patch.object(
                    vscode_runtime_proof.shutil, "which", return_value=actual
                ),
                mock.patch.object(vscode_runtime_proof, "desktop_target") as desktop,
                mock.patch.object(
                    vscode_runtime_proof, "extension_candidates"
                ) as extensions,
                mock.patch.object(
                    vscode_runtime_proof, "run_version", return_value="codex 1"
                ) as version,
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(
                    vscode_runtime_proof.main(["--expected-binary", target]),
                    expected_rc,
                )
                desktop.assert_not_called()
                extensions.assert_not_called()
                self.assertEqual(version.call_count, int(actual is not None))

    def test_full_inventory_reuses_versions_and_honors_no_run(self) -> None:
        target = str(Path("codex.exe").resolve())
        for no_run in (False, True):
            stdout = io.StringIO()
            with (
                self.subTest(no_run=no_run),
                mock.patch.object(
                    vscode_runtime_proof.shutil, "which", return_value=target
                ),
                mock.patch.object(
                    vscode_runtime_proof, "desktop_target", return_value=target
                ),
                mock.patch.object(
                    vscode_runtime_proof, "extension_candidates", return_value=[target]
                ),
                mock.patch.object(
                    vscode_runtime_proof, "run_version", return_value="codex 1"
                ) as version,
                contextlib.redirect_stdout(stdout),
            ):
                self.assertEqual(
                    vscode_runtime_proof.main(["--no-run-codex"] if no_run else []), 0
                )
                self.assertEqual(version.call_count, 0 if no_run else 1)
                self.assertEqual(
                    stdout.getvalue().count("version=codex 1"), 0 if no_run else 3
                )
                self.assertIn("inventory, not proof", stdout.getvalue())

    def test_desktop_target_uses_publish_dir_env(self) -> None:
        with mock.patch.dict(
            vscode_runtime_proof.os.environ,
            {"CODEX_LOCAL_PUBLISH_DIR": "C:/tmp/local"},
            clear=False,
        ):
            self.assertEqual(
                vscode_runtime_proof.desktop_target().replace("\\", "/"),
                "C:/tmp/local/codex.exe",
            )

    def test_extension_candidates_are_sorted_and_bounded(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir)
            extension_root = home / ".vscode" / "extensions" / "openai.codex"
            extension_root.mkdir(parents=True)
            (extension_root / "codex.exe").write_bytes(b"")
            nested = extension_root / "bin"
            nested.mkdir()
            (nested / "codex").write_bytes(b"")

            with mock.patch.object(
                vscode_runtime_proof.Path, "home", return_value=home
            ):
                matches = vscode_runtime_proof.extension_candidates(limit=1)

        self.assertEqual(len(matches), 1)
        self.assertTrue(matches[0].endswith("codex.exe"))


class ToolVersionsTest(unittest.TestCase):
    def test_ruff_requirement_matches_exact_dependency_name(self) -> None:
        for requirement in ("ruff>=0.15.8", "Ruff == 0.15.8", "ruff[extra]>=0.15.8"):
            with (
                self.subTest(requirement=requirement),
                mock.patch.object(tool_versions.Path, "read_text", return_value=""),
                mock.patch.object(
                    tool_versions.tomllib,
                    "loads",
                    return_value={
                        "project": {
                            "dependencies": ["ruff-lsp>=1", "ruffus>=1", requirement]
                        }
                    },
                ),
            ):
                self.assertEqual(
                    tool_versions.scripts_ruff_requirement.__wrapped__(), requirement
                )
        with (
            mock.patch.object(
                tool_versions.Path,
                "read_text",
                return_value='[project]\ndependencies = ["ruff-lsp>=1"]',
            ),
            self.assertRaisesRegex(RuntimeError, "must declare a ruff dependency"),
        ):
            tool_versions.scripts_ruff_requirement.__wrapped__()


class ConfigSchemaCheckTest(unittest.TestCase):
    def test_logged_command_quotes_arguments_with_spaces(self) -> None:
        stdout = io.StringIO()
        with (
            mock.patch.object(
                config_schema_check.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 0),
            ),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(
                config_schema_check.run(
                    ["cargo", "--manifest-path", "C:/repo with spaces/Cargo.toml"],
                    cwd=Path("/repo"),
                ),
                0,
            )

        self.assertIn("'C:/repo with spaces/Cargo.toml'", stdout.getvalue())

    def test_changed_outputs_detects_added_removed_and_modified_paths(self) -> None:
        before = {"a": "1", "b": "2"}
        after = {"b": "3", "c": "4"}
        self.assertEqual(
            config_schema_check.changed_outputs(before, after), ["a", "b", "c"]
        )

    def test_config_schema_inputs_cover_schema_crate_dependencies(self) -> None:
        self.assertIn("codex-rs/features/src", config_schema_check.SCHEMA_INPUTS)
        self.assertIn("codex-rs/protocol/src", config_schema_check.SCHEMA_INPUTS)
        self.assertIn("codex-rs/config/Cargo.toml", config_schema_check.SCHEMA_INPUTS)

    def test_config_schema_status_uses_utf8_and_expanded_inputs(self) -> None:
        completed = subprocess.CompletedProcess(["git"], 0, stdout="", stderr="")
        with mock.patch.object(
            config_schema_check.subprocess, "run", return_value=completed
        ) as run:
            self.assertFalse(config_schema_check.schema_inputs_changed(Path("/repo")))

        args = run.call_args.args[0]
        self.assertIn("codex-rs/features/src", args)
        self.assertIn("codex-rs/protocol/src", args)
        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")

    def test_missing_config_schema_commands_report_clean_diagnostics(self) -> None:
        for command in ("cargo", "just"):
            with self.subTest(command=command):
                stderr = io.StringIO()
                with (
                    mock.patch.object(
                        config_schema_check.subprocess,
                        "run",
                        side_effect=FileNotFoundError(
                            2, "No such file or directory", command
                        ),
                    ),
                    contextlib.redirect_stderr(stderr),
                ):
                    code = config_schema_check.run([command], cwd=Path("/repo"))

                self.assertEqual(code, 127)
                self.assertIn(f"Could not run {command}:", stderr.getvalue())
                self.assertNotIn("Traceback", stderr.getvalue())

    def test_missing_git_during_schema_status_marks_inputs_changed_cleanly(
        self,
    ) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                config_schema_check.subprocess,
                "run",
                side_effect=FileNotFoundError(2, "No such file or directory", "git"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertTrue(config_schema_check.schema_inputs_changed(Path("/repo")))

        self.assertIn(
            "Could not compare config schema inputs with HEAD:",
            stderr.getvalue(),
        )
        self.assertNotIn("Traceback", stderr.getvalue())

    def test_config_schema_rejects_removed_auto_mode(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            config_schema_check.main(["--mode", "auto"])

        self.assertEqual(raised.exception.code, 2)

    def test_config_schema_force_routes_the_generation_owner(self) -> None:
        with (
            mock.patch.object(
                config_schema_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                config_schema_check, "regenerate_schema", return_value=True
            ) as regenerate,
            mock.patch.object(
                config_schema_check, "run_protocol_check", return_value=0
            ),
            mock.patch.object(
                config_schema_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(
                config_schema_check.main(
                    ["--mode", "force", "--owner", "assignment:config-owner"]
                ),
                0,
            )

        regenerate.assert_called_once_with(Path("/repo"), "assignment:config-owner")


class GeneratedOutputLockTest(unittest.TestCase):
    def test_lock_is_process_scoped_and_recovers_after_release(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            ready = root / "ready"
            script = (
                "import sys; from pathlib import Path; "
                "from scripts.generated_output_lock import generated_output_lock\n"
                "with generated_output_lock(Path(sys.argv[1]), 'assignment:owner-a') as lock:\n"
                "    Path(sys.argv[2]).write_text(str(lock), encoding='utf-8')\n"
                "    sys.stdin.read(1)\n"
            )
            child = subprocess.Popen(
                [sys.executable, "-c", script, str(root), str(ready)],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                cwd=Path(__file__).resolve().parents[1],
            )
            try:
                deadline = time.monotonic() + 10
                while (
                    not ready.exists()
                    and child.poll() is None
                    and time.monotonic() < deadline
                ):
                    time.sleep(0.02)
                self.assertTrue(
                    ready.exists(), "child did not acquire the generation lock"
                )
                with self.assertRaises(generated_output_lock.GenerationLockError):
                    with generated_output_lock.generated_output_lock(
                        root, "assignment:owner-b"
                    ):
                        self.fail("a live generation owner cannot be stolen")
                _, errors = child.communicate(b"x", timeout=10)
                self.assertEqual(child.returncode, 0, errors)
            finally:
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=10)

            with generated_output_lock.generated_output_lock(
                root, "assignment:owner-b"
            ) as lock_path:
                self.assertTrue(lock_path.is_file())
            self.assertIn("assignment:owner-b", lock_path.read_text("utf-8"))


class AppServerSchemaRuntimeCheckTest(unittest.TestCase):
    def test_new_constraint_maps_remain_breaking_changes(self):
        compare = app_server_schema_runtime_check.stable_schema_compatibility_issues
        for keyword in ("patternProperties", "dependentSchemas"):
            self.assertEqual(
                compare({keyword: {}}, {keyword: {"field": {"type": "string"}}}),
                [f"$/{keyword}/field:added"],
            )

    def test_annotation_names_are_real_properties_and_enum_values(self):
        compare = app_server_schema_runtime_check.stable_schema_compatibility_issues
        for name in ("title", "description", "default", "examples"):
            baseline = {"properties": {name: {"type": "string"}}}
            self.assertEqual(
                compare(baseline, {"properties": {}}), [f"$/properties/{name}:removed"]
            )
            self.assertEqual(
                compare(baseline, {"properties": {name: {"type": "integer"}}}),
                [f"$/properties/{name}/type:changed"],
            )
        self.assertEqual(
            compare({"enum": [{"title": "a"}]}, {"enum": [{"title": "b"}]}),
            ["$/enum:changed"],
        )

    def test_combinator_annotations_are_ignored_but_constraints_are_checked(self):
        compare = app_server_schema_runtime_check.stable_schema_compatibility_issues
        for keyword in ("oneOf", "anyOf", "allOf", "prefixItems"):
            baseline = {keyword: [{"type": "string", "description": "before"}]}
            self.assertEqual(
                compare(
                    baseline, {keyword: [{"type": "string", "description": "after"}]}
                ),
                [],
            )
            self.assertEqual(
                compare(
                    baseline, {keyword: [{"type": "integer", "description": "before"}]}
                ),
                [f"$/{keyword}:changed"],
            )

    def test_schema_inputs_cover_core_protocol_dependency(self) -> None:
        self.assertIn(
            "codex-rs/protocol/src",
            app_server_schema_runtime_check.SCHEMA_INPUTS,
        )

    def test_schema_status_uses_utf8_and_expanded_inputs(self) -> None:
        completed = subprocess.CompletedProcess(["git"], 0, stdout="", stderr="")
        with mock.patch.object(
            app_server_schema_runtime_check.subprocess,
            "run",
            return_value=completed,
        ) as run:
            self.assertFalse(
                app_server_schema_runtime_check.schema_inputs_changed(Path("/repo"))
            )

        args = run.call_args.args[0]
        self.assertIn("codex-rs/protocol/src", args)
        self.assertEqual(run.call_args.kwargs["encoding"], "utf-8")

    def test_missing_schema_status_binary_falls_back_without_traceback(self) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                app_server_schema_runtime_check.subprocess,
                "run",
                side_effect=FileNotFoundError("git missing"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertTrue(
                app_server_schema_runtime_check.schema_inputs_changed(Path("/repo"))
            )

        self.assertIn(
            "Could not compare app-server schema inputs with HEAD:",
            stderr.getvalue(),
        )

    def test_missing_command_returns_clean_diagnostic(self) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                app_server_schema_runtime_check.subprocess,
                "run",
                side_effect=FileNotFoundError("cargo missing"),
            ),
            contextlib.redirect_stderr(stderr),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.run(["cargo"], cwd=Path("/repo")),
                127,
            )

        self.assertIn("Could not run cargo", stderr.getvalue())

    def test_logged_command_quotes_arguments_with_spaces(self) -> None:
        stdout = io.StringIO()
        completed = subprocess.CompletedProcess(["tool"], 0)
        with (
            mock.patch.object(
                app_server_schema_runtime_check.subprocess,
                "run",
                return_value=completed,
            ),
            contextlib.redirect_stdout(stdout),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.run(
                    ["tool", "path with spaces"], cwd=Path("/repo")
                ),
                0,
            )

        self.assertIn("'path with spaces'", stdout.getvalue())

    def test_force_regeneration_succeeds_when_outputs_change(self) -> None:
        with (
            mock.patch.object(
                app_server_schema_runtime_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "regenerate_schemas",
                return_value=True,
            ) as regenerate,
            mock.patch.object(
                app_server_schema_runtime_check, "run_protocol_check", return_value=0
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_stable_compatibility_check",
                return_value=0,
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_python_sdk_contract_check",
                return_value=0,
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(
                    ["--mode", "force", "--owner", "assignment:app-server-owner"]
                ),
                0,
            )
        regenerate.assert_called_once_with(Path("/repo"), "assignment:app-server-owner")

    def test_force_regeneration_forwards_generator_arguments(self) -> None:
        with (
            mock.patch.object(
                app_server_schema_runtime_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "regenerate_schemas",
                return_value=False,
            ) as regenerate,
            mock.patch.object(
                app_server_schema_runtime_check, "run_protocol_check", return_value=0
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_python_sdk_contract_check",
                return_value=0,
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(
                    [
                        "--mode",
                        "force",
                        "--owner",
                        "assignment:app-server-owner",
                        "--",
                        "--experimental",
                    ]
                ),
                0,
            )
        regenerate.assert_called_once_with(
            Path("/repo"),
            "assignment:app-server-owner",
            ["--experimental"],
        )

    def test_stable_schema_compatibility_rejects_breaks_and_allows_additions(
        self,
    ) -> None:
        baseline = {
            "definitions": {
                "Request": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"],
                }
            }
        }
        additive = {
            "definitions": {
                **baseline["definitions"],
                "Response": {"type": "object"},
            }
        }
        additive["definitions"]["Request"] = {
            **baseline["definitions"]["Request"],
            "properties": {
                **baseline["definitions"]["Request"]["properties"],
                "tag": {"type": "string"},
            },
        }
        breaking = {
            "definitions": {
                "Request": {
                    "type": "object",
                    "properties": {},
                    "required": ["name", "tag"],
                }
            }
        }

        self.assertEqual(
            app_server_schema_runtime_check.stable_schema_compatibility_issues(
                baseline, additive
            ),
            [],
        )
        issues = app_server_schema_runtime_check.stable_schema_compatibility_issues(
            baseline, breaking
        )
        self.assertIn("$/definitions/Request/properties/name:removed", issues)
        self.assertIn("$/definitions/Request/required:changed", issues)

    def test_stable_schema_compatibility_allows_namespaced_definitions(self) -> None:
        baseline = {"definitions": {"v2": {"Existing": {"type": "object"}}}}
        additive = {
            "definitions": {
                "v2": {
                    **baseline["definitions"]["v2"],
                    "Added": {"type": "object"},
                }
            }
        }

        self.assertEqual(
            app_server_schema_runtime_check.stable_schema_compatibility_issues(
                baseline, additive
            ),
            [],
        )

    def test_schema_gate_composes_protocol_compatibility_and_python_consumer(
        self,
    ) -> None:
        calls: list[str] = []

        @contextlib.contextmanager
        def lock():
            calls.append("lock")
            try:
                yield
            finally:
                calls.append("unlock")

        with (
            mock.patch.object(
                app_server_schema_runtime_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "schema_inputs_changed",
                return_value=False,
            ) as input_probe,
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_protocol_check",
                side_effect=lambda _root: calls.append("protocol") or 0,
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_stable_compatibility_check",
                side_effect=lambda _root, baseline, _allowed: (
                    calls.append(f"compatibility:{baseline}") or 0
                ),
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_python_sdk_contract_check",
                side_effect=lambda _root: calls.append("python-sdk") or 0,
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "generated_output_lock",
                return_value=lock(),
            ),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(["--mode", "check"]),
                0,
            )

        input_probe.assert_not_called()
        self.assertEqual(
            calls, ["lock", "protocol", "compatibility:HEAD^", "python-sdk", "unlock"]
        )

    def test_schema_gate_stops_before_consumer_on_compatibility_failure(self) -> None:
        with (
            mock.patch.object(
                app_server_schema_runtime_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "schema_inputs_changed",
                return_value=False,
            ),
            mock.patch.object(
                app_server_schema_runtime_check, "run_protocol_check", return_value=0
            ),
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_stable_compatibility_check",
                return_value=1,
            ),
            mock.patch.object(
                app_server_schema_runtime_check, "run_python_sdk_contract_check"
            ) as consumer,
            mock.patch.object(
                app_server_schema_runtime_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(["--mode", "check"]),
                1,
            )

        consumer.assert_not_called()

    def test_app_server_schema_rejects_removed_auto_mode(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            app_server_schema_runtime_check.main(["--mode", "auto"])

        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
