#!/usr/bin/env python3

from datetime import datetime
from pathlib import Path
import json
import re
import os
import shutil
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
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            calls = temp_path / "cargo-calls.txt"
            self.write_fake_cargo(
                fake_bin,
                "echo fake cargo %*",
                f'echo invoked>>"{calls}"',
                "echo cargo progress 1>&2",
                "echo cargoTargetDirEnv=%CARGO_TARGET_DIR%",
                profile="release",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = str(temp_path / "inherited-target")

            result = self.run_script(
                "-Profile",
                "release",
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
            self.assertEqual(calls.read_text().splitlines(), ["invoked"])
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
            _, built_code_mode_host, _, _ = self.built_artifact_paths()
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            self.write_fake_cargo(
                fake_bin,
                "echo fake cargo %*",
                (
                    'python -c "import os; '
                    f"os.utime(r'{built_code_mode_host}', "
                    f'({sidecar_timestamp}, {sidecar_timestamp}))"'
                ),
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
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
            self.write_fake_cargo(fake_bin, "echo fake cargo %*")
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
                / "codex-local-publish-local-release.stamp"
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
            self.assertIn("fake cargo build --target-dir ", result.stdout)
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
            snapshot_calls = temp_path / "snapshot-calls.txt"
            observed = {"Get-LocalPublishBuildInputSnapshot": snapshot_calls}
            mutate_during_build = temp_path / "mutate-during-build"
            tracked_source = self.repo_root / "codex-rs" / "tracked-source.rs"
            built_codex, _, _, _ = self.built_artifact_paths()
            self.write_fake_cargo(
                fake_bin,
                "echo fake cargo %*",
                f'if exist "{mutate_during_build}" echo changed during cargo>"{tracked_source}"',
                f'echo build>>"{build_count}"',
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

            initial = self.run_script(
                *auto_skip_args, env=env, observe_commands=observed
            )
            self.assertEqual(
                initial.returncode,
                0,
                f"stdout:\n{initial.stdout}\nstderr:\n{initial.stderr}",
            )
            self.assertIn("autoSkipBuild: false", initial.stdout)
            self.assertIn(
                "autoSkipBuildReason: source artifact missing", initial.stdout
            )
            self.assertIn("fake cargo build --target-dir ", initial.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 1
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 2)

            cached = self.run_script(
                *auto_skip_args, env=env, observe_commands=observed
            )
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
            self.assertNotIn("fake cargo build --target-dir ", cached.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 1
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 3)

            built_codex.write_bytes(b"mutated release artifact")
            artifact_invalidated = self.run_script(
                *auto_skip_args, env=env, observe_commands=observed
            )
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
            self.assertIn("fake cargo build --target-dir ", artifact_invalidated.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 2
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 5)

            forced = self.run_script(
                "-BuildOnly",
                "-SkipPreflightCheck",
                "-InstallDir",
                str(install_dir),
                env=env,
                observe_commands=observed,
            )
            self.assertEqual(
                forced.returncode,
                0,
                f"stdout:\n{forced.stdout}\nstderr:\n{forced.stderr}",
            )
            self.assertNotIn("autoSkipBuild:", forced.stdout)
            self.assertIn("fake cargo build --target-dir ", forced.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 3
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 7)

            self.touch_tracked_source(FRESH_SOURCE_TIME)
            invalidated = self.run_script(
                *auto_skip_args, env=env, observe_commands=observed
            )
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
            self.assertIn("fake cargo build --target-dir ", invalidated.stdout)
            self.assertEqual(
                len(build_count.read_text(encoding="utf-8").splitlines()), 4
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 9)

            # Reject an input change after the reused pre-build snapshot; a
            # missing post-build scan must not be able to stamp this build.
            self.touch_tracked_source(FRESH_SOURCE_TIME + 1)
            mutate_during_build.touch()
            changed_during_build = self.run_script(
                *auto_skip_args, env=env, observe_commands=observed
            )
            self.assertNotEqual(changed_during_build.returncode, 0)
            self.assertIn(
                "Local publish inputs changed during the build",
                changed_during_build.stdout + changed_during_build.stderr,
            )
            self.assertEqual(
                build_count.read_text(encoding="utf-8").splitlines(), ["build"] * 5
            )
            self.assertEqual(snapshot_calls.read_text().splitlines(), ["invoked"] * 11)
            self.assertFalse(
                (
                    self.repo_root
                    / "codex-rs"
                    / "target"
                    / "codex-local-publish-local-release.stamp"
                ).exists()
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

    def test_test_run_reuses_cached_build_and_runs_doctor_without_publishing(
        self,
    ) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            # TestRun exercises the Cargo output itself, so the doctor fake must
            # be a real executable at the built path.
            fake_codex, _, _, _ = self.write_built_artifacts()
            doctor_report = json.dumps(
                {
                    "checks": {
                        "auth.credentials": {"status": "fail"},
                        "local_publish.readiness": {
                            "status": "warning",
                            "summary": "doctor is not running from the local publish target",
                        },
                        "desktop.runtime_chain": {
                            "status": "ok",
                            "summary": "desktop runtime chain evidence collected",
                        },
                        "app_server.status": {
                            "status": "ok",
                            "summary": "background server reachable",
                        },
                        "network.websocket_reachability": {"status": "warning"},
                    }
                },
                separators=(",", ":"),
            ).replace('"', '""')
            source = "\n".join(
                [
                    "using System;",
                    "public static class FakeCodex {",
                    "    public static int Main(string[] args) {",
                    '        if (args.Length > 0 && args[0] == "doctor") {',
                    '            Console.Error.WriteLine("doctor warning");',
                    f'            Console.WriteLine(@"{doctor_report}");',
                    "            return 1;",
                    "        }",
                    '        Console.WriteLine("codex 9.9.9");',
                    '        Console.WriteLine("commit: test-commit");',
                    "        return 0;",
                    "    }",
                    "}",
                ]
            )
            fake_codex.unlink()
            compiled = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    f"Add-Type -TypeDefinition {ps_single_quote(source)} "
                    f"-OutputAssembly {ps_single_quote(fake_codex)} "
                    "-OutputType ConsoleApplication",
                ],
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
                creationflags=CREATE_NO_WINDOW,
            )
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            self.write_fake_cargo(fake_bin, "echo fake cargo %*")
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            self.write_build_stamp("local-release", FIXTURE_TIME, env=env)

            result = self.run_script(
                "-TestRun",
                "-AutoSkipBuild",
                "-RunDoctor",
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
            self.assertNotIn("fake cargo build --target-dir ", result.stdout)
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
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            self.write_fake_cargo(
                fake_bin,
                "echo fake cargo %*",
                'if "%1"=="--config" type "%2"',
                "echo rustcWrapperEnv=%RUSTC_WRAPPER%",
                "echo cargoBuildRustcWrapperEnv=%CARGO_BUILD_RUSTC_WRAPPER%",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["RUSTC_WRAPPER"] = "sccache"
            env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache"

            result = self.run_script(
                "-NoSccache",
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

    def test_environment_rustflags_cannot_replace_checked_in_target_flags(
        self,
    ) -> None:
        # Cargo lets RUSTFLAGS or CARGO_ENCODED_RUSTFLAGS, even when empty,
        # replace the target rustflags that give published binaries their 8 MiB
        # stack and static CRT; the per-target variable joins them instead.
        self.init_repo_fixture()
        for name, value, blocked in (
            ("RUSTFLAGS", "-C target-cpu=native", True),
            ("RUSTFLAGS", "", True),
            ("CARGO_ENCODED_RUSTFLAGS", "-Ctarget-cpu=native", True),
            (
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
                "-C target-cpu=native",
                False,
            ),
        ):
            with (
                self.subTest(name=name, value=value),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                temp_path = Path(temp_dir)
                install_dir = temp_path / "install"
                install_dir.mkdir()
                fake_bin = temp_path / "bin"
                fake_bin.mkdir()
                calls = temp_path / "cargo-calls.txt"
                self.write_fake_cargo(fake_bin, f'echo invoked>>"{calls}"')
                env = clean_env()
                env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
                env[name] = value

                result = self.run_script("-InstallDir", str(install_dir), env=env)

                output = f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
                if blocked:
                    self.assertNotEqual(result.returncode, 0, output)
                    self.assertFalse(calls.exists(), output)
                    self.assertIn(f"{name} is set", result.stderr)
                    self.assertFalse((install_dir / "codex.exe").exists())
                else:
                    self.assertEqual(result.returncode, 0, output)
                    self.assertEqual(calls.read_text().splitlines(), ["invoked"])

    def test_missing_sccache_clears_stale_inherited_wrapper(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            install_dir.mkdir()
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            self.write_fake_cargo(
                fake_bin,
                "echo rustcWrapperEnv=%RUSTC_WRAPPER%",
                "echo cargoBuildRustcWrapperEnv=%CARGO_BUILD_RUSTC_WRAPPER%",
            )
            # Builds require the git input fingerprint, but git's directory may
            # also hold sccache, so forward git instead of widening PATH.
            (fake_bin / "git.cmd").write_text(
                f'@"{shutil.which("git")}" %*\r\n', encoding="utf-8"
            )
            env = clean_env()
            env["PATH"] = str(fake_bin)
            env["RUSTC_WRAPPER"] = "sccache"
            env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache.exe"

            result = self.run_script(
                "-Profile",
                "local-release",
                "-SkipPreflightCheck",
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
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            self.write_fake_cargo(
                fake_bin,
                "echo fake cargo %*",
                "echo metadata commit=%CODEX_BUILD_COMMIT%",
                "echo metadata dirty=%CODEX_BUILD_DIRTY%",
                "echo metadata profile=%CODEX_BUILD_PROFILE%",
                "echo metadata timestamp=%CODEX_BUILD_TIMESTAMP%",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script(
                "-NoSccache",
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
            self.assertIn("buildMetadataProfile: local-release", result.stdout)
            self.assertIn(f"metadata commit={expected_commit}", result.stdout)
            self.assertIn("metadata dirty=false", result.stdout)
            self.assertIn("metadata profile=local-release", result.stdout)
            timestamp = re.search(
                r"^metadata timestamp=(.+)$", result.stdout, re.MULTILINE
            )
            self.assertIsNotNone(timestamp, result.stdout)
            self.assertIsNotNone(datetime.fromisoformat(timestamp.group(1).strip()))
            self.assert_no_publish_temps(install_dir)

    def test_build_modes_reject_explicit_source_binaries(self) -> None:
        # A build must stamp and publish Cargo's outputs, never a caller's file.
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            calls = temp_path / "cargo-calls.txt"
            self.write_fake_cargo(fake_bin, f'echo invoked>>"{calls}"')
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            foreign_codex = self.copy_valid_codex(
                temp_path / "foreign-codex.exe", append_padding=True
            )
            stamp = (
                self.repo_root
                / "codex-rs"
                / "target"
                / "codex-local-publish-local-release.stamp"
            )

            for mode in ((), ("-AutoSkipBuild",), ("-BuildOnly",), ("-TestRun",), ("-DryRun",)):
                with self.subTest(mode=mode):
                    install_dir = temp_path / ("install" + "".join(mode))
                    result = self.run_script(
                        *mode,
                        "-SourceExe",
                        str(foreign_codex),
                        "-SourceCodeModeHostExe",
                        str(self.source_code_mode_host),
                        "-SourceWindowsSandboxSetupExe",
                        str(self.source_windows_sandbox_setup),
                        "-SourceCommandRunnerExe",
                        str(self.source_command_runner),
                        "-InstallDir",
                        str(install_dir),
                        env=env,
                    )

                    self.assertNotEqual(
                        result.returncode,
                        0,
                        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                    )
                    self.assertIn(
                        "Explicit source binaries require -SkipBuild", result.stderr
                    )
                    self.assertFalse(calls.exists())
                    self.assertFalse(stamp.exists())
                    self.assertFalse((install_dir / "codex.exe").exists())

    def test_publish_refuses_unbound_build_without_input_fingerprint(self) -> None:
        # Without git the inputs cannot be fingerprinted, so nothing could bind
        # the fresh build to the source it compiled.
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            calls = temp_path / "cargo-calls.txt"
            self.write_fake_cargo(fake_bin, f'echo invoked>>"{calls}"')
            env = clean_env()
            env["PATH"] = str(fake_bin)

            result = self.run_script("-InstallDir", str(install_dir), env=env)

            self.assertNotEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn(
                "Could not fingerprint local publish inputs before the build",
                result.stderr,
            )
            self.assertFalse(calls.exists())
            self.assertNotIn("publishCommitted: true", result.stdout)
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_build_rejects_stale_outputs_when_cargo_writes_elsewhere(self) -> None:
        # A configured build.target moves Cargo's outputs under a triple
        # directory; older files at the publish path must not be stamped.
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            install_dir = temp_path / "install"
            stale_codex, _, _, _ = self.write_built_artifacts(
                codex_bytes=self.source_exe_bytes + b"\r\nstale-build"
            )
            relocated = (
                stale_codex.parent.parent / "x86_64-pc-windows-msvc" / "local-release"
            )
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        'if "%1"=="--version" exit /b 0',
                        f'if not exist "{relocated}" mkdir "{relocated}"',
                        f'copy /y "%ComSpec%" "{relocated / "codex.exe"}" >nul',
                        "exit /b 0",
                    ]
                ),
                encoding="utf-8",
            )
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            result = self.run_script("-InstallDir", str(install_dir), env=env)

            self.assertNotEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("a built source artifact is missing", result.stderr)
            self.assertFalse(
                (
                    self.repo_root
                    / "codex-rs"
                    / "target"
                    / "codex-local-publish-local-release.stamp"
                ).exists()
            )
            self.assertFalse((install_dir / "codex.exe").exists())

    def test_build_identity_tracks_toolchain_config_and_compile_inputs(self) -> None:
        self.init_repo_fixture()
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            fake_bin = temp_path / "bin"
            fake_bin.mkdir()
            probe_dirs = temp_path / "probe-dirs.txt"
            # Report the working directory so identity changes if the toolchain
            # is resolved anywhere other than where Cargo builds.
            for tool in ("cargo", "rustc"):
                (fake_bin / f"{tool}.cmd").write_text(
                    f'@echo {tool} %CD%\r\n@echo %CD%>>"{probe_dirs}"\r\n',
                    encoding="utf-8",
                )
            # The linker keeps its path across upgrades, as scoop's llvm
            # `current` junction does; only its reported version changes.
            linker_version = temp_path / "linker-version.txt"
            linker_version.write_text("LLD 1.0\n", encoding="utf-8")
            (fake_bin / "lld-link.cmd").write_text(
                f'@type "{linker_version}"\r\n', encoding="utf-8"
            )
            cargo_home = temp_path / "cargo-home"
            cargo_home.mkdir()
            elsewhere = temp_path / "elsewhere"
            elsewhere.mkdir()
            env = clean_env()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_HOME"] = str(cargo_home)
            compile_inputs = {
                "CARGO_BUILD_RUSTC": r"C:\other\rustc.exe",
                "CODEX_RELEASE_VERSION": "9.9.9",
                "SOURCE_DATE_EPOCH": "1",
                "CFLAGS": "/arch:AVX2",
                "CXXFLAGS_x86_64_pc_windows_msvc": "/O1",
                "CMAKE_GENERATOR": "Ninja",
            }
            for name in compile_inputs:
                env.pop(name, None)
            assignments = "\n".join(
                f"Set-Item -Path Env:{name} -Value {ps_single_quote(value)}; "
                f"$fingerprints.{name} = Get-Identity; "
                f"Remove-Item -Path Env:{name}"
                for name, value in compile_inputs.items()
            )
            command = rf"""
$ErrorActionPreference = 'Stop'
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-Identity {{ Get-LocalPublishBuildInputFingerprint -RepoRoot {ps_single_quote(self.repo_root)} }}
$fingerprints = [ordered]@{{}}
Set-Location -LiteralPath {ps_single_quote(self.repo_root)}
$fingerprints.base = Get-Identity
Set-Location -LiteralPath {ps_single_quote(elsewhere)}
$fingerprints.otherCwd = Get-Identity
{assignments}
Set-Content -LiteralPath {ps_single_quote(cargo_home / "config.toml")} -Value '[profile.local-release]'
$fingerprints.cargoHomeConfig = Get-Identity
New-Item -ItemType Directory -Path {ps_single_quote(self.repo_root / ".cargo")} | Out-Null
Set-Content -LiteralPath {ps_single_quote(self.repo_root / ".cargo" / "config.toml")} -Value '[build]'
$fingerprints.ancestorConfig = Get-Identity
Set-Content -LiteralPath {ps_single_quote(linker_version)} -Value 'LLD 2.0'
$fingerprints.linkerUpgrade = Get-Identity
$fingerprints | ConvertTo-Json -Compress
"""
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                env=env,
                timeout=RUN_TIMEOUT_SECONDS,
                creationflags=CREATE_NO_WINDOW,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            fingerprints = json.loads(result.stdout)
            for name, fingerprint in fingerprints.items():
                self.assertRegex(str(fingerprint), r"\A[0-9a-f]{64}\Z", name)
            self.assertEqual(fingerprints["otherCwd"], fingerprints["base"])
            for name in compile_inputs:
                with self.subTest(input=name):
                    self.assertNotEqual(fingerprints[name], fingerprints["base"])
            self.assertNotEqual(fingerprints["cargoHomeConfig"], fingerprints["base"])
            self.assertNotEqual(
                fingerprints["ancestorConfig"], fingerprints["cargoHomeConfig"]
            )
            self.assertNotEqual(
                fingerprints["linkerUpgrade"], fingerprints["ancestorConfig"]
            )
            codex_rs = (self.repo_root / "codex-rs").resolve()
            observed_dirs = probe_dirs.read_text(encoding="utf-8").splitlines()
            self.assertTrue(observed_dirs)
            self.assertEqual(
                {Path(line.strip()).resolve() for line in observed_dirs}, {codex_rs}
            )

    def test_build_dirty_flag_describes_publish_inputs(self) -> None:
        # Auto-skip reuses a binary while the publish inputs are unchanged, so
        # its embedded dirty flag must depend on exactly those inputs.
        self.init_repo_fixture()
        repo = ps_single_quote(self.repo_root)
        build_output = self.built_artifact_paths()[0]
        notes = self.repo_root / "docs" / "notes.md"
        tracked = self.repo_root / "codex-rs" / "tracked-source.rs"
        command = rf"""
$ErrorActionPreference = 'Stop'
. {ps_single_quote(SCRIPT)} -ImportOnly
$states = [ordered]@{{}}
$states.clean = Get-GitBuildDirty -RepoRoot {repo}
foreach ($path in @({ps_single_quote(build_output)}, {ps_single_quote(notes)})) {{
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $path) | Out-Null
    Set-Content -LiteralPath $path -Value 'not a publish input'
}}
$states.unrelated = Get-GitBuildDirty -RepoRoot {repo}
Set-Content -LiteralPath {ps_single_quote(tracked)} -Value 'changed'
$states.publishInput = Get-GitBuildDirty -RepoRoot {repo}
$states | ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [self.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            env=clean_env(),
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(
            json.loads(result.stdout),
            {"clean": "false", "unrelated": "false", "publishInput": "true"},
        )


if __name__ == "__main__":
    unittest.main()
