#!/usr/bin/env python3

from pathlib import Path
import os
import subprocess
import tempfile
import unittest
from unittest import mock

from scripts import app_server_schema_runtime_check
from scripts import config_schema_check
from scripts import dev_env_doctor
from scripts import git_doctor
from scripts import vscode_runtime_proof


REPO_ROOT = Path(__file__).resolve().parents[1]


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


class GitDoctorTest(unittest.TestCase):
    def test_path_kind_detects_wsl_windows_mount(self) -> None:
        self.assertEqual(
            git_doctor.path_kind(Path("/mnt/c/Users/kuh/repo")), "wsl-windows-mount"
        )

    def test_recommendations_include_git_tuning_when_unset(self) -> None:
        recs = "\n".join(git_doctor.recommendations("windows", None, None))
        self.assertIn("core.fsmonitor", recs)
        self.assertIn("core.untrackedCache", recs)

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

    def test_config_schema_auto_regenerates_after_skipped_check_failure(self) -> None:
        with (
            mock.patch.object(
                config_schema_check, "repo_root", return_value=Path("/repo")
            ),
            mock.patch.object(
                config_schema_check, "schema_inputs_changed", return_value=False
            ),
            mock.patch.object(
                config_schema_check, "run_protocol_check", side_effect=[1, 0]
            ) as run_check,
            mock.patch.object(
                config_schema_check, "regenerate_schema", return_value=True
            ) as regenerate,
        ):
            self.assertEqual(config_schema_check.main(["--mode", "auto"]), 1)

        regenerate.assert_called_once_with(Path("/repo"))
        self.assertEqual(run_check.call_count, 2)


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

    def test_auto_regenerates_after_skipped_check_failure(self) -> None:
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
                side_effect=[1, 0],
            ) as run_check,
            mock.patch.object(
                app_server_schema_runtime_check,
                "regenerate_schemas",
                return_value=True,
            ) as regenerate,
        ):
            self.assertEqual(
                app_server_schema_runtime_check.main(["--mode", "auto"]), 1
            )

        regenerate.assert_called_once_with(Path("/repo"))
        self.assertEqual(run_check.call_count, 2)


class WslPublishBridgeTest(unittest.TestCase):
    def test_wsl_bridge_routes_to_windows_powershell_script(self) -> None:
        text = (REPO_ROOT / "scripts" / "publish-local-codex-wsl.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            'windows_repo_root="$(wslpath -w "$repo_root")"',
            text,
        )
        self.assertIn(
            'windows_script="$(wslpath -w "$repo_root/scripts/publish-local-codex.ps1")"',
            text,
        )
        self.assertRegex(
            text,
            r'(?m)^\s*powershell\.exe\b.*-File "\$windows_script".*-RepoRoot "\$windows_repo_root"',
        )


if __name__ == "__main__":
    unittest.main()
