#!/usr/bin/env python3

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import powershell
from scripts.build_tooling_test_support import ps_single_quote
from scripts.build_tooling_test_support import pwsh_only


CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class BuildToolingPerformanceTest(unittest.TestCase):
    def test_perf_env_no_sccache_leaves_incremental_and_uses_lane(self) -> None:
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

    def test_perf_env_rejects_explicit_target_outside_reserved_lane(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "if defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=",
                        "echo cargo-args:%*",
                        "exit /b 0",
                        "",
                    ]
                ),
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

    def test_perf_env_rejects_dot_path_lane_names(self) -> None:
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

    def test_perf_env_keeps_same_length_cargo_watch_rewrite(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            (fake_bin / "cargo.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        "if defined CARGO_TARGET_DIR (echo targetenv=%CARGO_TARGET_DIR%) else echo targetenv=",
                        "echo cargo-args:%*",
                        "exit /b 0",
                        "",
                    ]
                ),
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

    def test_perf_env_non_native_success_does_not_use_stale_last_exit_code(
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

    def test_perf_env_non_native_failure_returns_nonzero(self) -> None:
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

    def test_perf_env_restore_helper_preserves_empty_environment_variable(
        self,
    ) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        command = (
            "$tokens = $null; $errors = $null; "
            f"$ast = [System.Management.Automation.Language.Parser]::ParseFile("
            f"{ps_single_quote(script)}, [ref]$tokens, [ref]$errors); "
            "$function = $ast.Find({ param($node) "
            "$node -is [System.Management.Automation.Language.FunctionDefinitionAst] "
            "-and $node.Name -eq 'Restore-ProcessEnvironmentVariable' }, $true); "
            "Invoke-Expression $function.Extent.Text; "
            "[Environment]::SetEnvironmentVariable("
            "'KD4_EMPTY_RESTORE_TEST', '', [EnvironmentVariableTarget]::Process); "
            "$old = [Environment]::GetEnvironmentVariable("
            "'KD4_EMPTY_RESTORE_TEST', 'Process'); "
            "$had = Test-Path Env:KD4_EMPTY_RESTORE_TEST; "
            "Remove-Item Env:KD4_EMPTY_RESTORE_TEST; "
            "Restore-ProcessEnvironmentVariable "
            "-Name 'KD4_EMPTY_RESTORE_TEST' -Value $old -WasSet $had; "
            "if (-not (Test-Path Env:KD4_EMPTY_RESTORE_TEST) -or "
            "$env:KD4_EMPTY_RESTORE_TEST -ne '') { exit 1 }"
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

    def test_common_rust_env_restarts_stale_sccache_server_cache_size(self) -> None:
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
                "\r\n".join(
                    [
                        "@echo off",
                        '>>"%FAKE_SCCACHE_CALLS%" echo(%*',
                        'if "%1"=="--show-stats" (',
                        '  type "%FAKE_SCCACHE_STATS%"',
                        "  exit /b 0",
                        ")",
                        'if "%1"=="--stop-server" exit /b 0',
                        'if "%1"=="--start-server" (',
                        '  >"%FAKE_SCCACHE_STATS%" echo Max cache size                       80 GiB',
                        "  exit /b 0",
                        ")",
                        "exit /b 0",
                        "",
                    ]
                ),
                encoding="utf-8",
            )

            env = os.environ.copy()
            env.pop("CODEX_SCCACHE_CACHE_SIZE", None)
            env["PATH"] = f"{fake_bin}{os.pathsep}{env['PATH']}"
            env["FAKE_SCCACHE_CALLS"] = str(calls)
            env["FAKE_SCCACHE_STATS"] = str(stats)
            script = REPO_ROOT / "scripts" / "common-rust-env.ps1"

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        # Mirror the production session: cargo-lane.ps1
                        # dot-sources this helper under StrictMode Latest
                        # with $ErrorActionPreference = "Stop".
                        "Set-StrictMode -Version Latest; "
                        "$ErrorActionPreference = 'Stop'; "
                        f". {ps_single_quote(script)}; "
                        f"Ensure-CodexRustSccacheServer -RepoRoot {ps_single_quote(REPO_ROOT)}; "
                        'Write-Output "cacheSize=$env:SCCACHE_CACHE_SIZE"'
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
            self.assertIn("cacheSize=80G", result.stdout)
            call_text = calls.read_text(encoding="utf-8")
            self.assertIn("--show-stats", call_text)
            self.assertIn("--stop-server", call_text)
            self.assertIn("--start-server", call_text)
            self.assertIn("80 GiB", stats.read_text(encoding="utf-8"))

    def test_common_rust_env_cache_size_honors_override(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        script = REPO_ROOT / "scripts" / "common-rust-env.ps1"
        command = (
            "Set-StrictMode -Version Latest; "
            "$ErrorActionPreference = 'Stop'; "
            f". {ps_single_quote(script)}; "
            'Write-Output "cacheSize=$(Get-CodexRustSccacheCacheSize)"'
        )

        for override, expected in (
            (" 100G ", "cacheSize=100G"),
            ("   ", "cacheSize=80G"),
        ):
            with self.subTest(override=override):
                env = os.environ.copy()
                env["CODEX_SCCACHE_CACHE_SIZE"] = override
                result = subprocess.run(
                    [
                        shell,
                        "-NoProfile",
                        "-ExecutionPolicy",
                        "Bypass",
                        "-Command",
                        command,
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
                self.assertIn(expected, result.stdout)

    def test_common_rust_env_compares_cache_sizes_by_bytes(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        script = REPO_ROOT / "scripts" / "common-rust-env.ps1"
        command = (
            "Set-StrictMode -Version Latest; "
            "$ErrorActionPreference = 'Stop'; "
            f". {ps_single_quote(script)}; "
            "$cases = @("
            "@('80GB', '80 GiB', $true), "
            "@('80g', '80 GiB', $true), "
            "@('500M', '500 MiB', $true), "
            "@('1T', '1 TiB', $true), "
            "@('1024G', '1 TiB', $true), "
            "@('80G', '10 GiB', $false), "
            "@('vendor-format', '80 GiB', $true)"
            "); "
            "foreach ($case in $cases) { "
            "$env:CODEX_SCCACHE_CACHE_SIZE = $case[0]; "
            "$actual = Test-CodexRustSccacheStatsCacheSize "
            "-Stats @('Max cache size                       ' + $case[1]); "
            "Write-Output ($actual -eq $case[2]) "
            "}"
        )

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
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
        self.assertEqual(
            [line.strip() for line in result.stdout.splitlines() if line.strip()],
            ["True"] * 7,
        )

    def test_sccache_perf_restart_ignores_stop_failure_and_checks_start(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        '>>"%FAKE_SCCACHE_CALLS%" echo(%*',
                        'if "%1"=="--stop-server" exit /b 7',
                        'if "%1"=="--start-server" exit /b 0',
                        'if "%1"=="--show-stats" (',
                        "  echo Max cache size                       80 GiB",
                        "  exit /b 0",
                        ")",
                        "exit /b 0",
                        "",
                    ]
                ),
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

    def test_sccache_perf_reset_fails_when_zero_stats_fails(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        '>>"%FAKE_SCCACHE_CALLS%" echo(%*',
                        'if "%1"=="--show-stats" (',
                        "  echo Max cache size                       80 GiB",
                        "  exit /b 0",
                        ")",
                        'if "%1"=="--zero-stats" exit /b 9',
                        "exit /b 0",
                        "",
                    ]
                ),
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

    def test_sccache_perf_reports_command_removed_after_lookup(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as tempdir:
            temp_root = Path(tempdir)
            fake_bin = temp_root / "bin"
            fake_bin.mkdir()
            calls = temp_root / "sccache-calls.txt"
            (fake_bin / "sccache.cmd").write_text(
                "\r\n".join(
                    [
                        "@echo off",
                        '>>"%FAKE_SCCACHE_CALLS%" echo(%*',
                        'if "%1"=="--show-stats" (',
                        "  echo Max cache size                       80 GiB",
                        '  del "%~f0"',
                        "  exit /b 0",
                        ")",
                        "exit /b 0",
                        "",
                    ]
                ),
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

    def test_justfile_bench_and_validation_fast_paths_are_explicit(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("bench package bench_name *args:", justfile)
        self.assertIn("bench-workspace *args:", justfile)
        self.assertIn("build-for-release *args:", justfile)
        self.assertIn("target-optimize-dry-run *args:", justfile)
        self.assertIn("app-server-runtime-check:", justfile)
        self.assertIn("app-server-command-exec-check:", justfile)
        self.assertIn("app-server-process-exec-check:", justfile)
        self.assertIn("app-server-thread-status-check:", justfile)
        self.assertIn("app-server-schema-protocol-check:", justfile)
        self.assertIn("app-server-schema-check:", justfile)
        self.assertIn('app-server-schema-regenerate owner experimental="":', justfile)
        self.assertIn("cargo nextest run -p codex-app-server-protocol -E", justfile)

    def test_agents_current_nested_instruction_layout_and_budget_are_explicit(
        self,
    ) -> None:
        expected_agent_files = [
            ".codex/AGENTS.md",
            "AGENTS.md",
            "codex-rs/AGENTS.md",
            "codex-rs/core/AGENTS.md",
            "codex-rs/prompts/AGENTS.md",
            "codex-rs/protocol/AGENTS.md",
            "codex-rs/shell-command/AGENTS.md",
            "codex-rs/tui/src/bottom_pane/AGENTS.md",
            "scripts/AGENTS.md",
            "scripts/codex_package/AGENTS.md",
            "scripts/install/AGENTS.md",
        ]
        actual_agent_files = sorted(
            subprocess.run(
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
        workspace_text = (REPO_ROOT / ".codex" / "AGENTS.md").read_text(
            encoding="utf-8"
        )

        self.assertEqual(actual_agent_files, sorted(expected_agent_files))
        self.assertEqual(actual_eol_attributes, expected_eol_attributes)
        self.assertIn(
            "Read every applicable `AGENTS.md` from the repository root through "
            "each path touched",
            normalized_root,
        )
        self.assertIn("`.codex/AGENTS.md` covers workspace routing", normalized_root)
        self.assertIn("The root `AGENTS.md` still applies", workspace_text)
        rust_parent = REPO_ROOT / "codex-rs" / "AGENTS.md"
        core_policy = REPO_ROOT / "codex-rs" / "core" / "AGENTS.md"
        source_map = REPO_ROOT / "SOURCEMAP.md"
        rust_instruction_chain = [
            REPO_ROOT / "AGENTS.md",
            rust_parent,
            core_policy,
        ]
        rust_parent_bytes = rust_instruction_chain[1].stat().st_size
        rust_chain_bytes = sum(path.stat().st_size for path in rust_instruction_chain)
        rust_parent_text = rust_parent.read_text(encoding="utf-8")
        core_policy_text = core_policy.read_text(encoding="utf-8")
        source_map_text = source_map.read_text(encoding="utf-8")

        self.assertLessEqual(
            rust_parent_bytes,
            4 * 1024,
            "codex-rs/AGENTS.md should keep detailed routing in SOURCEMAP.md",
        )
        self.assertLessEqual(
            rust_chain_bytes,
            16 * 1024,
            "the root + codex-rs + core automatic instruction chain is too large",
        )
        self.assertIn("[`../SOURCEMAP.md`](../SOURCEMAP.md)", rust_parent_text)
        self.assertIn("[`../../SOURCEMAP.md`](../../SOURCEMAP.md)", core_policy_text)
        self.assertEqual((rust_parent.parent / "../SOURCEMAP.md").resolve(), source_map)
        self.assertEqual(
            (core_policy.parent / "../../SOURCEMAP.md").resolve(), source_map
        )
        self.assertIn("## Validation routes", source_map_text)
        self.assertIn("## Rust workflow reference", source_map_text)
        self.assertIn("just rust-build-doctor", source_map_text)
        self.assertIn("Tool-search breadth changes must preserve", core_policy_text)
        self.assertIn("core_test_support::responses", core_policy_text)


if __name__ == "__main__":
    unittest.main()
