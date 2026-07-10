#!/usr/bin/env python3

from pathlib import Path
import os
import re
import shutil
import subprocess
import tempfile
import unittest

from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
HASHING_HELPER = Path(__file__).resolve().parent / "publish-local-codex.hashing.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: production invokes publish-local-codex.ps1
    # via `powershell -NoProfile -File ...` from the justfile, and 5.1 has
    # stricter native-stderr and StrictMode semantics than pwsh 7 — bugs in
    # that class are invisible when the tests run under pwsh.
    return shutil.which("powershell") or shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


PUBLISH_ENV_VARS = (
    "CODEX_LOCAL_PUBLISH_DIR",
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_CLI_PATH",
)


def clean_env() -> dict[str, str]:
    # A prior -ConfigureDesktopLocalCli publish persists these at User scope,
    # so the inherited environment can carry them; the script prefers
    # CODEX_LOCAL_PUBLISH_DIR over the test's temp USERPROFILE, which makes
    # assertions machine-state-dependent unless they are stripped.
    env = os.environ.copy()
    for name in PUBLISH_ENV_VARS:
        env.pop(name, None)
    return env


class PublishLocalCodexDryRunTest(PublishLocalCodexTestBase):
    def test_dry_run_reports_proof_without_writing_target(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("DRY-RUN", result.stdout)
            self.assertIn("sourceSha256:", result.stdout)
            self.assertIn("targetPath:", result.stdout)
            self.assertIn(
                f"sourceCodeModeHostPath: {self.source_code_mode_host}", result.stdout
            )
            self.assertIn(
                f"codeModeHostTargetPath: {install_dir / 'codex-code-mode-host.exe'}",
                result.stdout,
            )
            self.assertIn("sourceCodeModeHostSha256:", result.stdout)
            self.assertIn("codeModeHostTargetBeforeSha256: <missing>", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())
            self.assertFalse((install_dir / "codex-code-mode-host.exe").exists())

    def test_dry_run_allows_missing_default_source_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("profile: release", result.stdout)
            self.assertIn("sourceMissing: true", result.stdout)
            self.assertIn("buildCommand: cargo --config", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("--profile release", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_dry_run_reports_missing_windows_rusty_v8_archive(self) -> None:
        self.write_cargo_lock_with_v8()
        archive_url = self.rusty_v8_archive_url()
        cache_name = re.sub(r"[^A-Za-z0-9]", "_", archive_url)
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            env = self.publish_env_without_v8_archive(user_profile)

            result = self.run_script(
                "-DryRun",
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("v8ArchiveVersion: 149.2.0", result.stdout)
            self.assertIn(
                f"v8ArchiveUrl: {archive_url}",
                result.stdout,
            )
            self.assertIn(cache_name, result.stdout)
            self.assertIn("v8ArchiveStatus: missing", result.stdout)
            self.assertIn("v8ArchiveRemedy:", result.stdout)
            self.assertIn("-RustyV8Archive", result.stdout)
            self.assertIn("-AllowRustyV8Download", result.stdout)

    def test_publish_fails_before_cargo_when_windows_rusty_v8_archive_missing(
        self,
    ) -> None:
        self.write_cargo_lock_with_v8()
        target = self.expected_windows_rusty_v8_target()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            env = self.publish_env_without_v8_archive(user_profile)

            result = self.run_script(
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            combined = result.stdout + result.stderr
            self.assertNotEqual(result.returncode, 0, combined)
            self.assertIn("v8ArchiveStatus: missing", result.stdout)
            self.assertIn(
                f"Rusty V8 archive is missing for v149.2.0 ({target})",
                combined,
            )
            self.assertNotIn("cargo check failed", combined)

    def test_publish_seeds_windows_rusty_v8_cache_from_archive(self) -> None:
        self.write_cargo_lock_with_v8()
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            user_profile = temp_path / "profile"
            user_profile.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FRESH_SOURCE_TIME,
                append_padding=True,
            )
            archive = temp_path / self.rusty_v8_archive_name()
            archive.write_bytes(b"fake rusty v8 archive")
            checksum = self.write_rusty_v8_checksum(archive)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = self.publish_env_without_v8_archive(user_profile)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-RustyV8Archive",
                str(archive),
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            cache_path = self.rusty_v8_cache_path(user_profile)
            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"v8ArchiveChecksum: {checksum}", result.stdout)
            self.assertIn("v8ArchiveChecksumStatus: ok", result.stdout)
            self.assertIn("v8ArchiveCacheAction: seeded from", result.stdout)
            self.assertIn("v8ArchiveStatus: cached", result.stdout)
            self.assertEqual(cache_path.read_bytes(), archive.read_bytes())

    def test_dry_run_disambiguates_cli_payload_from_desktop_app(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(
                "targetKind: local CLI/TUI payload used by Codex Desktop; "
                "launching it directly opens a terminal.",
                result.stdout,
            )
            self.assertIn("desktopAppExecutable:", result.stdout)
            self.assertIn(
                "desktopAppLaunchCommand: explorer.exe "
                "shell:AppsFolder\\OpenAI.Codex_2p2nqsd0c76g0!App",
                result.stdout,
            )

    def test_dry_run_reports_desktop_local_cli_routing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            local_codex_home = Path(temp_dir) / "codex-home"
            env = clean_env()
            path_key = next((key for key in env if key.lower() == "path"), "Path")
            env["CODEX_CLI_PATH"] = str(Path(temp_dir) / "old-codex.exe")
            env[path_key] = f"{install_dir};{env.get(path_key, '')}"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-DesktopCliEnvironmentTarget",
                "Process",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                "-LocalCodexHome",
                str(local_codex_home),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "desktopLocalCliRouting", "enabled")
            self.assert_proof_value(
                result.stdout, "localCodexHome", str(local_codex_home)
            )
            self.assert_proof_value(result.stdout, "localCodexHomeScope", "Process")
            self.assert_proof_value(
                result.stdout, "localCodexHomeAction", "would create"
            )
            self.assert_proof_value(
                result.stdout,
                "localCodexSqliteHome",
                str(local_codex_home / "sqlite"),
            )
            self.assert_proof_value(
                result.stdout, "localCodexSqliteHomeScope", "Process"
            )
            self.assert_proof_value(
                result.stdout, "localCodexSqliteHomeAction", "would create"
            )
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvName", "CODEX_CLI_PATH"
            )
            self.assert_proof_value(
                result.stdout,
                "desktopCliPathEnvTarget",
                str(install_dir / "codex.exe"),
            )
            self.assert_proof_value(result.stdout, "desktopCliPathEnvScope", "Process")
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvAction", "would set"
            )
            self.assert_proof_value(
                result.stdout,
                "officialEnvCleanup",
                "CODEX_HOME unset, CODEX_CLI_PATH unset, CODEX_SQLITE_HOME unset",
            )
            self.assert_proof_value(
                result.stdout,
                "desktopUserPathLocalBinAction",
                "would remove 1 entry",
            )

    def test_dry_run_supports_user_scope_desktop_local_cli_routing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            local_codex_home = Path(temp_dir) / "codex-home"

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-DesktopCliEnvironmentTarget",
                "User",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
                "-LocalCodexHome",
                str(local_codex_home),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(result.stdout, "localCodexHomeScope", "User")
            self.assert_proof_value(
                result.stdout,
                "localCodexSqliteHome",
                str(local_codex_home / "sqlite"),
            )
            self.assert_proof_value(result.stdout, "localCodexSqliteHomeScope", "User")
            self.assert_proof_value(result.stdout, "desktopCliPathEnvScope", "User")
            self.assert_proof_value(
                result.stdout, "desktopCliPathEnvAction", "would set"
            )
            self.assert_proof_value(
                result.stdout, "desktopEnvironmentBroadcast", "would send"
            )

    def test_default_dry_run_reports_desktop_localexe_target(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            user_profile = Path(temp_dir) / "profile"
            expected_home = user_profile / "Desktop" / "LOCAL-KD"
            expected_target = expected_home / "codex.exe"
            stale_codex_home = user_profile / ".codex-test-home"
            env = clean_env()
            env["USERPROFILE"] = str(user_profile)
            env["CODEX_HOME"] = str(stale_codex_home)

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-ConfigureDesktopLocalCli",
                "-SourceExe",
                str(self.source_exe),
                env=env,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assert_proof_value(result.stdout, "targetPath", str(expected_target))
        self.assert_proof_value(result.stdout, "localCodexHome", str(expected_home))
        self.assert_proof_value(result.stdout, "localCodexHomeScope", "Process")
        self.assert_proof_value(result.stdout, "localCodexHomeAction", "would create")
        self.assert_proof_value(
            result.stdout,
            "localCodexSqliteHome",
            str(expected_home / "sqlite"),
        )
        self.assert_proof_value(result.stdout, "localCodexSqliteHomeScope", "Process")
        self.assert_proof_value(
            result.stdout, "localCodexSqliteHomeAction", "would create"
        )
        self.assert_proof_value(
            result.stdout,
            "desktopCliPathEnvTarget",
            str(expected_target),
        )
        self.assert_proof_value(
            result.stdout,
            "officialEnvCleanup",
            "CODEX_HOME unset, CODEX_CLI_PATH unset, CODEX_SQLITE_HOME unset",
        )
        self.assertNotEqual(
            self.proof_value(result.stdout, "localCodexHome"),
            str(stale_codex_home),
        )
        self.assertNotIn(
            "AppData\\Local\\OpenAI\\Codex\\bin\\codexKD-local\\codex.exe",
            result.stdout,
        )

    def test_dry_run_reports_source_build_stamp_details(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo codex 9.9.9",
                        "echo commit: abc123def456",
                        "echo dirty: true",
                        "echo profile: release",
                        "echo built: 123s since unix epoch",
                    ]
                ),
                encoding="utf-8",
            )

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceCommit: abc123def456", result.stdout)
            self.assertIn("sourceDirty: true", result.stdout)
            self.assertIn("sourceProfile: release", result.stdout)
            self.assertIn("sourceBuilt: 123s since unix epoch", result.stdout)

    def test_dry_run_reports_stale_target_when_source_tree_is_newer(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            install_dir = Path(temp_dir) / "install"
            install_dir.mkdir()
            target = install_dir / "codex.exe"
            target.write_bytes(self.source_exe_bytes)
            old_timestamp = 946684800
            os.utime(target, (old_timestamp, old_timestamp))

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(self.source_exe),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceTreeNewestWriteUtc:", result.stdout)
            self.assertIn("targetBeforeLastWriteUtc:", result.stdout)
            self.assertIn("targetBeforeStale: True", result.stdout)
            self.assertIn(
                "targetBeforeStaleRemedy: Run just publish-local-codex and restart Codex Desktop.",
                result.stdout,
            )

    def test_dry_run_reports_stale_source_build_when_skip_build_would_noop(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            stale_timestamp = 946684800
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=stale_timestamp,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(fake_codex.read_bytes())
            os.utime(target, (stale_timestamp, stale_timestamp))
            self.install_matching_code_mode_host(install_dir)

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assert_publish_readiness(result.stdout, "blocked: source build stale")
            self.assertIn(
                "sourceBuildStaleRemedy: Run just publish-local-codex -Profile release -RunDoctor without -SkipBuild, then restart Codex Desktop.",
                result.stdout,
            )
            self.assertIn("binaryChanged: false", result.stdout)
            self.assertIn("replace: not run: source build stale", result.stdout)
            self.assertIn("restartRequired: unknown until rebuild", result.stdout)
            self.assertNotIn("replace: not run: target already current", result.stdout)

    def test_runtime_proof_reports_doctor_skip_before_stale_source_failure(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            stale_timestamp = 946684800
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=stale_timestamp,
            )

            result = self.run_script(
                "-DryRun",
                "-SkipBuild",
                "-RunDoctor",
                "-RuntimeProof",
                "-FailOnStaleSourceBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("runtimeProof: requested", result.stdout)
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assert_publish_readiness(result.stdout, "blocked: source build stale")
            self.assertIn(
                f'doctorCommand: "{install_dir / "codex.exe"}" doctor --json (not run: target missing)',
                result.stdout,
            )
            self.assertIn("doctorStatus: skipped: target missing", result.stdout)
            self.assertIn("Dry-run source build is stale", result.stderr)

    def test_apply_blocks_skip_build_when_source_binary_is_stale(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.write_fake_codex(
                temp_path / "fake-codex.cmd",
                timestamp=946684800,
            )
            target = install_dir / "codex.exe"
            target.write_bytes(b"previous-codex")

            result = self.run_script(
                "-SkipBuild",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(target.read_bytes(), b"previous-codex")
            self.assertFalse((install_dir / "backups").exists())
            self.assertIn("sourceBuildStale: True", result.stdout)
            self.assertIn("replace: blocked: source build stale", result.stdout)
            self.assertIn("restartRequired: unknown until rebuild", result.stdout)
            self.assertIn(
                "SkipBuild cannot publish the newest Codex bundle",
                result.stderr,
            )


if __name__ == "__main__":
    unittest.main()
