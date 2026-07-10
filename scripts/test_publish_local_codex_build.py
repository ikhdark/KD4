#!/usr/bin/env python3

from pathlib import Path
import os
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


class PublishLocalCodexBuildTest(PublishLocalCodexTestBase):
    def test_actual_release_build_skips_preflight_and_uses_target_dir_argument(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "echo cargoTargetDirEnv=%CARGO_TARGET_DIR%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = str(temp_path / "inherited-target")

            result = self.run_script(
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("target\\publish-release", result.stdout)
            self.assertNotRegex(result.stdout, r"fake cargo .* check ")
            self.assertIn("cargoTargetDirEnv=", result.stdout)
            self.assertNotIn("inherited-target", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_build_only_returns_after_build_stamp_and_proof(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            built_dir = (
                self.repo_root / "codex-rs" / "target" / "publish-release" / "release"
            )
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        f'if not exist "{built_dir}" mkdir "{built_dir}"',
                        f'copy /y "%ComSpec%" "{built_dir / "codex.exe"}" >nul',
                        f'copy /y "%ComSpec%" "{built_dir / "codex-code-mode-host.exe"}" >nul',
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-BuildOnly",
                "-RunDoctor",
                "-ConfigureDesktopLocalCli",
                "-RestartDesktop",
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            stamp = (
                self.repo_root
                / "codex-rs"
                / "target"
                / "codex-local-publish-release.stamp"
            )
            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertTrue(stamp.exists())
            self.assertIn("action: build-only", result.stdout)
            self.assertIn("buildOnly: true", result.stdout)
            self.assertIn("builtCodexPath:", result.stdout)
            self.assertIn("buildStampPath:", result.stdout)
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertNotIn("sourceSha256:", result.stdout)
            self.assertNotIn("targetPath:", result.stdout)
            self.assertNotIn("publishLock:", result.stdout)
            self.assertNotIn("desktopLocalCliRouting:", result.stdout)
            self.assertNotIn("doctorCommand:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_test_run_executes_build_and_doctor_without_publishing(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_codex = temp_path / "fake-codex.cmd"
            fake_codex.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'if "%1"=="doctor" (',
                        'echo {"checks":{"auth.credentials":{"status":"fail"},"local_publish.readiness":{"status":"warning","summary":"doctor is not running from the local publish target"},"desktop.runtime_chain":{"status":"ok","summary":"desktop runtime chain evidence collected"},"app_server.status":{"status":"ok","summary":"background server reachable"},"network.websocket_reachability":{"status":"warning"}}}',
                        "exit /b 1",
                        ")",
                        "echo codex 9.9.9",
                        "echo commit: test-commit",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            self.write_build_stamp("release", FIXTURE_TIME, fake_codex)
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
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-TestRun",
                "-AutoSkipBuild",
                "-RunDoctor",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("action: test-run", result.stdout)
            self.assertIn("testRun: true", result.stdout)
            self.assertIn("autoSkipBuild: true", result.stdout)
            self.assertIn(
                "autoSkipBuildReason: source artifacts and tracked publish inputs match build stamp",
                result.stdout,
            )
            self.assertIn("buildCommand: <skipped>", result.stdout)
            self.assertNotIn("fake cargo --config ", result.stdout)
            self.assertIn(f'doctorCommand: "{fake_codex}" doctor --json', result.stdout)
            self.assertIn(
                "doctorStatus: warning: auth.credentials missing", result.stdout
            )
            self.assertIn("doctorLocalPublishStatus: warning", result.stdout)
            self.assertIn(
                "doctorLocalPublishSummary: doctor is not running from the local publish target",
                result.stdout,
            )
            self.assertIn("doctorDesktopRuntimeStatus: ok", result.stdout)
            self.assertIn(
                "doctorDesktopRuntimeSummary: desktop runtime chain evidence collected",
                result.stdout,
            )
            self.assertIn("doctorAppServerStatus: ok", result.stdout)
            self.assertIn(
                "doctorAppServerSummary: background server reachable", result.stdout
            )
            self.assertIn("replace: not run: test run", result.stdout)
            self.assertIn("restartRequired: false", result.stdout)
            self.assertNotIn("targetPath:", result.stdout)
            self.assertNotIn("publishLock:", result.stdout)
            self.assertNotIn("desktopLocalCliRouting:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_no_sccache_switch_disables_rustc_wrapper(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        'if "%1"=="--config" type "%2"',
                        "echo rustcWrapperEnv=%RUSTC_WRAPPER%",
                        "echo cargoBuildRustcWrapperEnv=%CARGO_BUILD_RUSTC_WRAPPER%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["RUSTC_WRAPPER"] = "sccache"
            env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache"

            result = self.run_script(
                "-NoSccache",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("rustcWrapper: <none: disabled by -NoSccache>", result.stdout)
            self.assertIn(
                "cargoRustcWrapperConfig: <none: disabled by -NoSccache>",
                result.stdout,
            )
            self.assertIn("rustcWrapperEnv=", result.stdout)
            self.assertIn("cargoBuildRustcWrapperEnv=", result.stdout)
            self.assertNotIn("rustcWrapperEnv=sccache", result.stdout)
            self.assertNotIn("cargoBuildRustcWrapperEnv=sccache", result.stdout)
            self.assertIn("fake cargo --config ", result.stdout)
            self.assertIn(" build --target-dir ", result.stdout)
            self.assertIn("[build]", result.stdout)
            self.assertIn('rustc-wrapper = ""', result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_publish_build_sets_version_metadata_env(self) -> None:
        self.init_repo_fixture()
        expected_commit = self.run_git("rev-parse", "--short=12", "HEAD").stdout.strip()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_codex = self.copy_valid_codex(
                temp_path / "fake-codex.exe",
                timestamp=FIXTURE_TIME + 450,
                append_padding=True,
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        "echo metadata commit=%CODEX_BUILD_COMMIT%",
                        "echo metadata dirty=%CODEX_BUILD_DIRTY%",
                        "echo metadata profile=%CODEX_BUILD_PROFILE%",
                        "echo metadata timestamp=%CODEX_BUILD_TIMESTAMP%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-NoSccache",
                "-SourceExe",
                str(fake_codex),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(f"buildMetadataCommit: {expected_commit}", result.stdout)
            self.assertIn("buildMetadataDirty: false", result.stdout)
            self.assertIn("buildMetadataProfile: release", result.stdout)
            self.assertIn(f"metadata commit={expected_commit}", result.stdout)
            self.assertIn("metadata dirty=false", result.stdout)
            self.assertIn("metadata profile=release", result.stdout)
            self.assertNotIn("metadata timestamp=unknown", result.stdout)
            self.assert_no_publish_temps(install_dir)


if __name__ == "__main__":
    unittest.main()
