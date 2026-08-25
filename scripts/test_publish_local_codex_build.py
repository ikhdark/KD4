#!/usr/bin/env python3

from pathlib import Path
import os
import subprocess
import tempfile
import unittest

from scripts.publish_local_codex_test_support import PublishLocalCodexTestBase
from scripts.publish_local_codex_test_support import clean_env
from scripts.publish_local_codex_test_support import ps_single_quote


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


class PublishLocalCodexBuildTest(PublishLocalCodexTestBase):
    def test_rusty_v8_target_prefers_rust_toolchain_host(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            fake_bin = Path(temp_dir)
            (fake_bin / "rustc.cmd").write_text(
                "@echo off\r\necho rustc 1.0.0\r\necho host: x86_64-pc-windows-msvc\r\n",
                encoding="utf-8",
            )
            command = "\n".join(
                [
                    "$tokens = $null",
                    "$errors = $null",
                    f"$ast = [System.Management.Automation.Language.Parser]::ParseFile({ps_single_quote(SCRIPT)}, [ref]$tokens, [ref]$errors)",
                    "$definition = $ast.Find({ param($node) $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Get-WindowsRustyV8Target' }, $true)",
                    "Invoke-Expression $definition.Extent.Text",
                    "Get-WindowsRustyV8Target",
                ]
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["PROCESSOR_ARCHITEW6432"] = "ARM64"
            env["PROCESSOR_ARCHITECTURE"] = "ARM64"
            result = subprocess.run(
                [self.shell, "-NoProfile", "-Command", command],
                cwd=SCRIPT.parent.parent,
                env=env,
                text=True,
                encoding="utf-8",
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
                creationflags=CREATE_NO_WINDOW,
            )
            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertEqual(result.stdout.strip(), "x86_64-pc-windows-msvc")
            self.assertNotIn("OSArchitecture", SCRIPT.read_text(encoding="utf-8"))

    def test_actual_release_build_runs_one_artifact_producing_cargo_command(
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
                        "echo cargo progress 1>&2",
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
            self.assertEqual(result.stdout.count("fake cargo "), 1)
            self.assertIn("cargoTargetDirEnv=", result.stdout)
            self.assertNotIn("inherited-target", result.stdout)
            self.assert_no_publish_temps(install_dir)

    def test_new_content_stamp_overrides_old_sidecar_mtime(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            source_timestamp = FIXTURE_TIME + 100
            sidecar_timestamp = FIXTURE_TIME - 100
            self.touch_tracked_source(source_timestamp)
            built_dir = (
                self.repo_root / "codex-rs" / "target" / "publish-release" / "release"
            )
            built_codex = built_dir / "codex.exe"
            built_code_mode_host = built_dir / "codex-code-mode-host.exe"
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        f'if not exist "{built_dir}" mkdir "{built_dir}"',
                        f'copy /y "%ComSpec%" "{built_codex}" >nul',
                        f'copy /y "%ComSpec%" "{built_code_mode_host}" >nul',
                        (
                            'python -c "import os; '
                            f"os.utime(r'{built_code_mode_host}', "
                            f'({sidecar_timestamp}, {sidecar_timestamp}))"'
                        ),
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-SourceExe",
                str(built_codex),
                "-SourceCodeModeHostExe",
                str(built_code_mode_host),
                "-InstallDir",
                str(install_dir),
                env=env,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assert_proof_value(
                result.stdout,
                "buildStamp",
                "written: content and artifact hashes recorded",
            )
            self.assert_proof_value(
                result.stdout,
                "sourceBuildFreshnessBasis",
                "content-bound build stamp",
            )
            self.assert_proof_value(
                result.stdout,
                "codeModeHostSourceBuildStale",
                "False",
            )
            self.assert_proof_value(result.stdout, "sourceBuildStale", "False")
            self.assertNotIn("sourceBuildStaleRemedy:", result.stdout)

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

    def test_build_only_auto_skip_reuses_and_invalidates_content_stamp(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            build_count = temp_path / "cargo-build-count.txt"
            built_dir = (
                self.repo_root / "codex-rs" / "target" / "publish-release" / "release"
            )
            fake_cargo = fake_bin / "cargo.cmd"
            fake_cargo.write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo fake cargo %*",
                        'if "%1"=="--version" exit /b 0',
                        f'echo build>>"{build_count}"',
                        f'if not exist "{built_dir}" mkdir "{built_dir}"',
                        f'copy /y "%ComSpec%" "{built_dir / "codex.exe"}" >nul',
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            auto_skip_args = (
                "-BuildOnly",
                "-AutoSkipBuild",
                "-SkipPreflightCheck",
                "-InstallDir",
                str(install_dir),
            )

            initial = self.run_script(*auto_skip_args, env=env)
            self.assertEqual(
                initial.returncode,
                0,
                f"stdout:\n{initial.stdout}\nstderr:\n{initial.stderr}",
            )
            self.assertIn("autoSkipBuild: false", initial.stdout)
            self.assertIn(
                "autoSkipBuildReason: source artifact missing", initial.stdout
            )
            self.assertIn("fake cargo --config ", initial.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 1
            )

            cached = self.run_script(*auto_skip_args, env=env)
            self.assertEqual(
                cached.returncode,
                0,
                f"stdout:\n{cached.stdout}\nstderr:\n{cached.stderr}",
            )
            self.assertIn("autoSkipBuild: true", cached.stdout)
            self.assertIn(
                "autoSkipBuildReason: source artifacts and tracked publish inputs match build stamp",
                cached.stdout,
            )
            self.assertIn("buildCommand: <skipped>", cached.stdout)
            self.assertNotIn("fake cargo --config ", cached.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 1
            )

            (built_dir / "codex.exe").write_bytes(b"mutated release artifact")
            artifact_invalidated = self.run_script(*auto_skip_args, env=env)
            self.assertEqual(
                artifact_invalidated.returncode,
                0,
                f"stdout:\n{artifact_invalidated.stdout}\nstderr:\n{artifact_invalidated.stderr}",
            )
            self.assertIn("autoSkipBuild: false", artifact_invalidated.stdout)
            self.assertIn(
                "autoSkipBuildReason: source artifact differs from stamped build",
                artifact_invalidated.stdout,
            )
            self.assertIn("fake cargo --config ", artifact_invalidated.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 2
            )

            forced = self.run_script(
                "-BuildOnly",
                "-SkipPreflightCheck",
                "-InstallDir",
                str(install_dir),
                env=env,
            )
            self.assertEqual(
                forced.returncode,
                0,
                f"stdout:\n{forced.stdout}\nstderr:\n{forced.stderr}",
            )
            self.assertNotIn("autoSkipBuild:", forced.stdout)
            self.assertIn("fake cargo --config ", forced.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 3
            )

            self.touch_tracked_source(FRESH_SOURCE_TIME)
            invalidated = self.run_script(*auto_skip_args, env=env)
            self.assertEqual(
                invalidated.returncode,
                0,
                f"stdout:\n{invalidated.stdout}\nstderr:\n{invalidated.stderr}",
            )
            self.assertIn("autoSkipBuild: false", invalidated.stdout)
            self.assertIn(
                "autoSkipBuildReason: tracked publish inputs changed",
                invalidated.stdout,
            )
            self.assertIn("fake cargo --config ", invalidated.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 4
            )

            for result in (initial, cached, artifact_invalidated, forced, invalidated):
                self.assertIn("buildOnly: true", result.stdout)
                self.assertNotIn("targetPath:", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_build_only_rejects_explicit_skip_build(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_script(
                "-BuildOnly",
                "-SkipBuild",
                "-InstallDir",
                str(Path(temp_dir) / "install"),
            )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "-BuildOnly cannot be combined with -SkipBuild.",
                result.stdout + result.stderr,
            )

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
                        "echo doctor warning 1>&2",
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
            self.write_build_stamp("release", FIXTURE_TIME, fake_codex, env=env)

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
            self.assertIn("doctor warning", result.stdout)
            self.assertIn(
                "doctorStatus: warning: allowed non-runtime doctor failure",
                result.stdout,
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

    def test_missing_sccache_clears_stale_inherited_wrapper(self) -> None:
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
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "echo rustcWrapperEnv=%RUSTC_WRAPPER%",
                        "echo cargoBuildRustcWrapperEnv=%CARGO_BUILD_RUSTC_WRAPPER%",
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = str(fake_bin)
            env["RUSTC_WRAPPER"] = "sccache"
            env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache.exe"

            result = self.run_script(
                "-Profile",
                "local-release",
                "-SkipPreflightCheck",
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
            self.assertIn("rustcWrapper: <none: sccache not found>", result.stdout)
            self.assertIn("rustcWrapperEnv=", result.stdout)
            self.assertIn("cargoBuildRustcWrapperEnv=", result.stdout)
            self.assertNotIn("rustcWrapperEnv=sccache", result.stdout)
            self.assertNotIn("cargoBuildRustcWrapperEnv=sccache", result.stdout)

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
