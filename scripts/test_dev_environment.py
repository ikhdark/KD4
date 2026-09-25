#!/usr/bin/env python3

import contextlib
import io
import json
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
    def test_git_version_is_reported_as_an_available_prerequisite(self):
        with (
            mock.patch.object(dev_env_doctor.shutil, "which", return_value="git.exe"),
            mock.patch.object(
                dev_env_doctor.subprocess,
                "run",
                return_value=subprocess.CompletedProcess(
                    [], 0, "git version 2.53.0.windows.1\n", ""
                ),
            ),
        ):
            check = dev_env_doctor.check_tool(
                "git", ["git", "--version"], required=True, guidance="Install Git"
            )
        self.assertTrue(check.ok)
        self.assertEqual(check.version, "git version 2.53.0.windows.1")

    def test_cli_requires_ripgrep_and_reports_its_version(self):
        for available in (False, True):
            with (
                self.subTest(available=available),
                mock.patch.object(
                    dev_env_doctor, "package_manager_pin", return_value="pnpm@99.0.0"
                ),
                mock.patch.object(
                    dev_env_doctor.tool_versions,
                    "rust_toolchain_channel",
                    return_value="99.0.0",
                ),
                mock.patch.object(
                    dev_env_doctor.shutil,
                    "which",
                    side_effect=lambda name: (
                        None if name == "rg" and not available else name
                    ),
                ),
                mock.patch.object(
                    dev_env_doctor.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, "99.0.0\n", ""),
                ),
                contextlib.redirect_stdout(io.StringIO()) as stdout,
            ):
                result = dev_env_doctor.main(["--json"])
            report = json.loads(stdout.getvalue())
            self.assertEqual(result, 0 if available else 1)
            self.assertEqual(report["ok"], available)
            rg = next(check for check in report["checks"] if check["name"] == "rg")
            self.assertEqual(rg["command"], ["rg", "--version"])
            self.assertTrue(rg["required"])
            self.assertEqual(rg["ok"], available)
            self.assertEqual(rg["version"], "99.0.0" if available else None)

    def test_version_probe_skips_banners_in_either_stream(self) -> None:
        for stdout, stderr in (
            ("Warning: deprecated shim\n10.1.0-rc.1\n", ""),
            ("", "Warning: deprecated shim\n10.1.0-rc.1\n"),
            ("Warning: deprecated shim\n", "10.1.0-rc.1\n"),
        ):
            with (
                self.subTest(stdout=stdout, stderr=stderr),
                mock.patch.object(dev_env_doctor.shutil, "which", return_value="pnpm"),
                mock.patch.object(
                    dev_env_doctor.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], 0, stdout, stderr),
                ),
            ):
                check = dev_env_doctor.check_tool(
                    "pnpm",
                    ["pnpm", "--version"],
                    required=True,
                    guidance="pin",
                    required_version="10.1.0-rc.1",
                )
                self.assertTrue(check.ok)
                self.assertEqual(check.version, "10.1.0-rc.1")

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
                "rg",
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

    def test_node_floor_comes_from_package_json_engines(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_json = Path(temp_dir) / "package.json"
            with mock.patch.object(dev_env_doctor, "PACKAGE_JSON", package_json):
                package_json.write_text(
                    '{"engines":{"node":">=22.22.2"}}', encoding="utf-8"
                )
                floor = dev_env_doctor.node_engine_floor()
                package_json.write_text('{"engines":{"node":"^22"}}', encoding="utf-8")
                with self.assertRaisesRegex(
                    dev_env_doctor.PackageJsonError, "engines.node"
                ):
                    dev_env_doctor.node_engine_floor()

        self.assertEqual(floor, (22, 22, 2))
        for version, ok in (("v22.22.1", False), ("v22.22.2", True), ("v26.9.0", True)):
            with (
                self.subTest(version=version),
                mock.patch.object(dev_env_doctor.shutil, "which", return_value="node"),
                mock.patch.object(dev_env_doctor, "run_version", return_value=version),
            ):
                check = dev_env_doctor.check_tool(
                    "node",
                    ["node", "--version"],
                    required=True,
                    guidance="node",
                    min_version=floor,
                )
                self.assertEqual(check.ok, ok)

    def test_rust_probes_use_the_toolchains_workflows_run(self) -> None:
        with (
            mock.patch.object(
                dev_env_doctor.tool_versions,
                "rust_toolchain_channel",
                return_value="1.95.0",
            ),
            mock.patch.object(
                dev_env_doctor,
                "check_tool",
                side_effect=lambda name, command, **kwargs: (name, command, kwargs),
            ),
        ):
            checks = {
                name: (tuple(command), kwargs)
                for name, command, kwargs in dev_env_doctor.collect_checks()
            }

        for name in ("cargo", "rustfmt", "clippy", "cargo-nextest"):
            self.assertEqual(
                checks[name][1]["cwd"], dev_env_doctor.REPO_ROOT / "codex-rs", name
            )
        self.assertEqual(
            checks["rustfmt"][0],
            (
                "rustup",
                "run",
                tool_versions.RUSTFMT_TOOLCHAIN,
                "cargo",
                "fmt",
                "--version",
            ),
        )
        self.assertEqual(checks["cargo"][1]["pinned_version"], (1, 95, 0))
        for version, ok in (
            ("cargo 1.98.1 (797e8a9bc 2026-08-05)", False),
            ("cargo 1.95.0 (f2d3ce0bd 2026-03-21)", True),
        ):
            with (
                self.subTest(version=version),
                mock.patch.object(dev_env_doctor.shutil, "which", return_value="cargo"),
                mock.patch.object(dev_env_doctor, "run_version", return_value=version),
            ):
                check = dev_env_doctor.check_tool(
                    "cargo",
                    ["cargo", "--version"],
                    required=True,
                    guidance="pin",
                    pinned_version=(1, 95, 0),
                )
                self.assertEqual(check.ok, ok)

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
        self.assertEqual(run.call_args.kwargs["env"]["RUSTUP_AUTO_INSTALL"], "0")

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
        with mock.patch.dict(
            os.environ, {"CODEX_LOCAL_PUBLISH_DIR": "", "CODEX_CLI_PATH": ""}
        ):
            self.assertEqual(
                Path(vscode_runtime_proof.desktop_target()),
                Path.home() / "Desktop" / "LOCAL-KD" / "bin" / "codex.exe",
            )

    def test_desktop_target_is_the_cli_desktop_launches(self) -> None:
        # The publisher routes Desktop through CODEX_CLI_PATH, which can name a
        # non-default install; the Desktop row must report that binary.
        with mock.patch.dict(
            os.environ,
            {
                "CODEX_CLI_PATH": r"D:\kd\bin\codex.exe",
                "CODEX_LOCAL_PUBLISH_DIR": r"C:\tmp\local",
            },
        ):
            self.assertEqual(
                vscode_runtime_proof.desktop_target(), r"D:\kd\bin\codex.exe"
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
            {"CODEX_LOCAL_PUBLISH_DIR": "C:/tmp/local", "CODEX_CLI_PATH": ""},
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

    def test_extension_candidates_skip_vsix_staging_folders(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            home = Path(temp_dir)
            extensions = home / ".vscode" / "extensions"
            for folder in (".0a1b2c3d-staging", "openai.chatgpt-1.0.0-win32-x64"):
                binary = extensions / folder / "bin" / "windows-x86_64" / "codex.exe"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"")

            with mock.patch.object(
                vscode_runtime_proof.Path, "home", return_value=home
            ):
                matches = vscode_runtime_proof.extension_candidates(limit=1)

        self.assertEqual(
            [Path(match).relative_to(extensions).parts[0] for match in matches],
            ["openai.chatgpt-1.0.0-win32-x64"],
        )


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
    def test_check_rejects_stale_output_without_regeneration(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            schema = root / config_schema_check.GENERATED_OUTPUTS[0]
            schema.parent.mkdir(parents=True)
            schema.write_bytes(b'{"stale": true}\n')
            with (
                mock.patch.object(config_schema_check, "repo_root", return_value=root),
                mock.patch.object(
                    config_schema_check, "run_protocol_check", return_value=1
                ) as check,
                mock.patch.object(
                    config_schema_check, "regenerate_schema"
                ) as regenerate,
            ):
                self.assertEqual(config_schema_check.main(["--mode", "check"]), 1)
            check.assert_called_once_with(root)
            regenerate.assert_not_called()
            self.assertEqual(schema.read_bytes(), b'{"stale": true}\n')

    def test_removed_baseline_is_rejected_by_both_schema_commands(self) -> None:
        for module in (config_schema_check, app_server_schema_runtime_check):
            with (
                self.subTest(module=module.__name__),
                self.assertRaises(SystemExit) as error,
            ):
                module.main(["--mode", "check", "--baseline", "HEAD"])
            self.assertEqual(error.exception.code, 2)

    def test_logged_command_quotes_arguments_with_spaces(self) -> None:
        stdout = io.StringIO()
        with (
            mock.patch.object(
                config_schema_check,
                "run_finite",
                return_value=mock.Mock(
                    returncode=0, stdout="", status="passed", output_truncated=False
                ),
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

    def test_missing_config_schema_commands_report_clean_diagnostics(self) -> None:
        for command in ("cargo", "just"):
            with self.subTest(command=command):
                stderr = io.StringIO()
                with (
                    mock.patch.object(
                        config_schema_check,
                        "run_finite",
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
    def setUp(self):
        self.baseline = mock.patch.object(
            app_server_schema_runtime_check, "resolve_baseline", return_value="a" * 40
        )
        self.baseline.start()
        self.addCleanup(self.baseline.stop)

    def test_command_launch_errors_preserve_exit_classification(self) -> None:
        for module in (config_schema_check, app_server_schema_runtime_check):
            for error, expected in (
                (FileNotFoundError("missing"), 127),
                (PermissionError("denied"), 1),
            ):
                with (
                    self.subTest(module=module.__name__, error=error),
                    mock.patch.object(module, "run_finite", side_effect=error),
                    contextlib.redirect_stdout(io.StringIO()),
                    contextlib.redirect_stderr(io.StringIO()) as stderr,
                ):
                    self.assertEqual(module.run(["tool"], cwd=Path.cwd()), expected)
                    self.assertIn(f"Could not run tool: {error}", stderr.getvalue())

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

    def test_missing_command_returns_clean_diagnostic(self) -> None:
        stderr = io.StringIO()
        with (
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_finite",
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
        completed = mock.Mock(
            returncode=0, stdout="", status="passed", output_truncated=False
        )
        with (
            mock.patch.object(
                app_server_schema_runtime_check,
                "run_finite",
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
                    [
                        "--mode",
                        "force",
                        "--owner",
                        "assignment:app-server-owner",
                        "--compatibility-baseline",
                        "contract",
                    ]
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
                app_server_schema_runtime_check.main(
                    ["--mode", "check", "--compatibility-baseline", "contract"]
                ),
                0,
            )

        self.assertEqual(
            calls,
            ["lock", "protocol", "compatibility:" + "a" * 40, "python-sdk", "unlock"],
        )

    def test_schema_gate_stops_before_consumer_on_compatibility_failure(self) -> None:
        with (
            mock.patch.object(
                app_server_schema_runtime_check, "repo_root", return_value=Path("/repo")
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
                app_server_schema_runtime_check.main(
                    ["--mode", "check", "--compatibility-baseline", "contract"]
                ),
                1,
            )

        consumer.assert_not_called()

    def test_app_server_schema_rejects_removed_auto_mode(self) -> None:
        with self.assertRaises(SystemExit) as raised:
            app_server_schema_runtime_check.main(["--mode", "auto"])

        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
