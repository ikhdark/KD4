#!/usr/bin/env python3

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.build_tooling_test_support import (
    REPO_ROOT,
    powershell,
    ps_single_quote,
    pwsh_only,
)

CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class BuildToolingPerformanceTest(unittest.TestCase):
    def test_perf_env_no_sccache_leaves_incremental_and_uses_lane_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        env = os.environ.copy()
        env["CARGO_INCREMENTAL"] = "keep"
        env["RUSTC_WRAPPER"] = "existing-wrapper"
        env["SCCACHE_BASEDIR"] = "stale"
        env["SCCACHE_CACHE_SIZE"] = "stale"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    f"$programArgs = @({ps_single_quote(shell)}, '-NoProfile', "
                    "'-Command', 'exit 7'); "
                    f"& {ps_single_quote(script)} -NoSccache "
                    "-CargoTargetLane 'perf nextest/nosccache' "
                    f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                    "-ProgramArgs $programArgs; "
                    "exit $LASTEXITCODE"
                ),
            ],
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            env=env,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            7,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("rustPerfEnv:", result.stdout)
        self.assertIn("cargoIncremental=keep", result.stdout)
        self.assertIn("rustcWrapper=<empty>", result.stdout)
        self.assertIn("sccacheBaseDir=<unset>", result.stdout)
        self.assertIn("cargoTargetDir=", result.stdout)
        self.assertIn("perf-nextest-nosccache", result.stdout)

    def test_perf_env_rejects_explicit_target_outside_reserved_lane_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "@echo off\r\nif defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=\r\necho cargo-args:%*\r\nexit /b 0\r\n",
                encoding="utf-8",
            )
            explicit_target = temp_root / "explicit-target"
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = "stale-target-env"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        "$programArgs = @('cargo', 'check', '--target-dir', "
                        f"{ps_single_quote(explicit_target)}); "
                        f"& {ps_single_quote(script)} "
                        "-CargoTargetLane 'perf explicit target' "
                        f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                        "-ProgramArgs $programArgs; "
                        "exit $LASTEXITCODE"
                    ),
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cargoTargetDir=<explicit command argument>", result.stdout)
        self.assertIn("does not match reserved lane target", result.stderr)
        self.assertNotIn("targetenv=", result.stdout)
        self.assertNotIn("stale-target-env", result.stdout)

    def test_perf_env_rejects_dot_path_lane_names_through_script_process(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        for lane in ("..", "..."):
            with self.subTest(lane=lane):
                result = subprocess.run(
                    [
                        shell,
                        "-NoProfile",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-Command",
                        (
                            f"$programArgs = @({ps_single_quote(shell)}, '-NoProfile', "
                            "'-Command', 'exit 0'); "
                            f"& {ps_single_quote(script)} "
                            f"-CargoTargetLane {ps_single_quote(lane)} "
                            f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                            "-ProgramArgs $programArgs"
                        ),
                    ],
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    capture_output=True,
                    check=False,
                    creationflags=CREATE_NO_WINDOW,
                )

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Cargo target lane", result.stderr)

    def test_perf_env_keeps_same_length_cargo_watch_rewrite_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "@echo off\r\nif defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=\r\necho cargo-args:%*\r\nexit /b 0\r\n",
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["CARGO_TARGET_DIR"] = "stale-target-env"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        "$programArgs = @('cargo', 'watch', '-x', "
                        "'test -- --nocapture'); "
                        f"& {ps_single_quote(script)} "
                        "-CargoTargetLane 'perf watch' "
                        f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                        "-ProgramArgs $programArgs; exit $LASTEXITCODE"
                    ),
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("targetenv=", result.stdout)
        self.assertNotIn("targetenv=stale-target-env", result.stdout)
        self.assertIn("--target-dir", result.stdout)
        self.assertIn(" -- --nocapture", result.stdout)

    def test_perf_env_non_native_success_ignores_stale_last_exit_code_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    "function Invoke-TestSuccess { 'ok' | Out-Null }; "
                    "$global:LASTEXITCODE = 99; "
                    f". {ps_single_quote(script)} -ProgramArgs @('Invoke-TestSuccess')"
                ),
            ],
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def test_perf_env_non_native_failure_returns_nonzero_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    f". {ps_single_quote(script)} "
                    "-ProgramArgs @('Get-Item', 'Z:\\missing-kd4-path', "
                    "'-ErrorAction', 'Continue')"
                ),
            ],
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            1,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def test_perf_env_restores_empty_environment_variable_through_script_process(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        command = (
            "[Environment]::SetEnvironmentVariable("
            "'SCCACHE_BASEDIR', '', [EnvironmentVariableTarget]::Process); "
            f"& {ps_single_quote(script)} -NoSccache "
            f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
            f"-ProgramArgs @({ps_single_quote(shell)}, '-NoProfile', "
            "'-Command', 'exit 0'); "
            "$scriptExit = $LASTEXITCODE; "
            "if ($scriptExit -ne 0) { exit $scriptExit }; "
            "if (-not (Test-Path Env:SCCACHE_BASEDIR) -or "
            "$env:SCCACHE_BASEDIR -ne '') { exit 23 }; "
            "Write-Output 'restoredEmpty=True'"
        )

        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("restoredEmpty=True", result.stdout)

    def test_sccache_stats_restarts_stale_server_through_script_process(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            stats = temp_root / "sccache-stats.txt"
            stats.write_text(
                "Max cache size                       10 GiB\r\n",
                encoding="utf-8",
            )
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\r\nif "%1"=="--show-stats" (\r\n  type "%FAKE_SCCACHE_STATS%"\r\n  exit /b 0\r\n)\r\nif "%1"=="--stop-server" exit /b 0\r\nif "%1"=="--start-server" (\r\n  >"%FAKE_SCCACHE_STATS%" echo Max cache size                       80 GiB\r\n  exit /b 0\r\n)\r\nexit /b 0\r\n',
                encoding="utf-8",
            )

            env = os.environ.copy()
            env.pop("CODEX_SCCACHE_CACHE_SIZE", None)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            env["FAKE_SCCACHE_STATS"] = str(stats)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "stats",
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            call_lines = calls.read_text(encoding="utf-8").splitlines()
            self.assertEqual(
                call_lines,
                [
                    "--show-stats",
                    "--stop-server",
                    "--start-server",
                    "--show-stats",
                    "--show-stats",
                ],
            )
            self.assertIn("80 GiB", stats.read_text(encoding="utf-8"))

    def test_sccache_stats_honors_cache_size_override_through_script_process(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            observations = temp_root / "sccache-env.txt"
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_ENV%" echo cacheSize=%SCCACHE_CACHE_SIZE%\r\nexit /b 0\r\n',
                encoding="utf-8",
            )

            for override, expected in ((" 100G ", "100G"), ("   ", "80G")):
                with self.subTest(override=override):
                    observations.unlink(missing_ok=True)
                    env = os.environ.copy()
                    env["CODEX_SCCACHE_CACHE_SIZE"] = override
                    env["FAKE_SCCACHE_ENV"] = str(observations)
                    env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
                    result = subprocess.run(
                        [
                            shell,
                            "-NoProfile",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-File",
                            str(script),
                            "stats",
                        ],
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        capture_output=True,
                        check=False,
                        env=env,
                        creationflags=CREATE_NO_WINDOW,
                    )

                    self.assertEqual(
                        result.returncode,
                        0,
                        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                    )
                    observed = observations.read_text(encoding="utf-8").splitlines()
                    self.assertGreaterEqual(len(observed), 2)
                    self.assertEqual(set(observed), {f"cacheSize={expected}"})

    def test_sccache_stats_compares_cache_sizes_by_bytes_through_script_process(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        script = REPO_ROOT / "scripts" / "sccache-perf.ps1"
        cases = (
            ("80GB", "80 GiB", False),
            ("80g", "80 GiB", False),
            ("500M", "500 MiB", False),
            ("1T", "1 TiB", False),
            ("1024G", "1 TiB", False),
            ("80G", "10 GiB", True),
            ("vendor-format", "80 GiB", False),
        )

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            stats = temp_root / "sccache-stats.txt"
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\r\nif "%1"=="--show-stats" (\r\n  type "%FAKE_SCCACHE_STATS%"\r\n  exit /b 0\r\n)\r\nif "%1"=="--stop-server" exit /b 0\r\nif "%1"=="--start-server" (\r\n  >"%FAKE_SCCACHE_STATS%" echo Max cache size                       80 GiB\r\n  exit /b 0\r\n)\r\nexit /b 0\r\n',
                encoding="utf-8",
            )

            for expected_size, actual_size, should_restart in cases:
                with self.subTest(
                    expected_size=expected_size,
                    actual_size=actual_size,
                ):
                    calls.unlink(missing_ok=True)
                    stats.write_text(
                        f"Max cache size                       {actual_size}\r\n",
                        encoding="utf-8",
                    )
                    env = os.environ.copy()
                    env["CODEX_SCCACHE_CACHE_SIZE"] = expected_size
                    env["FAKE_SCCACHE_CALLS"] = str(calls)
                    env["FAKE_SCCACHE_STATS"] = str(stats)
                    env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
                    result = subprocess.run(
                        [
                            shell,
                            "-NoProfile",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-File",
                            str(script),
                            "stats",
                        ],
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        capture_output=True,
                        check=False,
                        env=env,
                        creationflags=CREATE_NO_WINDOW,
                    )

                    self.assertEqual(
                        result.returncode,
                        0,
                        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                    )
                    observed = calls.read_text(encoding="utf-8").splitlines()
                    if should_restart:
                        self.assertEqual(
                            observed,
                            [
                                "--show-stats",
                                "--stop-server",
                                "--start-server",
                                "--show-stats",
                                "--show-stats",
                            ],
                        )
                    else:
                        self.assertEqual(observed, ["--show-stats", "--show-stats"])

    def test_sccache_restart_ignores_stop_failure_and_checks_start_through_script_process(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\r\nif "%1"=="--stop-server" exit /b 7\r\nif "%1"=="--start-server" exit /b 0\r\nif "%1"=="--show-stats" (\r\n  echo Max cache size                       80 GiB\r\n  exit /b 0\r\n)\r\nexit /b 0\r\n',
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "restart",
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )
            call_lines = (
                calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("Max cache size", result.stdout)
        self.assertEqual(
            call_lines,
            ["--stop-server", "--start-server", "--show-stats"],
        )

    def test_sccache_reset_reports_zero_stats_failure_through_script_process(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\r\nif "%1"=="--show-stats" (\r\n  echo Max cache size                       80 GiB\r\n  exit /b 0\r\n)\r\nif "%1"=="--zero-stats" exit /b 9\r\nexit /b 0\r\n',
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "reset",
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )
            call_lines = (
                calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sccache --zero-stats failed with exit code 9", result.stderr)
        self.assertEqual(
            call_lines,
            ["--show-stats", "--zero-stats"],
        )

    def test_sccache_reset_reports_command_removed_after_lookup_through_script_process(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                '@echo off\r\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\r\nif "%1"=="--show-stats" (\r\n  echo Max cache size                       80 GiB\r\n  del "%~f0"\r\n  exit /b 0\r\n)\r\nexit /b 0\r\n',
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            script = REPO_ROOT / "scripts" / "sccache-perf.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(script),
                    "reset",
                ],
                text=True,
                encoding="utf-8",
                errors="replace",
                capture_output=True,
                check=False,
                env=env,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sccache --zero-stats failed to launch", result.stderr)

    def test_just_cli_reaches_bench_and_validation_fast_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "cargo-calls.txt"
            (fake_bin / "cargo.cmd").write_text(
                '@echo off\r\n>>"%FAKE_CARGO_CALLS%" echo(%*\r\nexit /b 0\r\n',
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["FAKE_CARGO_CALLS"] = str(calls)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"

            invocations = (
                ["bench", "fixture-package", "fixture-bench", "--sample-size", "1"],
                ["bench-workspace", "--sample-size", "1"],
                ["build-for-release", "--locked"],
                ["app-server-runtime-check"],
                ["app-server-command-exec-check"],
                ["app-server-process-exec-check"],
                ["app-server-thread-status-check"],
                ["app-server-schema-protocol-check"],
            )
            for invocation in invocations:
                with self.subTest(invocation=invocation):
                    result = subprocess.run(
                        ["just", *invocation],
                        cwd=REPO_ROOT,
                        env=env,
                        text=True,
                        encoding="utf-8",
                        errors="replace",
                        capture_output=True,
                        check=False,
                        creationflags=CREATE_NO_WINDOW,
                    )
                    self.assertEqual(
                        result.returncode,
                        0,
                        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                    )

            routed = calls.read_text(encoding="utf-8")
            self.assertIn(
                "bench -p fixture-package --bench fixture-bench --sample-size 1",
                routed,
            )
            self.assertIn("bench --workspace --bench * --sample-size 1", routed)
            self.assertIn("build --release --locked", routed)
            self.assertIn("check -p codex-app-server", routed)
            self.assertIn("nextest run -p codex-app-server-protocol -E", routed)

        for invocation, expected in (
            (
                ["target-optimize-dry-run"],
                'scripts/rust_build_status.py" optimize --dry-run',
            ),
            (
                ["app-server-schema-check"],
                'scripts/app_server_schema_runtime_check.py" --mode check',
            ),
            (
                ["app-server-schema-regenerate", "fixture-owner"],
                (
                    'scripts/app_server_schema_runtime_check.py" --mode force '
                    '--owner "fixture-owner"'
                ),
            ),
        ):
            with self.subTest(dry_run=invocation):
                result = subprocess.run(
                    ["just", "--dry-run", *invocation],
                    cwd=REPO_ROOT,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    capture_output=True,
                    check=False,
                    creationflags=CREATE_NO_WINDOW,
                )
                self.assertEqual(
                    result.returncode,
                    0,
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                )
                self.assertIn(expected, result.stdout + result.stderr)

    def test_repository_policy_layout_is_reachable_through_source_map_validation(
        self,
    ) -> None:
        validation = subprocess.run(
            ["just", "source-map-check"],
            cwd=REPO_ROOT,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(
            validation.returncode,
            0,
            f"stdout:\n{validation.stdout}\nstderr:\n{validation.stderr}",
        )
        expected_agent_files = ["AGENTS.md"]
        discovered_agent_files = subprocess.run(
            [
                "git",
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                ":(glob)**/AGENTS.md",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=True,
            creationflags=CREATE_NO_WINDOW,
        ).stdout.splitlines()
        actual_agent_files = sorted(
            path for path in discovered_agent_files if (REPO_ROOT / path).is_file()
        )
        actual_eol_attributes = subprocess.run(
            ["git", "check-attr", "eol", "--", *expected_agent_files],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=True,
            creationflags=CREATE_NO_WINDOW,
        ).stdout.splitlines()
        expected_eol_attributes = [f"{path}: eol: lf" for path in expected_agent_files]
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized_root = " ".join(root_text.split())

        self.assertEqual(actual_agent_files, sorted(expected_agent_files))
        self.assertEqual(actual_eol_attributes, expected_eol_attributes)
        self.assertIn(
            "Read the root `AGENTS.md` in full",
            normalized_root,
        )
        source_map = REPO_ROOT / "SOURCEMAP.md"
        root_policy_bytes = (REPO_ROOT / "AGENTS.md").stat().st_size
        source_map_text = source_map.read_text(encoding="utf-8")

        self.assertLessEqual(
            root_policy_bytes,
            16 * 1024,
            "the root automatic instruction file is too large",
        )
        self.assertIn("## Validation routes", source_map_text)
        self.assertIn("## Rust workflow reference", source_map_text)
        self.assertIn("just rust-build-doctor", source_map_text)


if __name__ == "__main__":
    unittest.main()
