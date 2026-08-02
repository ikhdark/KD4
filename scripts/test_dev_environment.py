#!/usr/bin/env python3

import contextlib
import io
from pathlib import Path, PurePosixPath
import os
import subprocess
import tempfile
import unittest
from unittest import mock

from scripts import app_server_schema_runtime_check
from scripts import config_schema_check
from scripts import dev_env_doctor
from scripts import generated_output_lock
from scripts import git_doctor
from scripts import vscode_runtime_proof


class DevEnvironmentDoctorTest(unittest.TestCase):
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
    def test_path_kind_detects_wsl_windows_mount(self) -> None:
        with (
            mock.patch.object(git_doctor.os, "name", "posix"),
            mock.patch.dict(git_doctor.os.environ, {}, clear=True),
            mock.patch.object(
                git_doctor.platform,
                "uname",
                return_value=mock.Mock(release="5.15.90.1-microsoft-standard-WSL2"),
            ),
        ):
            self.assertEqual(
                git_doctor.path_kind(PurePosixPath("/mnt/c/Users/kuh/repo")),
                "wsl-windows-mount",
            )

    def test_path_kind_does_not_treat_plain_linux_mnt_as_wsl(self) -> None:
        with (
            mock.patch.object(git_doctor.os, "name", "posix"),
            mock.patch.dict(git_doctor.os.environ, {}, clear=True),
            mock.patch.object(
                git_doctor.platform,
                "uname",
                return_value=mock.Mock(release="6.8.0-generic"),
            ),
            mock.patch.object(git_doctor.platform, "system", return_value="Linux"),
        ):
            self.assertEqual(
                git_doctor.path_kind(PurePosixPath("/mnt/data/repo")), "linux"
            )

    def test_recommendations_include_git_tuning_when_unset(self) -> None:
        recs = "\n".join(git_doctor.recommendations("windows", None, None))
        self.assertIn("core.fsmonitor", recs)
        self.assertIn("core.untrackedCache", recs)

    def test_recommendations_accept_fsmonitor_hook_path(self) -> None:
        recs = "\n".join(
            git_doctor.recommendations(
                "linux", "/usr/libexec/git-core/query-watchman", "true"
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
    def test_desktop_target_uses_publish_dir_env(self) -> None:
        with mock.patch.dict(
            vscode_runtime_proof.os.environ,
            {"CODEX_LOCAL_PUBLISH_DIR": "C:/tmp/local"},
            clear=False,
        ):
            binary = "codex.exe" if os.name == "nt" else "codex"
            self.assertEqual(
                vscode_runtime_proof.desktop_target().replace("\\", "/"),
                f"C:/tmp/local/{binary}",
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


class ConfigSchemaCheckTest(unittest.TestCase):
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

    def test_config_schema_auto_is_check_only_after_failure(self) -> None:
        with (
            mock.patch.object(
                config_schema_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                config_schema_check, "schema_inputs_changed", return_value=False
            ),
            mock.patch.object(
                config_schema_check, "run_protocol_check", return_value=1
            ) as run_check,
            mock.patch.object(
                config_schema_check, "regenerate_schema", return_value=True
            ) as regenerate,
            mock.patch.object(
                config_schema_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(config_schema_check.main(["--mode", "auto"]), 1)

        regenerate.assert_not_called()
        run_check.assert_called_once_with(Path("/repo"))

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
            with generated_output_lock.generated_output_lock(
                root, "assignment:owner-a"
            ) as lock_path:
                self.assertTrue(lock_path.is_file())
                with self.assertRaises(generated_output_lock.GenerationLockError):
                    with generated_output_lock.generated_output_lock(
                        root, "assignment:owner-b"
                    ):
                        self.fail("a live generation owner cannot be stolen")
            self.assertIn("assignment:owner-a", lock_path.read_text("utf-8"))

            with generated_output_lock.generated_output_lock(
                root, "assignment:owner-b"
            ):
                pass


class AppServerSchemaRuntimeCheckTest(unittest.TestCase):
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

    def test_auto_is_check_only_after_failure(self) -> None:
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
                app_server_schema_runtime_check,
                "run_protocol_check",
                return_value=1,
            ) as run_check,
            mock.patch.object(
                app_server_schema_runtime_check,
                "regenerate_schemas",
                return_value=True,
            ) as regenerate,
            mock.patch.object(
                app_server_schema_runtime_check,
                "generated_output_lock",
                return_value=contextlib.nullcontext(),
            ),
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(["--mode", "auto"]), 1
            )

        regenerate.assert_not_called()
        run_check.assert_called_once_with(Path("/repo"))


if __name__ == "__main__":
    unittest.main()
