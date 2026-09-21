#!/usr/bin/env python3

import contextlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts import rust_build_status  # noqa: E402

SCRIPT = REPO_ROOT / "scripts" / "cargo-lane.ps1"
CLEANUP_SCRIPT = REPO_ROOT / "scripts" / "cargo-lane-trash-cleanup.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
LANES_ROOT_MARKER = ".codex-cargo-lanes-root"
LANES_ROOT_MARKER_CONTENT = "codex-kd cargo lanes root v1"


def powershell() -> str | None:
    # Production recipes explicitly invoke Windows PowerShell 5.1. Exercise
    # that host first so tests cannot pass only under PowerShell 7 semantics.
    return shutil.which("powershell") or shutil.which("pwsh")


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


class CargoLaneTest(unittest.TestCase):
    def test_background_pruning_does_not_block_commands_and_has_one_owner(self):
        repo = self.temp_root / "repo with spaces"
        scripts = repo / "scripts"
        scripts.mkdir(parents=True)
        (repo / "codex-rs").mkdir()
        for name in (
            "cargo-lane.ps1",
            "common-rust-env.ps1",
            "cargo-lane-patterns.ps1",
            "cargo_lane_patterns.json",
            "cargo-lane-trash-cleanup.ps1",
        ):
            shutil.copyfile(SCRIPT.parent / name, scripts / name)
        ready, release = repo / "prune-ready", repo / "prune-release"
        (scripts / "rust_build_status.py").write_text(
            "import pathlib,time\n"
            f"root=pathlib.Path({str(repo)!r})\n"
            "with (root/'attempts').open('a') as out: out.write('attempt\\n')\n"
            "(root/'prune-ready').touch()\n"
            "deadline=time.monotonic()+20\n"
            "while not (root/'prune-release').exists() and time.monotonic()<deadline: time.sleep(.02)\n"
            "assert (root/'prune-release').exists(), 'fixture was not released'\n"
        )
        lanes = repo / "codex-rs/target/lanes"
        env = {
            **os.environ,
            "CODEX_CARGO_LANE_MAINTENANCE_SYNC": "0",
            "CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE": "0",
            "CODEX_CARGO_LANE_ACTIVE_NAMES": "",
            "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "1",
        }
        completed = False
        try:
            first = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-File",
                    str(scripts / "cargo-lane.ps1"),
                    "-Lane",
                    "first",
                    sys.executable,
                    "-c",
                    "raise SystemExit(7)",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
                creationflags=CREATE_NO_WINDOW,
                check=False,
            )
            self.assertEqual(first.returncode, 7, first.stdout + first.stderr)
            deadline = time.monotonic() + 10
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(ready.exists(), first.stdout + first.stderr)
            self.assertFalse((lanes / ".gc-stamp").exists())
            # Python's production entrypoint uses the same existing worker.
            with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as errors:
                with mock.patch.dict(os.environ, env), contextlib.redirect_stderr(errors):
                    self.assertEqual(
                        rust_build_status.run_in_cargo_lane(
                            repo_root=repo,
                            requested_lane="second",
                            command=[sys.executable, "-c", "raise SystemExit(9)"],
                        ),
                        9,
                    )
                errors.seek(0)
                self.assertEqual(errors.read(), "")
            self.assertEqual((repo / "attempts").read_text().splitlines(), ["attempt"])
            self.assertFalse(release.exists())
            completed = True
        finally:
            release.touch()
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if (lanes / ".gc-stamp").exists() and not (
                    lanes / ".cargo-lane-trash-cleanup.lock"
                ).exists():
                    break
                time.sleep(0.05)
            # Lock release precedes PowerShell process exit. Wait for the actual
            # fixture workers before removing directories held as their cwd.
            wait = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-Command",
                    f"Get-CimInstance Win32_Process -Filter \"Name = 'powershell.exe'\" | Where-Object {{ $_.ProcessId -ne $PID -and $_.CommandLine -like '*cargo-lane*' -and $_.CommandLine.Contains({ps_single_quote(repo)}) }} | ForEach-Object {{ Wait-Process -Id $_.ProcessId -Timeout 10 -ErrorAction SilentlyContinue }}",
                ],
                capture_output=True,
                text=True,
                timeout=15,
                creationflags=CREATE_NO_WINDOW,
                check=False,
            )
            self.assertEqual(wait.returncode, 0, wait.stderr)
            if completed:
                self.assertTrue((lanes / ".gc-stamp").exists())
                self.assertFalse((lanes / ".cargo-lane-trash-cleanup.lock").exists())

    def test_shared_target_argument_corpus(self):
        target = self.temp_root / "target with spaces"
        t = str(target)
        cases = [
            (
                ["cargo", "+stable", "check", "-p", "core"],
                ["cargo", "+stable", "check", "--target-dir", t, "-p", "core"],
            ),
            (
                ["rustup", "run", "stable", "cargo", "check"],
                ["rustup", "run", "stable", "cargo", "check", "--target-dir", t],
            ),
            (
                [
                    "rustup",
                    "run",
                    "--install",
                    "stable",
                    "cargo",
                    "nextest",
                    "run",
                    "--",
                    "--target-dir",
                    "test-arg",
                ],
                [
                    "rustup",
                    "run",
                    "--install",
                    "stable",
                    "cargo",
                    "nextest",
                    "run",
                    "--target-dir",
                    t,
                    "--",
                    "--target-dir",
                    "test-arg",
                ],
            ),
            (
                ["cargo", "watch", "-xcheck"],
                ["cargo", "watch", '-xcheck --target-dir "' + t + '"'],
            ),
            (
                ["cargo", "watch", "-qx=test -- --nocapture"],
                [
                    "cargo",
                    "watch",
                    "-q",
                    '-xtest --target-dir "' + t + '" -- --nocapture',
                ],
            ),
            (
                ["cargo", "watch", "--watch", "-sfilename", "-xcheck"],
                [
                    "cargo",
                    "watch",
                    "--watch",
                    "-sfilename",
                    '-xcheck --target-dir "' + t + '"',
                ],
            ),
            (["cargo", "watch", "-scargo check"], None),
            (["cargo", "watch", "-qscargo check"], None),
            (["cargo", "watch", "check"], None),
            (["cargo", "watch", "-x"], None),
            (["cargo", "check", "--target-dir", "escape"], None),
        ]
        corpus = self.temp_root / "corpus.json"
        corpus.write_text(json.dumps([{"args": args} for args, _ in cases]))
        script = f"""
$ErrorActionPreference = 'Stop'
. {ps_single_quote(SCRIPT.parent / "common-rust-env.ps1")}
$results = @(foreach ($case in (Get-Content -Raw {ps_single_quote(corpus)} | ConvertFrom-Json)) {{
    try {{ @{{ args = @(Add-CargoTargetDirArgument -CommandArgs $case.args -TargetDir {ps_single_quote(target)}); rejected = $false }} }}
    catch {{ @{{ args = @(); rejected = $true }} }}
}})
ConvertTo-Json -InputObject $results -Depth 10 -Compress
"""
        result = subprocess.run(
            [self.shell, "-NoProfile", "-Command", script],
            capture_output=True,
            text=True,
            timeout=30,
            creationflags=CREATE_NO_WINDOW,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        actual = json.loads(result.stdout)
        for (args, expected), ps_result in zip(cases, actual, strict=True):
            with self.subTest(args=args):
                if expected is None:
                    with self.assertRaises(ValueError):
                        rust_build_status._cargo_command_with_target_dir(args, target)
                    self.assertTrue(ps_result["rejected"])
                else:
                    self.assertEqual(
                        rust_build_status._cargo_command_with_target_dir(args, target),
                        expected,
                    )
                    self.assertFalse(ps_result["rejected"])
                    self.assertEqual(ps_result["args"], expected)

    def test_candidate_junction_is_skipped_without_writing_external_metadata(self):
        self.mark_lanes_root()
        outside = self.temp_root / "outside"
        outside.mkdir()
        (outside / "sentinel").write_bytes(b"unchanged")
        self.make_junction(self.lanes_root / "candidate", outside)
        result = self.run_script("-Lane", "candidate")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("LANE=candidate-2", result.stdout)
        self.assertEqual(sorted(p.name for p in outside.iterdir()), ["sentinel"])
        self.assertEqual((outside / "sentinel").read_bytes(), b"unchanged")

    def test_concurrent_auto_reservations_stay_in_canonical_family(self):
        self.make_lane("core-2")
        self.make_lane("core-3")
        os.utime(self.lanes_root / "core-2", None)
        line = next(
            i
            for i, text in enumerate(SCRIPT.read_text().splitlines(), 1)
            if text.startswith("$candidateLane =")
        )
        cargo = self.temp_root / "cargo.ps1"
        cargo.write_text(
            f"[IO.File]::WriteAllText((Join-Path {ps_single_quote(self.temp_root)} ('child-' + $PID)), $env:CODEX_CARGO_LANE_TARGET_DIR)\nwhile (-not (Test-Path {ps_single_quote(self.temp_root / 'release-child')})) {{ Start-Sleep -Milliseconds 20 }}\n"
        )
        processes = []
        try:
            for index in range(2):
                command = f"""
$env:CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE = '1'
$env:CODEX_CARGO_LANE_ACTIVE_NAMES = 'core'
Set-PSBreakpoint -Script {ps_single_quote(SCRIPT)} -Line {line} -Action {{
    [IO.File]::WriteAllText({ps_single_quote(self.temp_root / f"ready-{index}")}, 'ready')
    while (-not (Test-Path {ps_single_quote(self.temp_root / "release-snapshot")})) {{ Start-Sleep -Milliseconds 20 }}
}} | Out-Null
& {ps_single_quote(SCRIPT)} -Lane auto -LanesRoot {ps_single_quote(self.lanes_root)} {ps_single_quote(cargo)} check -p core
"""
                processes.append(
                    subprocess.Popen(
                        [self.shell, "-NoProfile", "-Command", command],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        creationflags=CREATE_NO_WINDOW,
                    )
                )
            deadline = time.monotonic() + 15
            while (
                len(list(self.temp_root.glob("ready-*"))) != 2
                and time.monotonic() < deadline
            ):
                time.sleep(0.02)
            self.assertEqual(len(list(self.temp_root.glob("ready-*"))), 2)
            (self.temp_root / "release-snapshot").touch()
            while (
                len(list(self.temp_root.glob("child-*"))) != 2
                and time.monotonic() < deadline
            ):
                time.sleep(0.02)
            children = list(self.temp_root.glob("child-*"))
            self.assertEqual(len(children), 2)
            self.assertEqual(
                {Path(p.read_text(encoding="utf-8-sig")).name for p in children},
                {"core-2", "core-3"},
            )
        finally:
            (self.temp_root / "release-snapshot").touch()
            (self.temp_root / "release-child").touch()
            for process in processes:
                out, err = process.communicate(timeout=15)
                self.assertEqual(process.returncode, 0, out + err)

    def test_reservation_rechecks_cargo_lock_after_active_snapshot(self):
        lane = self.make_lane("late-cargo")
        lock_path = lane / "debug" / ".cargo-lock"
        lock_path.parent.mkdir()
        lock_path.touch()
        # Pause at the real entrypoint immediately after its initial snapshot,
        # then simulate a raw Cargo process opening its profile lock.
        line = next(
            i
            for i, text in enumerate(SCRIPT.read_text().splitlines(), 1)
            if text.startswith("$candidateLane =")
        )
        command = f"""
$ErrorActionPreference = 'Stop'
$env:CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE = '1'
$env:CODEX_CARGO_TARGET_MAX_TOTAL_BYTES = '0'
Set-PSBreakpoint -Script {ps_single_quote(SCRIPT)} -Line {line} -Action {{
    $global:lateCargoLock = [IO.File]::Open({ps_single_quote(lock_path)}, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
}} | Out-Null
& {ps_single_quote(SCRIPT)} -LanesRoot {ps_single_quote(self.lanes_root)} -Lane late-cargo
"""
        result = subprocess.run(
            [self.shell, "-NoProfile", "-Command", command],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("LANE=late-cargo-2", result.stdout)
        self.assertFalse((lane / ".lane-active.lock").exists())

    def test_cleanup_preserves_all_payloads_under_busy_cargo_profiles(self):
        for profile in (Path("debug"), Path("triple") / "release"):
            with self.subTest(profile=profile):
                lane = self.make_lane("busy.trash-20260102030405000", size=10)
                lock_path = lane / profile / ".cargo-lock"
                lock_path.parent.mkdir(parents=True)
                held = rust_build_status._try_acquire_binary_file_lock(lock_path)
                self.assertIsNotNone(held)
                command = [
                    self.shell,
                    "-NoProfile",
                    "-File",
                    str(CLEANUP_SCRIPT),
                    "-LanesRoot",
                    str(self.lanes_root),
                    "-MaxPasses",
                    "1",
                ]
                with held:
                    result = subprocess.run(
                        command,
                        check=False,
                        capture_output=True,
                        text=True,
                        timeout=30,
                        creationflags=CREATE_NO_WINDOW,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual((lane / "payload.bin").read_bytes(), b"x" * 10)
                result = subprocess.run(
                    command,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    creationflags=CREATE_NO_WINDOW,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(lane.exists())

    def test_powershell_skips_gc_held_by_python(self):
        self.mark_lanes_root()
        lock = rust_build_status._try_acquire_binary_file_lock(
            self.lanes_root / ".lane-gc.lock"
        )
        self.assertIsNotNone(lock)
        with lock:
            result = self.run_fake_cargo("-Lane", "unit", "cargo", "check")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("cargo-args:check", result.stdout)
            self.assertFalse((self.lanes_root / ".gc-stamp").exists())

    def test_auto_lane_ranks_last_used_above_directory_mtime(self):
        for name, stamp_time, dir_time in (("warm-2", 10, 30), ("warm-3", 20, 1)):
            lane = self.make_lane(name)
            stamp = lane / ".lane-last-used"
            stamp.touch()
            os.utime(stamp, (stamp_time, stamp_time))
            os.utime(lane, (dir_time, dir_time))
        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            "-p=warm",
            extra_env={"CODEX_CARGO_LANE_ACTIVE_NAMES": "warm"},
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"--target-dir {self.lane_path('warm-3')}", result.stdout)

    def test_real_cargo_profile_locks_protect_selection_and_pruning(self):
        cargo = shutil.which("cargo")
        if cargo is None:
            self.skipTest("Cargo is required")
        crate = self.temp_root / "crate"
        crate.mkdir()
        (crate / "Cargo.toml").write_text(
            '[package]\nname="lock-probe"\nversion="0.1.0"\nedition="2021"\n'
            '[lib]\npath="lib.rs"\n',
            encoding="utf-8",
        )
        (crate / "lib.rs").write_text("pub fn probe() {}\n", encoding="utf-8")
        (crate / "build.rs").write_text(
            "fn main() {\n"
            'std::fs::write(std::env::var("PROBE_READY").unwrap(), "ready").unwrap();\n'
            'let release = std::env::var("PROBE_RELEASE").unwrap();\n'
            "while !std::path::Path::new(&release).exists() {\n"
            "std::thread::sleep(std::time::Duration::from_millis(50)); }\n}\n",
            encoding="utf-8",
        )
        self.mark_lanes_root()
        host = subprocess.run(
            ["rustc", "-vV"], check=True, capture_output=True, text=True
        )
        triple = next(
            line.removeprefix("host: ")
            for line in host.stdout.splitlines()
            if line.startswith("host: ")
        )
        for suffix, flags in (
            (Path("debug"), []),
            (Path(triple) / "release", ["--target", triple, "--release"]),
        ):
            with self.subTest(profile=str(suffix)):
                lane = self.make_lane("real-cargo")
                ready = crate / "ready"
                release = crate / "release"
                ready.unlink(missing_ok=True)
                release.unlink(missing_ok=True)
                env = os.environ.copy()
                for key in list(env):
                    if key.startswith("CARGO_") or key in (
                        "RUSTFLAGS",
                        "RUSTC_WRAPPER",
                        "RUSTC_WORKSPACE_WRAPPER",
                    ):
                        env.pop(key)
                env.update(
                    CARGO_HOME=str(self.temp_root / "cargo-home"),
                    PROBE_READY=str(ready),
                    PROBE_RELEASE=str(release),
                )
                process = subprocess.Popen(
                    [cargo, "check", "--offline", "--target-dir", str(lane), *flags],
                    cwd=crate,
                    env=env,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                    creationflags=CREATE_NO_WINDOW,
                )
                try:
                    deadline = time.monotonic() + 50
                    while (
                        not ready.exists()
                        and process.poll() is None
                        and time.monotonic() < deadline
                    ):
                        time.sleep(0.05)
                    self.assertTrue(
                        ready.exists(), "Cargo did not enter the build script"
                    )
                    self.assertTrue((lane / suffix / ".cargo-lock").is_file())
                    self.assertTrue(rust_build_status.cargo_lock_is_busy(lane))
                    with rust_build_status.reserve_cargo_lane(
                        repo_root=self.temp_root,
                        lane_root=self.lanes_root,
                        requested_lane="real-cargo",
                        command=["cargo", "check"],
                    ) as (name, _):
                        self.assertEqual(name, "real-cargo-2")
                    result = self.run_fake_cargo(
                        "-Lane", "real-cargo", "cargo", "check"
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(self.lane_path("real-cargo-2"), result.stdout)
                    from unittest import mock

                    with mock.patch.dict(
                        os.environ, {"CODEX_CARGO_LANES_ROOT": str(self.lanes_root)}
                    ):
                        removed = rust_build_status.prune_stale_lanes(
                            repo_root=self.temp_root,
                            processes=[],
                            keep_warm_per_base=0,
                            max_age_days=None,
                        )
                    self.assertNotIn(lane, removed)
                    self.assertTrue(lane.exists())
                finally:
                    release.write_text("release", encoding="utf-8")
                    try:
                        stdout, stderr = process.communicate(timeout=20)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        stdout, stderr = process.communicate()
                self.assertEqual(process.returncode, 0, stdout + stderr)
                self.assertFalse(rust_build_status.cargo_lock_is_busy(lane))

    def test_setup_failure_releases_reservation_in_surviving_host(self):
        command = f"""
$ErrorActionPreference = 'Stop'
$before = (Get-Location).Path
$env:CODEX_CARGO_LANE_TARGET_DIR = 'previous-lane'
$env:LOCALAPPDATA = $null
$env:CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE = '1'
$env:CODEX_CARGO_TARGET_MAX_TOTAL_BYTES = '0'
try {{
    & {ps_single_quote(SCRIPT)} -LanesRoot {ps_single_quote(self.lanes_root)} -Lane setup-failure -IsolateCargoHome
    throw 'Expected setup to fail'
}} catch {{
    if ($_.Exception.Message -notlike '*LOCALAPPDATA is not set*') {{ throw }}
}}
if ($env:CODEX_CARGO_LANE_TARGET_DIR -ne 'previous-lane') {{ throw 'Lane environment leaked' }}
if ((Get-Location).Path -ne $before) {{ throw 'Location changed' }}
$probe = [IO.File]::Open({ps_single_quote(self.lanes_root / "setup-failure" / ".lane-active.lock")}, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
$probe.Dispose()
Write-Output 'reservation released'
"""
        result = subprocess.run(
            [self.shell, "-NoProfile", "-Command", command],
            capture_output=True,
            text=True,
            timeout=30,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("reservation released", result.stdout)

    def test_trash_namespace_is_rejected_before_mutation(self):
        result = self.run_script("-Lane", "active.trash-20260102030405000")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("reserved for cleanup", result.stderr)
        self.assertEqual(list(self.lanes_root.iterdir()), [])

    def test_watch_rewrites_all_execs_before_terminal_separator(self):
        result = self.run_fake_cargo(
            "-Lane", "watch", "cargo", "watch", "-x", "test --", "--exec=check", "--"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"test --target-dir {self.lane_path('watch')} --", result.stdout)
        self.assertIn(
            f"--exec=check --target-dir {self.lane_path('watch')}", result.stdout
        )

    def test_alias_and_nextest_list_are_isolated(self):
        for command in (("b",), ("clean",), ("nextest", "list")):
            result = self.run_fake_cargo("-Lane", "alias", "cargo", *command)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(f"--target-dir {self.lane_path('alias')}", result.stdout)
        result = self.run_fake_cargo("-Lane", "alias", "cargo", "local-alias")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("cargo-args:", result.stdout)

    def test_cleanup_keeps_reserved_trash_then_removes_released_directory(self):
        self.mark_lanes_root()
        path = self.make_lane("held.trash-20260102030405000", size=10)
        held = rust_build_status._try_acquire_binary_file_lock(
            path / ".lane-active.lock"
        )
        self.assertIsNotNone(held)
        with held:
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-File",
                    str(CLEANUP_SCRIPT),
                    "-LanesRoot",
                    str(self.lanes_root),
                    "-MaxPasses",
                    "1",
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(path.exists())
        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-File",
                str(CLEANUP_SCRIPT),
                "-LanesRoot",
                str(self.lanes_root),
                "-MaxPasses",
                "1",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(path.exists())

    def setUp(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        self.shell = shell
        self.temp_dir = tempfile.TemporaryDirectory()
        self.temp_root = Path(self.temp_dir.name)
        self.lanes_root = self.temp_root / "lanes"
        self.lanes_root.mkdir(parents=True)

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def run_script(
        self,
        *args: str,
        extra_env: dict[str, str] | None = None,
        lanes_root: Path | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE"] = "1"
        env["CODEX_CARGO_LANE_MAINTENANCE_SYNC"] = "1"
        # Most lane-wrapper tests exercise naming, locking, and forwarding. Keep
        # them isolated from the real repository's large non-lane target tree;
        # target-budget tests opt back into the production default explicitly.
        env["CODEX_CARGO_TARGET_MAX_TOTAL_BYTES"] = "0"
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(SCRIPT),
                "-LanesRoot",
                str(self.lanes_root if lanes_root is None else lanes_root),
                *args,
            ],
            text=True,
            capture_output=True,
            check=False,
            env=env,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )

    def lane_path(self, lane: str) -> str:
        return str(self.lanes_root / lane)

    def mark_lanes_root(self, root: Path | None = None) -> None:
        lane_root = self.lanes_root if root is None else root
        lane_root.mkdir(parents=True, exist_ok=True)
        (lane_root / LANES_ROOT_MARKER).write_text(
            LANES_ROOT_MARKER_CONTENT + "\n",
            encoding="utf-8",
        )

    def fake_cargo_bin(self) -> Path:
        bin_dir = self.temp_root / "bin"
        bin_dir.mkdir(exist_ok=True)
        (bin_dir / "cargo.cmd").write_text(
            "@echo off\r\necho cargo-args:%*\r\n",
            encoding="utf-8",
        )
        return bin_dir

    def run_fake_cargo(
        self,
        *args: str,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        env = dict(extra_env or {})
        base_path = env.get("PATH", os.environ["PATH"])
        env["PATH"] = f"{self.fake_cargo_bin()}{os.pathsep}{base_path}"
        return self.run_script(*args, extra_env=env)

    def make_lane(self, lane: str, *, size: int = 0, days_old: int = 0) -> Path:
        self.mark_lanes_root()
        path = self.lanes_root / lane
        path.mkdir(parents=True, exist_ok=True)
        if size > 0:
            (path / "payload.bin").write_bytes(b"x" * size)
        if days_old > 0:
            timestamp = time.time() - (days_old * 24 * 60 * 60)
            os.utime(path, (timestamp, timestamp))
        return path

    def make_junction(self, junction: Path, target: Path) -> None:
        created = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(junction), str(target)],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        if created.returncode != 0:
            self.skipTest(f"could not create test junction: {created.stderr}")

    def test_rejects_command_mistaken_for_positional_lane(self) -> None:
        result = self.run_fake_cargo("cargo", "check")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("looks like a command", result.stderr)
        self.assertIn("-Lane <name>", result.stderr)

    def test_lane_option_rejects_another_option_as_its_value(self) -> None:
        result = self.run_script("-Lane", "-Fetch")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not start with '-'", result.stderr)

    def test_pure_dot_lane_names_are_rejected_before_root_mutation(self) -> None:
        before = sorted(path.name for path in self.lanes_root.iterdir())
        for lane in (".", "..", "..."):
            with self.subTest(lane=lane):
                result = self.run_script("-Lane", lane, "cmd.exe", "/c", "echo ok")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("not a valid lane name", result.stderr)
                self.assertEqual(
                    sorted(path.name for path in self.lanes_root.iterdir()),
                    before,
                )

    def test_missing_python_does_not_block_lane_command(self) -> None:
        lane = f"unit-no-python-{os.getpid()}"
        fake_bin = self.fake_cargo_bin()
        path_without_python = (
            f"{fake_bin}{os.pathsep}{os.environ['SystemRoot']}\\System32"
        )

        result = self.run_script(
            "-Lane",
            lane,
            "cargo",
            "check",
            extra_env={
                "PATH": path_without_python,
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("cargo-args:check --target-dir", result.stdout)
        self.assertIn("python executable was not found", result.stdout + result.stderr)

    def test_lane_last_used_stamp_is_refreshed_after_command_finishes(self) -> None:
        lane = f"unit-last-used-{os.getpid()}"
        stamp = self.lanes_root / lane / ".lane-last-used"
        stale_timestamp = time.time() - (30 * 24 * 60 * 60)
        command = (
            "import os, sys; "
            "stamp = sys.argv[1]; "
            "os.utime(stamp, (float(sys.argv[2]), float(sys.argv[2]))); "
            "raise SystemExit(7)"
        )

        result = self.run_script(
            "-Lane",
            lane,
            sys.executable,
            "-c",
            command,
            str(stamp),
            str(stale_timestamp),
        )

        self.assertEqual(result.returncode, 7)
        self.assertGreater(stamp.stat().st_mtime, stale_timestamp + 60)

    def test_explicit_busy_lane_reports_effective_suffix(self) -> None:
        lane = f"unit-explicit-busy-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "check",
            extra_env={"CODEX_CARGO_LANE_ACTIVE_NAMES": lane},
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        warning_output = result.stdout + result.stderr
        self.assertIn(f"'{lane}' is busy", warning_output)
        self.assertIn(f"'{lane}-2'", warning_output)
        self.assertIn(f"--target-dir {self.lane_path(f'{lane}-2')}", result.stdout)

    def test_powershell_runner_honors_python_active_lane_lock(self) -> None:
        lane = f"unit-python-lock-{os.getpid()}"
        with rust_build_status.reserve_cargo_lane(
            repo_root=REPO_ROOT,
            requested_lane=lane,
            command=["cargo", "check"],
            lane_root=self.lanes_root,
        ):
            result = self.run_fake_cargo("-Lane", lane, "cargo", "check")

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        output = result.stdout + result.stderr
        self.assertIn(f"'{lane}' is busy", output)
        self.assertIn(f"'{lane}-2'", output)
        self.assertIn(f"--target-dir {self.lane_path(f'{lane}-2')}", result.stdout)

    def test_relative_lanes_root_follows_powershell_location(self) -> None:
        relative_root = "relative-lanes"
        command = (
            f"Set-Location {ps_single_quote(self.temp_root)}; "
            f"& {ps_single_quote(SCRIPT)} -LanesRoot {relative_root} "
            "-Lane relative cmd.exe /d /c echo ok"
        )
        env = os.environ.copy()
        env["CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE"] = "1"
        env["CODEX_CARGO_LANE_MAINTENANCE_SYNC"] = "1"
        env["CODEX_CARGO_TARGET_MAX_TOTAL_BYTES"] = "0"
        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=REPO_ROOT,
            text=True,
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
        self.assertTrue((self.temp_root / relative_root / "relative").is_dir())
        self.assertTrue((self.temp_root / relative_root / LANES_ROOT_MARKER).is_file())

    def test_cleanup_relative_root_follows_powershell_location(self) -> None:
        relative_root = self.temp_root / "cleanup-lanes"
        self.mark_lanes_root(relative_root)
        trash = relative_root / "old.trash-20260728123456789"
        trash.mkdir(parents=True)
        command = (
            f"Set-Location {ps_single_quote(self.temp_root)}; "
            f"& {ps_single_quote(CLEANUP_SCRIPT)} -LanesRoot cleanup-lanes "
            "-MaxPasses 1 -RetryDelaySeconds 0"
        )

        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=REPO_ROOT,
            text=True,
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
        self.assertFalse(trash.exists())

    def test_unmarked_custom_root_is_not_pruned(self) -> None:
        unsafe_root = self.temp_root / "ordinary-root"
        ordinary_dir = unsafe_root / "family-photos"
        ordinary_dir.mkdir(parents=True)
        (ordinary_dir / "photo.txt").write_text("keep", encoding="utf-8")

        result = self.run_script(
            "-Lane",
            f"unit-unmarked-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            lanes_root=unsafe_root,
            extra_env={"CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0"},
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertTrue(ordinary_dir.exists())
        self.assertFalse((unsafe_root / LANES_ROOT_MARKER).exists())
        self.assertIn(
            "Skipping Cargo lane pruning for unrecognized lanes root",
            result.stdout + result.stderr,
        )

    def test_cleanup_rejects_unmarked_root(self) -> None:
        unsafe_root = self.temp_root / "ordinary-cleanup-root"
        trash = unsafe_root / "family.trash-20260728123456789"
        trash.mkdir(parents=True)

        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(CLEANUP_SCRIPT),
                "-LanesRoot",
                str(unsafe_root),
                "-MaxPasses",
                "1",
                "-RetryDelaySeconds",
                "0",
            ],
            text=True,
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
        self.assertTrue(trash.exists())

    def test_lane_runner_rejects_junction_root_before_mutation(self) -> None:
        external_root = self.temp_root / "external-lanes-root"
        self.mark_lanes_root(external_root)
        sentinel = external_root / "keep.txt"
        sentinel.write_text("keep", encoding="utf-8")
        junction_root = self.temp_root / "junction-lanes-root"
        self.make_junction(junction_root, external_root)

        result = self.run_script(
            "-Lane",
            f"unit-junction-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            lanes_root=junction_root,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not be a reparse point or junction", result.stderr)
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_lane_runner_rejects_junction_ancestor_before_creating_root(self):
        external = self.temp_root / "external-parent"
        external.mkdir()
        sentinel = external / "keep.txt"
        sentinel.write_text("keep", encoding="utf-8")
        junction = self.temp_root / "linked-parent"
        self.make_junction(junction, external)
        result = self.run_script(
            "-Lane", "unit", "cmd.exe", "/c", "echo ok", lanes_root=junction / "lanes"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not be a reparse point or junction", result.stderr)
        self.assertEqual(list(external.iterdir()), [sentinel])
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def test_cleanup_rejects_junction_root(self) -> None:
        external_root = self.temp_root / "external-cleanup-root"
        self.mark_lanes_root(external_root)
        trash = external_root / "old.trash-20260728123456789"
        trash.mkdir()
        junction_root = self.temp_root / "junction-cleanup-root"
        self.make_junction(junction_root, external_root)

        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(CLEANUP_SCRIPT),
                "-LanesRoot",
                str(junction_root),
                "-MaxPasses",
                "1",
                "-RetryDelaySeconds",
                "0",
            ],
            text=True,
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
        self.assertTrue(trash.exists())

    def test_cleanup_preserves_trash_named_junction_and_external_target(self) -> None:
        self.mark_lanes_root()
        external = self.temp_root / "external-sentinel"
        external.mkdir()
        sentinel = external / "keep.txt"
        sentinel.write_text("keep", encoding="utf-8")
        junction = self.lanes_root / "linked.trash-20260728123456789"
        created = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(junction), str(external)],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        if created.returncode != 0:
            self.skipTest(f"could not create test junction: {created.stderr}")

        result = subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(CLEANUP_SCRIPT),
                "-LanesRoot",
                str(self.lanes_root),
                "-MaxPasses",
                "1",
                "-RetryDelaySeconds",
                "0",
            ],
            text=True,
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
        self.assertTrue(junction.exists())
        self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")

    def hold_lane_lock(
        self,
        lane: str,
        *,
        lock_name: str = ".cargo-lock",
    ) -> subprocess.Popen[str]:
        lane_path = self.make_lane(lane)
        lock_path = (
            lane_path / "debug" / lock_name
            if lock_name == ".cargo-lock"
            else lane_path / lock_name
        )
        lock_path.parent.mkdir(exist_ok=True)
        ready_path = lane_path / f"{lock_name}.ready"
        helper = (
            self.temp_root / f"hold-{lock_name.removeprefix('.').replace('.', '-')}.ps1"
        )
        helper.write_text(
            "\n".join(
                [
                    "param([string]$LockPath, [string]$ReadyPath)",
                    "$stream = [IO.File]::Open(",
                    "    $LockPath,",
                    "    [IO.FileMode]::OpenOrCreate,",
                    "    [IO.FileAccess]::ReadWrite,",
                    "    [IO.FileShare]::None",
                    ")",
                    "[IO.File]::WriteAllText($ReadyPath, 'ready')",
                    "try { Start-Sleep -Seconds 30 } finally { $stream.Dispose() }",
                ]
            ),
            encoding="utf-8",
        )
        process = subprocess.Popen(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(helper),
                str(lock_path),
                str(ready_path),
            ],
            text=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=CREATE_NO_WINDOW,
        )
        deadline = time.time() + 5
        while time.time() < deadline:
            if ready_path.exists():
                return process
            if process.poll() is not None:
                self.fail("lock helper exited early")
            time.sleep(0.05)
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
        self.fail("timed out waiting for lock helper")

    def test_sets_default_rust_min_stack_for_direct_lane_commands(self) -> None:
        lane = f"unit-stack-{os.getpid()}"

        result = self.run_script(
            "-Lane",
            lane,
            "cmd.exe",
            "/d",
            "/c",
            "if defined CARGO_TARGET_DIR (echo target=%CARGO_TARGET_DIR%) else echo target=&echo %RUST_MIN_STACK%",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
        self.assertIn("8388608", lines)
        self.assertIn("target=", lines)

    def test_no_command_guidance_routes_core_tests_through_named_lanes(self) -> None:
        fake_bin = self.fake_cargo_bin()
        args_log = self.temp_root / "guidance-gc-args.txt"
        (fake_bin / "python.cmd").write_text(
            '@echo off\r\necho %* >> "%CODEX_TEST_GC_ARGS_LOG%"\r\n',
            encoding="utf-8",
        )
        self.mark_lanes_root()
        (self.lanes_root / ".gc-stamp").write_text("fresh\n", encoding="utf-8")
        result = self.run_script(
            "-Lane",
            f"unit-guidance-{os.getpid()}",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "1",
                "CODEX_TEST_GC_ARGS_LOG": str(args_log),
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("just core-test-lane core_lib", result.stdout)
        self.assertFalse(args_log.exists(), "Guidance must not force post-build GC")
        self.assertNotIn("test-lane-package codex-core", result.stdout)
        self.assertNotIn("cargo nextest run -p codex-core", result.stdout)

    def test_uses_scoop_llvm_lld_link_when_not_on_path(self) -> None:
        lane = f"unit-linker-{os.getpid()}"
        user_profile = self.temp_root / "user"
        scoop_llvm_bin = user_profile / "scoop" / "apps" / "llvm" / "current" / "bin"
        scoop_llvm_bin.mkdir(parents=True)
        lld_link = scoop_llvm_bin / "lld-link.exe"
        lld_link.write_bytes(b"")
        fake_bin = self.fake_cargo_bin()
        python_bin = Path(sys.executable).parent
        path_without_llvm = (
            f"{fake_bin}{os.pathsep}{python_bin}{os.pathsep}"
            f"{os.environ['SystemRoot']}\\System32"
        )

        result = self.run_script(
            "-Lane",
            lane,
            "cmd.exe",
            "/d",
            "/c",
            "echo x64=%CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER%&echo arm64=%CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER%",
            extra_env={
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "",
                "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER": "",
                "PATH": path_without_llvm,
                "SCOOP": "",
                "USERPROFILE": str(user_profile),
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"x64={lld_link}", result.stdout)
        self.assertIn(f"arm64={lld_link}", result.stdout)

    def test_auto_lane_uses_package_name_for_stable_cache_affinity(self) -> None:
        package = f"unit-core-{os.getpid()}"
        for selection in (
            ["-p", package],
            [f"-p{package}"],
            [f"-p={package}"],
            ["--package", package],
            [f"--package={package}"],
        ):
            with self.subTest(selection=selection):
                result = self.run_fake_cargo(
                    "-Lane",
                    "auto",
                    "cargo",
                    "check",
                    *selection,
                )
                self.assertEqual(
                    result.returncode,
                    0,
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                )
                self.assertIn(f"--target-dir {self.lane_path(package)}", result.stdout)

    def test_mismatched_cargo_target_dir_is_rejected(self) -> None:
        package = f"unit-explicit-target-{os.getpid()}"
        explicit_target = self.temp_root / "explicit-target"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            "--target-dir",
            str(explicit_target),
            "-p",
            package,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match reserved lane target", result.stderr)

    def test_matching_cargo_target_dir_is_not_duplicated(self) -> None:
        lane = f"unit-matching-target-{os.getpid()}"
        explicit_target = self.lanes_root / lane

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "check",
            "--target-dir",
            str(explicit_target),
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.count("--target-dir"), 1)
        self.assertIn(f"--target-dir {explicit_target}", result.stdout)

    def test_lowercase_c_is_not_treated_as_value_taking_uppercase_option(self) -> None:
        lane = f"unit-lower-c-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "-c",
            "check",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"check --target-dir {self.lane_path(lane)}", result.stdout)

    def test_mismatched_equals_cargo_target_dir_is_rejected(self) -> None:
        package = f"unit-explicit-equals-target-{os.getpid()}"
        explicit_target = "explicit-equals-target"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            f"--target-dir={explicit_target}",
            "-p",
            package,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match reserved lane target", result.stderr)

    def test_mismatched_nextest_target_dir_is_rejected(self) -> None:
        package = f"unit-nextest-explicit-target-{os.getpid()}"
        explicit_target = self.temp_root / "nextest-target"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "nextest",
            "run",
            "--target-dir",
            str(explicit_target),
            "-p",
            package,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match reserved lane target", result.stderr)

    def test_auto_lane_reuses_warm_idle_suffix_when_base_lane_is_active(self) -> None:
        package = f"unit-core-active-{os.getpid()}"
        warm_suffix = f"{package}-2"
        self.mark_lanes_root()
        (self.lanes_root / warm_suffix).mkdir(parents=True, exist_ok=True)

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            "-p",
            package,
            extra_env={"CODEX_CARGO_LANE_ACTIVE_NAMES": package},
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(warm_suffix)}", result.stdout)

    def test_auto_lane_mints_suffix_when_base_lane_is_active(self) -> None:
        package = f"unit-core-mint-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            "-p",
            package,
            extra_env={"CODEX_CARGO_LANE_ACTIVE_NAMES": package},
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(f'{package}-2')}", result.stdout)

    def test_auto_lane_skips_busy_cargo_lock(self) -> None:
        package = f"unit-core-lock-{os.getpid()}"
        lock_process = self.hold_lane_lock(package)
        try:
            result = self.run_fake_cargo(
                "-Lane",
                "auto",
                "cargo",
                "check",
                "-p",
                package,
            )
        finally:
            lock_process.terminate()
            try:
                lock_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                lock_process.kill()
                lock_process.wait(timeout=5)

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(f'{package}-2')}", result.stdout)

    def test_auto_lane_skips_unreadable_cargo_lock(self) -> None:
        package = f"unit-core-readonly-{os.getpid()}"
        lane = self.make_lane(package)
        lock_path = lane / "debug" / ".cargo-lock"
        lock_path.parent.mkdir(exist_ok=True)
        lock_path.write_text("stale", encoding="utf-8")
        lock_path.chmod(stat.S_IREAD)
        try:
            result = self.run_fake_cargo(
                "-Lane",
                "auto",
                "cargo",
                "check",
                "-p",
                package,
            )
        finally:
            lock_path.chmod(stat.S_IWRITE)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.lane_path(f"{package}-2"), result.stdout)

    def test_auto_lane_skips_busy_lane_reservation_lock(self) -> None:
        package = f"unit-core-reserved-{os.getpid()}"
        lock_process = self.hold_lane_lock(package, lock_name=".lane-active.lock")
        try:
            result = self.run_fake_cargo(
                "-Lane",
                "auto",
                "cargo",
                "check",
                "-p",
                package,
            )
        finally:
            lock_process.terminate()
            try:
                lock_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                lock_process.kill()
                lock_process.wait(timeout=5)

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(f'{package}-2')}", result.stdout)

    def test_cargo_llvm_cov_gets_lane_target_dir(self) -> None:
        package = f"unit-coverage-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "llvm-cov",
            "-p",
            package,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(package)}", result.stdout)

    def test_cargo_watch_default_check_gets_lane_target_dir(self) -> None:
        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("-x", result.stdout)
        self.assertIn("check --target-dir", result.stdout)
        self.assertIn(str(self.lanes_root), result.stdout)

    def test_cargo_watch_exec_gets_lane_target_dir(self) -> None:
        package = f"unit-watch-exec-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
            "-x",
            f"check -p {package}",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"check -p {package} --target-dir", result.stdout)
        self.assertIn(self.lane_path(package), result.stdout)

    def test_cargo_watch_exec_equals_gets_lane_target_dir(self) -> None:
        package = f"unit-watch-equals-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
            f"--exec=check -p {package}",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--exec=check -p {package} --target-dir", result.stdout)
        self.assertIn(self.lane_path(package), result.stdout)

    def test_cargo_watch_exec_rejects_mismatched_target_dir(self) -> None:
        lane = f"unit-watch-mismatch-{os.getpid()}"
        mismatched_target = self.temp_root / "watch-escape"

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "watch",
            "-x",
            f'check --target-dir "{mismatched_target}"',
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match reserved lane target", result.stderr)

    def test_cargo_watch_exec_accepts_matching_target_dir(self) -> None:
        lane = f"unit-watch-match-{os.getpid()}"
        matching_target = self.lanes_root / lane

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "watch",
            "-x",
            f'check --target-dir "{matching_target}"',
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.count("--target-dir"), 1)

    def test_cargo_watch_exec_inserts_target_before_test_arguments(self) -> None:
        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
            "-x",
            "test -- --nocapture",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("test --target-dir", result.stdout)
        self.assertIn(" -- --nocapture", result.stdout)

    def test_cargo_watch_shell_command_is_rejected(self) -> None:
        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
            "-s",
            "cargo check",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--shell/-s is not allowed", result.stderr)

    def test_timestamped_explicit_lane_is_preserved_literally(self) -> None:
        lane = f"unit-stable-{os.getpid()}-20260608183755"

        result = self.run_fake_cargo(
            "-Lane",
            lane,
            "cargo",
            "check",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(lane)}", result.stdout)

    def test_isolated_cargo_home_preserves_user_config_and_adds_sccache(self) -> None:
        lane = f"unit-cargo-home-{os.getpid()}"
        user_profile = self.temp_root / "user"
        cargo_config = user_profile / ".cargo" / "config.toml"
        cargo_config.parent.mkdir(parents=True)
        source_config = 'build.rustc-wrapper = "custom-wrapper"\n[net]\nretry = 3\n'
        cargo_config.write_text(source_config, encoding="utf-8")
        local_app_data = self.temp_root / "local-app-data"
        fake_bin = self.temp_root / "bin"
        fake_bin.mkdir()
        (fake_bin / "sccache.cmd").write_text("@echo off\r\n", encoding="utf-8")

        result = self.run_script(
            "-Lane",
            lane,
            "-IsolateCargoHome",
            "cmd.exe",
            "/d",
            "/c",
            "echo wrapper=%RUSTC_WRAPPER% incremental=%CARGO_INCREMENTAL% %CARGO_HOME% %SCCACHE_BASEDIR% %SCCACHE_CACHE_SIZE%",
            extra_env={
                "CARGO_INCREMENTAL": "",
                "LOCALAPPDATA": str(local_app_data),
                "RUSTC_WRAPPER": "",
                "USERPROFILE": str(user_profile),
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            },
        )

        isolated_config = (
            local_app_data / "cargo-lanes" / "codexKD" / lane / "config.toml"
        )
        config_text = isolated_config.read_text(encoding="utf-8")
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(config_text, source_config)
        self.assertIn("wrapper=sccache", result.stdout)
        self.assertIn(str(REPO_ROOT), result.stdout)
        self.assertIn("80G", result.stdout)
        self.assertIn("incremental=%CARGO_INCREMENTAL%", result.stdout)

    def test_sccache_lane_preserves_explicit_cargo_incremental(self) -> None:
        fake_bin = self.temp_root / "bin"
        fake_bin.mkdir()
        (fake_bin / "sccache.cmd").write_text("@echo off\r\n", encoding="utf-8")

        result = self.run_script(
            "-Lane",
            f"unit-incremental-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo incremental=%CARGO_INCREMENTAL%",
            extra_env={
                "CARGO_INCREMENTAL": "1",
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "RUSTC_WRAPPER": "",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("incremental=1", result.stdout)

    def test_auto_lane_routes_release_builds_to_release_lane(self) -> None:
        for release_arg in ("--release", "-r", "--profile=release"):
            with self.subTest(release_arg=release_arg):
                package = f"unit-release-{os.getpid()}-{release_arg.replace('-', 'x').replace('=', 'x')}"
                release_lane = f"{package}-release"

                result = self.run_fake_cargo(
                    "-Lane",
                    "auto",
                    "cargo",
                    "build",
                    "-p",
                    package,
                    release_arg,
                )

                self.assertEqual(
                    result.returncode,
                    0,
                    f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
                )
                self.assertIn(
                    f"--target-dir {self.lane_path(release_lane)}", result.stdout
                )

    def test_gc_prunes_old_idle_lanes_but_excludes_requested_lane(self) -> None:
        requested = f"unit-keep-old-{os.getpid()}"
        victim = f"unit-victim-old-{os.getpid()}"
        requested_path = self.make_lane(requested, days_old=30)
        victim_path = self.make_lane(victim, days_old=30)

        result = self.run_script(
            "-Lane",
            requested,
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
                "CODEX_CARGO_LANE_MAX_AGE_DAYS": "1",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertTrue(requested_path.exists())
        self.assertFalse(victim_path.exists())

    def test_trash_cleanup_worker_removes_existing_trash_dirs(self) -> None:
        self.mark_lanes_root()
        for index in range(2):
            trash = self.lanes_root / f"unit-trash-{index}.trash-20260612000000000"
            trash.mkdir(parents=True)
            (trash / "payload.bin").write_bytes(b"x")

        result = self.run_script(
            "-Lane",
            f"unit-trash-cleanup-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE": "",
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

        deadline = time.time() + 20
        while time.time() < deadline:
            if (
                not list(self.lanes_root.glob("*.trash-*"))
                and not (self.lanes_root / ".cargo-lane-trash-cleanup.lock").exists()
            ):
                break
            time.sleep(0.2)

        self.assertFalse(list(self.lanes_root.glob("*.trash-*")))
        self.assertFalse((self.lanes_root / ".cargo-lane-trash-cleanup.lock").exists())

    def test_failed_gc_does_not_advance_stamp(self) -> None:
        fake_bin = self.fake_cargo_bin()
        (fake_bin / "python.cmd").write_text(
            "@echo off\r\nexit /b 7\r\n", encoding="utf-8"
        )

        result = self.run_script(
            "-Lane",
            f"unit-gc-failure-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertFalse((self.lanes_root / ".gc-stamp").exists())
        self.assertIn("leaving the GC stamp unchanged", result.stdout + result.stderr)
        self.assertTrue((self.lanes_root / ".gc-retry").is_file())

    def test_powershell_maintenance_honors_python_lock_and_retry_stamp(self):
        fake_bin = self.fake_cargo_bin()
        args_log = self.temp_root / "gc-attempts.txt"
        (fake_bin / "python.cmd").write_text(
            '@echo off\r\necho attempt >> "%CODEX_TEST_GC_ARGS_LOG%"\r\nexit /b 7\r\n',
            encoding="utf-8",
        )
        self.mark_lanes_root()
        env = {
            "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            "CODEX_TEST_GC_ARGS_LOG": str(args_log),
            "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
        }

        def run():
            result = self.run_script(
                "-Lane", "unit", "cmd.exe", "/c", "echo ok", extra_env=env
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

        handle = rust_build_status._try_acquire_binary_file_lock(
            self.lanes_root / ".lane-gc.lock"
        )
        self.assertIsNotNone(handle)
        try:
            run()
            self.assertFalse(args_log.exists())
        finally:
            rust_build_status._release_binary_file_lock(handle)
            handle.close()
        retry = self.lanes_root / ".gc-retry"
        retry.touch()
        run()
        self.assertFalse(args_log.exists())
        os.utime(retry, (1, 1))
        run()
        self.assertEqual(args_log.read_text().strip(), "attempt")
        self.assertGreater(retry.stat().st_mtime, 1)
        self.assertFalse((self.lanes_root / ".gc-stamp").exists())

    def test_gc_size_cap_evicts_oversized_idle_lane(self) -> None:
        oversized = f"unit-size-large-{os.getpid()}"
        small = f"unit-size-small-{os.getpid()}"
        oversized_path = self.make_lane(oversized, size=20, days_old=3)
        small_path = self.make_lane(small, size=10, days_old=1)

        result = self.run_script(
            "-Lane",
            f"unit-size-keep-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
                "CODEX_CARGO_LANE_MAX_AGE_DAYS": "3650",
                "CODEX_CARGO_LANE_MAX_LANE_BYTES": "15",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertFalse(oversized_path.exists())
        self.assertTrue(small_path.exists())

    def test_gc_global_cap_evicts_oldest_idle_lane(self) -> None:
        oldest_path = self.make_lane(
            f"unit-lru-oldest-{os.getpid()}", size=2 * 1024 * 1024
        )
        newest_path = self.make_lane(
            f"unit-lru-newest-{os.getpid()}", size=2 * 1024 * 1024
        )
        for path, days_old in ((oldest_path, 3), (newest_path, 1)):
            stamp = path / ".lane-last-used"
            stamp.write_text("test\n", encoding="utf-8")
            timestamp = time.time() - (days_old * 24 * 60 * 60)
            os.utime(stamp, (timestamp, timestamp))

        result = self.run_script(
            "-Lane",
            f"unit-lru-active-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
                "CODEX_CARGO_LANE_MAX_AGE_DAYS": "3650",
                "CODEX_CARGO_LANE_MAX_TOTAL_BYTES": str(3 * 1024 * 1024),
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertFalse(oldest_path.exists())
        self.assertTrue(newest_path.exists())

    def test_gc_defaults_skip_aggregate_caps(self) -> None:
        fake_bin = self.fake_cargo_bin()
        args_log = self.temp_root / "gc-args.txt"
        (fake_bin / "python.cmd").write_text(
            '@echo off\r\necho %* > "%CODEX_TEST_GC_ARGS_LOG%"\r\n',
            encoding="utf-8",
        )

        result = self.run_script(
            "-Lane",
            f"unit-default-cap-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
                "CODEX_CARGO_LANE_MAX_TOTAL_BYTES": "",
                "CODEX_CARGO_TARGET_MAX_TOTAL_BYTES": "",
                "CODEX_TEST_GC_ARGS_LOG": str(args_log),
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertNotIn(
            "--max-total-lane-bytes",
            args_log.read_text(encoding="utf-8"),
        )
        self.assertNotIn(
            "--max-total-target-bytes",
            args_log.read_text(encoding="utf-8"),
        )

    def test_completed_command_keeps_fresh_hourly_gc_stamp(self) -> None:
        fake_bin = self.fake_cargo_bin()
        args_log = self.temp_root / "post-build-gc-args.txt"
        (fake_bin / "python.cmd").write_text(
            '@echo off\r\necho %* >> "%CODEX_TEST_GC_ARGS_LOG%"\r\n',
            encoding="utf-8",
        )
        self.mark_lanes_root()
        stamp = self.lanes_root / ".gc-stamp"
        stamp.write_text("fresh\n", encoding="utf-8")

        result = self.run_script(
            "-Lane",
            f"unit-post-build-gc-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "1",
                "CODEX_CARGO_TARGET_MAX_TOTAL_BYTES": "",
                "CODEX_TEST_GC_ARGS_LOG": str(args_log),
                "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertFalse(
            args_log.exists(),
            f"command forced GC despite a fresh stamp\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(stamp.read_text(encoding="utf-8"), "fresh\n")

    def test_gc_excludes_active_lanes_from_age_pruning(self) -> None:
        active = f"unit-active-old-{os.getpid()}"
        active_path = self.make_lane(active, days_old=30)

        result = self.run_script(
            "-Lane",
            f"unit-active-run-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_ACTIVE_NAMES": active,
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "0",
                "CODEX_CARGO_LANE_MAX_AGE_DAYS": "1",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertTrue(active_path.exists())

    def test_gc_invalid_env_knobs_fall_back_to_defaults(self) -> None:
        result = self.run_script(
            "-Lane",
            f"unit-env-{os.getpid()}",
            "cmd.exe",
            "/d",
            "/c",
            "echo ok",
            extra_env={
                "CODEX_CARGO_LANE_GC_INTERVAL_HOURS": "not-a-number",
                "CODEX_CARGO_LANE_MAX_AGE_DAYS": "not-a-number",
                "CODEX_CARGO_LANE_MAX_LANE_BYTES": "not-a-number",
                "CODEX_CARGO_LANE_MAX_TOTAL_BYTES": "not-a-number",
                "CODEX_CARGO_TARGET_MAX_TOTAL_BYTES": "not-a-number",
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
