#!/usr/bin/env python3

import os
import json
import sys
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.build_tooling_test_support import REPO_ROOT
from scripts.build_tooling_test_support import load_toml
from scripts.build_tooling_test_support import powershell
from scripts.build_tooling_test_support import ps_single_quote
from scripts.build_tooling_test_support import pwsh_only


CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class BuildToolingPerformanceTest(unittest.TestCase):
    def setUp(self):
        lane_root = self.enterContext(tempfile.TemporaryDirectory())
        self.enterContext(
            mock.patch.dict(
                os.environ,
                {
                    "CODEX_CARGO_LANES_ROOT": lane_root,
                    # These environment tests do not exercise lane maintenance.
                    "CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE": "1",
                },
            )
        )

    def test_perf_env_disables_incremental_independently_of_wrapper(self):
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        for wrapper, incremental, exit_code in (
            (None, None, 0),
            ("", "1", 0),
            ("custom-wrapper", "", 7),
        ):
            with self.subTest(wrapper=wrapper, incremental=incremental):
                env = os.environ.copy()
                # Use absolute executables, with no sccache discoverable on PATH.
                env["PATH"] = ""
                setup = []
                names = {"RUSTC_WRAPPER": wrapper, "CARGO_INCREMENTAL": incremental}
                for name, value in names.items():
                    setup.append(
                        f"Remove-Item Env:{name} -ErrorAction SilentlyContinue"
                        if value is None
                        else f"$env:{name} = {ps_single_quote(value)}"
                    )
                child = (
                    "import json,os,sys; print('CHILD='+json.dumps({k:os.environ.get(k) "
                    f"for k in {list(names)!r}}})); sys.exit({exit_code})"
                )
                entries = "; ".join(
                    f"{name}=[Environment]::GetEnvironmentVariable('{name}')"
                    for name in names
                )
                command = (
                    "; ".join(setup)
                    + f"; & {ps_single_quote(script)} -ProgramArgs @("
                    + f"{ps_single_quote(sys.executable)}, '-c', {ps_single_quote(child)}); "
                    + "$childExit = $LASTEXITCODE; 'RESTORED=' + (@{"
                    + entries
                    + "} | ConvertTo-Json -Compress); exit $childExit"
                )
                result = subprocess.run(
                    [shell, "-NoProfile", "-Command", command],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    creationflags=CREATE_NO_WINDOW,
                )
                self.assertEqual(result.returncode, exit_code, result.stderr)
                proofs = {
                    line.split("=", 1)[0]: json.loads(line.split("=", 1)[1])
                    for line in result.stdout.splitlines()
                    if line.startswith(("CHILD=", "RESTORED="))
                }
                self.assertEqual(proofs["RESTORED"], names)
                self.assertEqual(
                    proofs["CHILD"],
                    {"RUSTC_WRAPPER": wrapper, "CARGO_INCREMENTAL": "0"},
                )

    def test_perf_env_no_sccache_disables_incremental_and_uses_lane(self) -> None:
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        env = os.environ.copy()
        env["CARGO_INCREMENTAL"] = "keep"
        env["RUSTC_WRAPPER"] = "existing-wrapper"
        env["SCCACHE_BASEDIRS"] = "stale"
        env["SCCACHE_CACHE_SIZE"] = "stale"
        child = (
            "import json,os,sys; from pathlib import Path; "
            "from scripts import rust_build_status; "
            "target=Path(os.environ['CODEX_CARGO_LANE_TARGET_DIR']); "
            "assert rust_build_status._binary_file_lock_is_busy(target/'.lane-active.lock'); "
            "print('CHILD='+json.dumps({k:os.environ.get(k) for k in "
            "['CODEX_CARGO_LANE_TARGET_DIR','CARGO_INCREMENTAL','RUSTC_WRAPPER',"
            "'SCCACHE_BASEDIRS']})); sys.exit(7)"
        )

        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                (
                    f"$programArgs = @({ps_single_quote(sys.executable)}, '-c', "
                    f"{ps_single_quote(child)}); "
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
            timeout=30,
        )

        self.assertEqual(
            result.returncode,
            7,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("rustPerfEnv:", result.stdout)
        self.assertIn("cargoIncremental=0", result.stdout)
        self.assertIn("rustcWrapper=<empty>", result.stdout)
        self.assertIn("sccacheBaseDir=<unset>", result.stdout)
        # run-lane resolves the directory; the proof must not guess a path.
        self.assertIn(
            "cargoTargetDir=<run-lane reservation for perf-nextest-nosccache>",
            result.stdout,
        )
        proof = json.loads(
            next(line for line in result.stdout.splitlines() if line.startswith("CHILD="))
            .removeprefix("CHILD=")
        )
        self.assertEqual(
            Path(proof.pop("CODEX_CARGO_LANE_TARGET_DIR")),
            Path(env["CODEX_CARGO_LANES_ROOT"]) / "perf-nextest-nosccache",
        )
        self.assertEqual(
            proof,
            {"CARGO_INCREMENTAL": "0", "RUSTC_WRAPPER": "", "SCCACHE_BASEDIRS": None},
        )

    def test_no_sccache_isolates_child_and_restores_workspace_wrapper(self):
        shell = pwsh_only()
        if shell is None:
            self.skipTest("pwsh is not available")
        script = REPO_ROOT / "scripts" / "invoke-rust-perf-env.ps1"
        names = [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_INCREMENTAL",
            "SCCACHE_BASEDIRS",
            "SCCACHE_CACHE_SIZE",
        ]
        cases = [
            (wrapper, code) for wrapper in (None, "", "sccache") for code in (0, 7)
        ]
        commands = []
        for workspace_wrapper, exit_code in cases:
            # Set this inside PowerShell to preserve absent versus empty.
            setup = (
                "Remove-Item Env:RUSTC_WORKSPACE_WRAPPER -ErrorAction SilentlyContinue"
                if workspace_wrapper is None
                else "$env:RUSTC_WORKSPACE_WRAPPER = "
                + ps_single_quote(workspace_wrapper)
            )
            child = (
                "import json,os,sys; print('CHILD='+json.dumps({k:os.environ.get(k) "
                f"for k in {names!r}}})); sys.exit({exit_code})"
            )
            entries = "; ".join(
                name + "=[Environment]::GetEnvironmentVariable('" + name + "')"
                for name in names
            )
            commands.append(
                setup
                + "; & "
                + ps_single_quote(script)
                + " -NoSccache -ProgramArgs @("
                + ps_single_quote(sys.executable)
                + ", '-c', "
                + ps_single_quote(child)
                + "); 'EXIT=' + $LASTEXITCODE; 'RESTORED=' + (@{"
                + entries
                + "} | ConvertTo-Json -Compress)"
            )
        env = os.environ.copy()
        env.update(dict.fromkeys(names, "inherited"))
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", "; ".join(commands) + "; exit 0"],
            check=False,
            env=env,
            capture_output=True,
            text=True,
            timeout=30,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        proofs = [
            json.loads(line.split("=", 1)[1])
            for line in result.stdout.splitlines()
            if line.startswith(("CHILD=", "EXIT=", "RESTORED="))
        ]
        self.assertEqual(len(proofs), 3 * len(cases), result.stdout)
        for index, (workspace_wrapper, exit_code) in enumerate(cases):
            with self.subTest(wrapper=workspace_wrapper, exit_code=exit_code):
                child, code, restored = proofs[index * 3 : index * 3 + 3]
                self.assertEqual(code, exit_code)
                self.assertEqual(
                    child,
                    {
                        "RUSTC_WRAPPER": "",
                        "RUSTC_WORKSPACE_WRAPPER": "",
                        "CARGO_INCREMENTAL": "0",
                        "SCCACHE_BASEDIRS": None,
                        "SCCACHE_CACHE_SIZE": None,
                    },
                )
                expected = dict.fromkeys(names, "inherited")
                expected["RUSTC_WORKSPACE_WRAPPER"] = workspace_wrapper
                self.assertEqual(restored, expected)
        self.assertEqual(
            result.stdout.count("rustcWorkspaceWrapper=<empty>"), len(cases)
        )

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
                        f"-WorkingDirectory {ps_single_quote(REPO_ROOT / 'codex-rs')} "
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
                timeout=30,
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
                    timeout=30,
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
                timeout=30,
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
            timeout=30,
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
            timeout=30,
        )

        self.assertEqual(
            result.returncode,
            1,
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
                timeout=30,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertIn("cacheSize=80G", result.stdout)
            self.assertEqual(
                calls.read_text(encoding="utf-8").splitlines(),
                ["--show-stats", "--stop-server", "--start-server", "--show-stats"],
            )
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
                    timeout=30,
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
            timeout=30,
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

    def test_sccache_perf_stats_reports_size_drift_without_restarting(self) -> None:
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
                        "  echo Compile requests                    42",
                        "  echo Max cache size                       10 GiB",
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
                timeout=30,
            )
            call_lines = calls.read_text(encoding="utf-8").splitlines()

        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, output)
        # A restart would zero the counters being reported.
        self.assertEqual(call_lines, ["--show-stats"])
        self.assertIn("Compile requests                    42", result.stdout)
        # The host wraps warnings at console width.
        self.assertIn(
            'run "just sccache-restart" to apply it', " ".join(output.split())
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
                        'if "%1"=="--start-server" exit /b %FAKE_START_CODE%',
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

            for start_code in (0, 9):
                with self.subTest(start_code=start_code):
                    calls.unlink(missing_ok=True)
                    env["FAKE_START_CODE"] = str(start_code)
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
                        timeout=30,
                    )
                    call_lines = calls.read_text(encoding="utf-8").splitlines()
                    if start_code:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn(
                            "sccache --start-server failed with exit code 9",
                            result.stderr,
                        )
                        self.assertEqual(call_lines, ["--stop-server", "--start-server"])
                        self.assertNotIn("Max cache size", result.stdout)
                    else:
                        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                        self.assertIn("Max cache size", result.stdout)
                        self.assertEqual(
                            call_lines, ["--stop-server", "--start-server", "--show-stats"]
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
                timeout=30,
            )
            call_lines = (
                calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sccache --zero-stats failed with exit code 9", result.stderr)
        self.assertEqual(
            call_lines,
            ["--zero-stats"],
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
            (fake_bin / "sccache.ps1").write_text(
                'Add-Content -LiteralPath $env:FAKE_SCCACHE_CALLS -Value ($args -join " ")\n'
                "Remove-Item -LiteralPath $PSCommandPath\n"
                "$global:LASTEXITCODE = 0\n",
                encoding="utf-8",
            )
            env = os.environ.copy()
            env["PATH"] = str(fake_bin)
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
                timeout=30,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sccache --show-stats failed to launch", result.stderr)

    def test_sccache_perf_reset_does_not_restart_mismatched_server(self):
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            calls = root / "calls.txt"
            (root / "sccache.cmd").write_text(
                '@echo off\n>>"%FAKE_SCCACHE_CALLS%" echo(%*\n'
                'if "%1"=="--show-stats" echo Max cache size 10 GiB\n'
                "exit /b 0\n",
                encoding="utf-8",
            )
            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(REPO_ROOT / "scripts" / "sccache-perf.ps1"),
                    "reset",
                ],
                env={
                    **os.environ,
                    "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                    "FAKE_SCCACHE_CALLS": str(calls),
                    "CODEX_SCCACHE_CACHE_SIZE": "80G",
                },
                text=True,
                capture_output=True,
                timeout=30,
                creationflags=CREATE_NO_WINDOW,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                calls.read_text().splitlines(), ["--zero-stats", "--show-stats"]
            )
            self.assertIn("Max cache size 10 GiB", result.stdout)

    def test_justfile_bench_and_validation_fast_paths_are_explicit(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")

        self.assertNotIn("bench package bench_name *args:", justfile)
        self.assertNotIn("bench-workspace *args:", justfile)
        self.assertIn("build-for-release *args:", justfile)
        self.assertIn("target-optimize-dry-run *args:", justfile)
        self.assertIn("app-server-runtime-check:", justfile)
        self.assertIn("app-server-command-exec-check:", justfile)
        self.assertIn("app-server-process-exec-check:", justfile)
        self.assertIn("app-server-thread-status-check:", justfile)
        self.assertIn("app-server-schema-protocol-check:", justfile)
        self.assertIn("app-server-schema-check *args:", justfile)
        self.assertIn('app-server-schema-regenerate owner experimental="":', justfile)
        schema_recipe = justfile.split("\napp-server-schema-protocol-check:\n", 1)[
            1
        ].split("\n\n", 1)[0]
        self.assertEqual(
            schema_recipe.strip(),
            "just core-gate app-server-schema-fixtures; exit $LASTEXITCODE",
        )
        manifest = load_toml(REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml")
        steps = manifest["gates"]["app-server-schema-fixtures"]["steps"]
        self.assertEqual(len(steps), 1)
        target = manifest["targets"][steps[0]["target"]]
        self.assertEqual(target["package"], "codex-app-server-protocol")
        self.assertEqual(target["test"], "schema_fixtures")
        self.assertEqual(
            set(steps[0]["tests"]),
            {
                "typescript_schema_fixtures_match_generated",
                "json_schema_fixtures_match_generated",
            },
        )




if __name__ == "__main__":
    unittest.main()
