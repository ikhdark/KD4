#!/usr/bin/env python3

from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
SCRIPT = REPO_ROOT / "scripts" / "cargo-lane.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def powershell() -> str | None:
    return shutil.which("pwsh") or shutil.which("powershell")


@unittest.skipUnless(os.name == "nt", "cargo-lane is Windows-only")
class CargoLaneTest(unittest.TestCase):
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
    ) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        if extra_env:
            env.update(extra_env)
        env.setdefault("CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE", "1")
        return subprocess.run(
            [
                self.shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(SCRIPT),
                "-LanesRoot",
                str(self.lanes_root),
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
        path = self.lanes_root / lane
        path.mkdir(parents=True, exist_ok=True)
        if size > 0:
            (path / "payload.bin").write_bytes(b"x" * size)
        if days_old > 0:
            timestamp = time.time() - (days_old * 24 * 60 * 60)
            os.utime(path, (timestamp, timestamp))
        return path

    def hold_lane_lock(
        self,
        lane: str,
        *,
        lock_name: str = ".cargo-lock",
    ) -> subprocess.Popen[str]:
        lane_path = self.make_lane(lane)
        lock_path = lane_path / lock_name
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
            "echo linker=%CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER%",
            extra_env={
                "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER": "",
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
        self.assertIn(f"linker={lld_link}", result.stdout)

    def test_auto_lane_uses_package_name_for_stable_cache_affinity(self) -> None:
        package = f"unit-core-{os.getpid()}"

        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "check",
            "-p",
            package,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {self.lane_path(package)}", result.stdout)

    def test_existing_cargo_target_dir_is_not_duplicated(self) -> None:
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

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {explicit_target}", result.stdout)
        self.assertNotIn(str(self.lanes_root), result.stdout)

    def test_existing_equals_cargo_target_dir_is_not_duplicated(self) -> None:
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

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir={explicit_target}", result.stdout)
        self.assertNotIn(str(self.lanes_root), result.stdout)

    def test_existing_nextest_target_dir_is_not_duplicated(self) -> None:
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

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn(f"--target-dir {explicit_target}", result.stdout)
        self.assertNotIn(str(self.lanes_root), result.stdout)

    def test_auto_lane_reuses_warm_idle_suffix_when_base_lane_is_active(self) -> None:
        package = f"unit-core-active-{os.getpid()}"
        warm_suffix = f"{package}-2"
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

    def test_cargo_watch_shell_command_is_not_rewritten(self) -> None:
        result = self.run_fake_cargo(
            "-Lane",
            "auto",
            "cargo",
            "watch",
            "-s",
            "cargo check",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertNotIn("--target-dir", result.stdout)

    def test_timestamped_explicit_lane_is_normalized_for_cache_reuse(self) -> None:
        lane = f"unit-stable-{os.getpid()}-20260608183755"
        stable_lane = lane.removesuffix("-20260608183755")

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
        self.assertIn(f"--target-dir {self.lane_path(stable_lane)}", result.stdout)
        self.assertNotIn("20260608183755", result.stdout)

    def test_isolated_cargo_home_preserves_user_config_and_adds_sccache(self) -> None:
        lane = f"unit-cargo-home-{os.getpid()}"
        user_profile = self.temp_root / "user"
        cargo_config = user_profile / ".cargo" / "config.toml"
        cargo_config.parent.mkdir(parents=True)
        cargo_config.write_text("[net]\nretry = 3\n", encoding="utf-8")
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
            "echo %CARGO_HOME% %SCCACHE_BASEDIR% %SCCACHE_CACHE_SIZE%",
            extra_env={
                "LOCALAPPDATA": str(local_app_data),
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
        self.assertIn("[net]", config_text)
        self.assertIn("retry = 3", config_text)
        self.assertIn('rustc-wrapper = "sccache"', config_text)
        self.assertIn(str(REPO_ROOT), result.stdout)
        self.assertIn("80G", result.stdout)

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
            },
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
