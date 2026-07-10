#!/usr/bin/env python3

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: the justfile invokes these scripts via
    # `powershell -NoProfile -File ...`, so tests should exercise the same
    # host (5.1 has stricter native-stderr and StrictMode semantics).
    return shutil.which("powershell") or shutil.which("pwsh")


def pwsh_only() -> str | None:
    # invoke-rust-perf-env.ps1 runs under pwsh 7.4+ in production (recipes
    # invoke it inline in the just-shell pwsh session), and its -NoSccache
    # proof depends on pwsh's empty-env-var semantics, so its tests must not
    # fall back to Windows PowerShell 5.1.
    return shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def load_just_shell_module():
    path = REPO_ROOT / "scripts" / "just-shell.py"
    spec = importlib.util.spec_from_file_location("just_shell", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_format_module():
    path = REPO_ROOT / "scripts" / "format.py"
    spec = importlib.util.spec_from_file_location("format_script", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_root_maintenance_module():
    path = REPO_ROOT / "scripts" / "root_maintenance.py"
    spec = importlib.util.spec_from_file_location("root_maintenance", path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_toml(path: Path):
    return tomllib.loads(path.read_text(encoding="utf-8"))


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

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_keeps_explicit_cargo_target_dir_argument(self) -> None:
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
        self.assertIn("cargoTargetDir=<explicit command argument>", result.stdout)
        self.assertIn("targetenv=", result.stdout)
        self.assertIn(f"--target-dir {explicit_target}", result.stdout)
        self.assertNotIn("stale-target-env", result.stdout)
        self.assertNotIn("perf-explicit-target", result.stdout)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
    def test_perf_env_rejects_dot_path_lane_names(self) -> None:
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
                    f"$programArgs = @({ps_single_quote(shell)}, '-NoProfile', "
                    "'-Command', 'exit 0'); "
                    f"& {ps_single_quote(script)} -CargoTargetLane '..' "
                    f"-WorkingDirectory {ps_single_quote(REPO_ROOT)} "
                    "-ProgramArgs $programArgs"
                ),
            ],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cargo target lane", result.stderr)

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
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

    @unittest.skipUnless(os.name == "nt", "invoke-rust-perf-env is Windows-only")
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
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    @unittest.skipUnless(
        os.name == "nt", "common-rust-env sccache restart is Windows-only"
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
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
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

    @unittest.skipUnless(os.name == "nt", "common-rust-env is Windows-only")
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

    @unittest.skipUnless(os.name == "nt", "sccache-perf is Windows-only")
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
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
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

    @unittest.skipUnless(os.name == "nt", "sccache-perf is Windows-only")
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
                        'echo %*>>"%FAKE_SCCACHE_CALLS%"',
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

    def test_justfile_bench_and_bazel_fast_paths_are_explicit(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertIn("bench package bench_name *args:", justfile)
        self.assertIn("bench-workspace *args:", justfile)
        self.assertIn("Pass explicit Bazel test targets.", justfile)
        self.assertIn("target-optimize-dry-run *args:", justfile)
        self.assertIn("app-server-runtime-check:", justfile)
        self.assertIn("app-server-command-exec-check:", justfile)
        self.assertIn("app-server-process-exec-check:", justfile)
        self.assertIn("app-server-thread-status-check:", justfile)
        self.assertIn("app-server-schema-protocol-check:", justfile)
        self.assertIn("app-server-schema-check:", justfile)
        self.assertIn("app-server-schema-check-force:", justfile)
        self.assertIn("cargo nextest run -p codex-app-server-protocol -E", justfile)

    def test_agents_current_nested_instruction_layout_is_explicit(
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
            path.relative_to(REPO_ROOT).as_posix()
            for path in REPO_ROOT.rglob("AGENTS.md")
            if ".git" not in path.parts
        )
        root_text = (REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        normalized_root = " ".join(root_text.split())

        self.assertEqual(actual_agent_files, sorted(expected_agent_files))
        self.assertIn("further nested files apply only where present", normalized_root)
        self.assertIn(
            "Never rely on an instruction file that is absent", normalized_root
        )


if __name__ == "__main__":
    unittest.main()
