#!/usr/bin/env python3

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class BuildToolingStorageCliTest(unittest.TestCase):
    """Exercise build storage policy through the supported command boundaries."""

    maxDiff = None

    def _copy_status_runtime(self, root: Path) -> Path:
        scripts_dir = root / "scripts"
        scripts_dir.mkdir(parents=True)
        (scripts_dir / "__init__.py").write_text("", encoding="utf-8")
        for name in (
            "rust_build_status.py",
            "rust_build_status_support.py",
            "tool_versions.py",
            "cargo_lane_patterns.json",
        ):
            shutil.copy2(REPO_ROOT / "scripts" / name, scripts_dir / name)
        cargo_config = root / "codex-rs" / ".cargo" / "config.toml"
        cargo_config.parent.mkdir(parents=True)
        shutil.copy2(REPO_ROOT / "codex-rs" / ".cargo" / "config.toml", cargo_config)
        return scripts_dir / "rust_build_status.py"

    def _make_fake_bin(self, root: Path, *programs: str) -> Path:
        fake_bin = root / "fake-bin"
        fake_bin.mkdir(parents=True, exist_ok=True)
        pwsh_driver = root / "fake_pwsh_driver.py"
        pwsh_driver.write_text(
            """
import json
import os
from pathlib import Path
import sys

args_log = os.environ.get("FAKE_PWSH_ARGS_LOG")
if args_log:
    with Path(args_log).open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(sys.argv[1:]) + "\\n")
payload = os.environ.get("FAKE_PWSH_OUTPUT")
if payload:
    sys.stdout.write(Path(payload).read_text(encoding="utf-8"))
raise SystemExit(int(os.environ.get("FAKE_PWSH_EXIT", "0")))
""".lstrip(),
            encoding="utf-8",
        )
        (fake_bin / "pwsh.cmd").write_text(
            f'@echo off\n"{sys.executable}" "{pwsh_driver}" %*\nexit /b %ERRORLEVEL%\n',
            encoding="utf-8",
        )
        recorder = root / "fake_program_driver.py"
        recorder.write_text(
            """
import json
import os
from pathlib import Path
import sys

record = {
    "program": sys.argv[1],
    "argv": sys.argv[2:],
    "env": {
        name: os.environ.get(name)
        for name in (
            "CARGO_TARGET_DIR",
            "CODEX_CARGO_LANE_TARGET_DIR",
            "NEXTEST_PROFILE",
            "RUST_MIN_STACK",
        )
        if name in os.environ
    },
}
log = Path(os.environ["FAKE_PROGRAM_LOG"])
with log.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(record) + "\\n")
raise SystemExit(int(os.environ.get("FAKE_PROGRAM_EXIT", "0")))
""".lstrip(),
            encoding="utf-8",
        )
        for program in programs:
            (fake_bin / f"{program}.cmd").write_text(
                f'@echo off\n"{sys.executable}" "{recorder}" "{program}" %*\n'
                "exit /b %ERRORLEVEL%\n",
                encoding="utf-8",
            )
        return fake_bin

    def _environment(self, fake_bin: Path, **updates: str) -> dict[str, str]:
        env = os.environ.copy()
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "")
        env["PYTHONIOENCODING"] = "utf-8"
        env.pop("CARGO_TARGET_DIR", None)
        env.pop("CODEX_CARGO_LANE_TARGET_DIR", None)
        env.pop("CODEX_CARGO_LANES_ROOT", None)
        env.pop("CODEX_CARGO_LANE_ACTIVE_NAMES", None)
        env.update(updates)
        return env

    def _empty_process_file(self, root: Path) -> Path:
        output = root / "processes.json"
        output.write_text("[]", encoding="utf-8")
        return output

    def _process_file(self, root: Path, rows: list[dict[str, object]]) -> Path:
        output = root / "processes.json"
        output.write_text(json.dumps(rows), encoding="utf-8")
        return output

    def _run_status(
        self,
        script: Path,
        *arguments: str,
        env: dict[str, str] | None = None,
        cwd: Path | None = None,
        timeout: float = 40,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(script), *arguments],
            cwd=cwd or script.parent.parent,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=timeout,
        )

    def _assert_ok(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def _records(self, path: Path) -> list[dict[str, object]]:
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]

    def _create_lane(self, root: Path, name: str, size: int = 0) -> Path:
        lane = root / "codex-rs" / "target" / "lanes" / name
        lane.mkdir(parents=True)
        if size:
            (lane / "artifact.bin").write_bytes(b"x" * size)
        return lane

    def _set_tree_mtime(self, path: Path, timestamp: float) -> None:
        for child in path.rglob("*"):
            os.utime(child, (timestamp, timestamp))
        os.utime(path, (timestamp, timestamp))

    def _create_cargo_artifact(self, path: Path) -> None:
        (path / ".fingerprint").mkdir(parents=True)
        (path / "deps").mkdir()
        (path / "build").mkdir()

    def _mark_lanes_root(self, root: Path) -> None:
        root.mkdir(parents=True, exist_ok=True)
        (root / ".codex-cargo-lanes-root").write_text(
            "codex-kd cargo lanes root v1\n",
            encoding="utf-8",
        )

    def _make_junction(self, junction: Path, target: Path) -> None:
        result = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(junction), str(target)],
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=20,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def _start_lock_holder(
        self,
        root: Path,
        lock_path: Path,
    ) -> tuple[subprocess.Popen[str], Path, Path]:
        ready = root / f"{lock_path.parent.name}-{lock_path.name}.ready"
        release = root / f"{lock_path.parent.name}-{lock_path.name}.release"
        holder = root / "hold_windows_file_lock.py"
        holder.write_text(
            """
import msvcrt
from pathlib import Path
import sys
import time

lock_path, ready, release = map(Path, sys.argv[1:])
lock_path.parent.mkdir(parents=True, exist_ok=True)
with lock_path.open("a+b") as handle:
    handle.seek(0, 2)
    if handle.tell() == 0:
        handle.write(b"0")
        handle.flush()
    handle.seek(0)
    msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
    ready.write_text("ready", encoding="utf-8")
    while not release.exists():
        time.sleep(0.01)
""".lstrip(),
            encoding="utf-8",
        )
        process = subprocess.Popen(
            [sys.executable, str(holder), str(lock_path), str(ready), str(release)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            creationflags=CREATE_NO_WINDOW,
        )
        deadline = time.monotonic() + 10
        while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.01)
        if not ready.exists():
            stdout, stderr = process.communicate(timeout=5)
            self.fail(f"lock holder failed\nstdout:\n{stdout}\nstderr:\n{stderr}")
        return process, ready, release

    def _stop_lock_holder(self, process: subprocess.Popen[str], release: Path) -> None:
        release.write_text("release", encoding="utf-8")
        stdout, stderr = process.communicate(timeout=10)
        self.assertEqual(
            process.returncode,
            0,
            f"stdout:\n{stdout}\nstderr:\n{stderr}",
        )

    def test_cli_run_lane_holds_reservation_and_sanitizes_cargo_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lanes = root / "lanes"
            output = root / "child.json"
            script = (
                "import json,os,pathlib,sys; "
                "pathlib.Path(sys.argv[1]).write_text(json.dumps({"
                "'lane':os.environ.get('CODEX_CARGO_LANE_TARGET_DIR'),"
                "'cargo':os.environ.get('CARGO_TARGET_DIR')}),encoding='utf-8')"
            )
            env = os.environ.copy()
            env["CARGO_TARGET_DIR"] = str(root / "inherited")
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                script,
                str(output),
                env=env,
            )
            self._assert_ok(result)
            payload = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(Path(payload["lane"]), (lanes / "unit").resolve())
            self.assertIsNone(payload["cargo"])

            second = root / "second.txt"
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text(os.environ['CODEX_CARGO_LANE_TARGET_DIR'],encoding='utf-8')",
                str(second),
            )
            self._assert_ok(result)
            self.assertEqual(Path(second.read_text(encoding="utf-8")), (lanes / "unit").resolve())

    def test_cli_reservation_suffixes_a_concurrently_active_explicit_lane(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lanes = root / "lanes"
            first_output = root / "first.txt"
            second_output = root / "second.txt"
            release = root / "release"
            child = root / "hold_lane.py"
            child.write_text(
                """
import os
from pathlib import Path
import sys
import time

output, release = map(Path, sys.argv[1:])
output.write_text(os.environ["CODEX_CARGO_LANE_TARGET_DIR"], encoding="utf-8")
while not release.exists():
    time.sleep(0.01)
""".lstrip(),
                encoding="utf-8",
            )
            command = [
                sys.executable,
                str(REPO_ROOT / "scripts" / "rust_build_status.py"),
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                str(child),
                str(first_output),
                str(release),
            ]
            first = subprocess.Popen(
                command,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                creationflags=CREATE_NO_WINDOW,
            )
            deadline = time.monotonic() + 10
            while not first_output.exists() and first.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(first_output.exists(), "first lane child did not start")
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text(os.environ['CODEX_CARGO_LANE_TARGET_DIR'],encoding='utf-8')",
                str(second_output),
            )
            release.write_text("release", encoding="utf-8")
            stdout, stderr = first.communicate(timeout=10)
            self.assertEqual(first.returncode, 0, f"stdout:\n{stdout}\nstderr:\n{stderr}")
            self._assert_ok(result)
            self.assertEqual(Path(first_output.read_text(encoding="utf-8")).name, "unit")
            self.assertEqual(Path(second_output.read_text(encoding="utf-8")).name, "unit-2")
            self.assertIn("using 'unit-2'", result.stderr)

    def test_cli_injects_one_reserved_target_into_direct_cargo_commands(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            lanes = root / "lanes"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                "cargo",
                "nextest",
                "run",
                "-p",
                "codex-core",
                env=env,
            )
            self._assert_ok(result)
            target = str((lanes / "unit").resolve())
            record = self._records(log)[0]
            argv = record["argv"]
            self.assertEqual(argv[:4], ["nextest", "run", "--target-dir", target])
            self.assertEqual(argv.count("--target-dir"), 1)

            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                "cargo",
                "check",
                "--target-dir",
                target,
                env=env,
            )
            self._assert_ok(result)
            argv = self._records(log)[1]["argv"]
            self.assertEqual(argv.count("--target-dir"), 1)
            self.assertEqual(argv[-1], target)

    def test_cli_directly_launches_supported_nextest_recipe_with_exact_argv(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            lanes = root / "lanes"
            env = self._environment(
                fake_bin,
                FAKE_PROGRAM_LOG=str(log),
                FAKE_PROGRAM_EXIT="9",
            )
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                "just",
                "_test-lane-local-reserved",
                "-p",
                "codex-app-server",
                "test(filter with spaces)",
                env=env,
            )
            self.assertEqual(result.returncode, 9)
            record = self._records(log)[0]
            self.assertEqual(record["program"], "cargo")
            self.assertEqual(
                record["argv"],
                [
                    "nextest",
                    "run",
                    "--target-dir",
                    str((lanes / "unit").resolve()),
                    "--no-fail-fast",
                    "-p",
                    "codex-app-server",
                    "test(filter with spaces)",
                ],
            )
            self.assertEqual(record["env"]["NEXTEST_PROFILE"], "local")
            self.assertEqual(record["env"]["RUST_MIN_STACK"], "8388608")
            self.assertNotIn("CODEX_CARGO_LANE_TARGET_DIR", record["env"])

    def test_cli_keeps_just_fallback_when_core_helpers_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "just")
            log = root / "programs.jsonl"
            lanes = root / "lanes"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                "just",
                "_test-lane-fast-reserved",
                "-p",
                "codex-core",
                "test(core_filter)",
                env=env,
            )
            self._assert_ok(result)
            record = self._records(log)[0]
            self.assertEqual(record["program"], "just")
            self.assertEqual(
                record["argv"],
                ["_test-lane-fast-reserved", "-p", "codex-core", "test(core_filter)"],
            )
            self.assertEqual(
                record["env"]["CODEX_CARGO_LANE_TARGET_DIR"],
                str((lanes / "unit").resolve()),
            )
            self.assertNotIn("NEXTEST_PROFILE", record["env"])

    def test_cli_direct_routes_cover_fast_and_package_nextest_recipes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            lanes = root / "lanes"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            commands = (
                ("just", "_test-lane-fast-reserved", "-p", "codex-app-server", "filter with spaces"),
                ("just", "_test-lane-package-reserved", "codex-cli", "filter with spaces"),
            )
            for command in commands:
                result = self._run_status(
                    REPO_ROOT / "scripts" / "rust_build_status.py",
                    "run-lane",
                    "--lane",
                    "unit",
                    "--repo-root",
                    str(root / "repo"),
                    "--lanes-root",
                    str(lanes),
                    "--",
                    *command,
                    env=env,
                )
                self._assert_ok(result)
            records = self._records(log)
            self.assertEqual([record["program"] for record in records], ["cargo", "cargo"])
            self.assertEqual(records[0]["argv"][-3:], ["-p", "codex-app-server", "filter with spaces"])
            self.assertEqual(records[1]["argv"][-3:], ["-p", "codex-cli", "filter with spaces"])
            self.assertEqual([record["env"]["NEXTEST_PROFILE"] for record in records], ["fast", "fast"])

    def test_cli_parses_toolchain_and_value_taking_cargo_global_options(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            lanes = root / "lanes"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                "cargo",
                "+nightly",
                "--config",
                "profile.dev.debug=0",
                "-C",
                "codex-rs",
                "-Zunstable-options",
                "check",
                "-p",
                "codex-core",
                env=env,
            )
            self._assert_ok(result)
            argv = self._records(log)[0]["argv"]
            check_index = argv.index("check")
            self.assertEqual(
                argv[check_index + 1 : check_index + 3],
                ["--target-dir", str((lanes / "unit").resolve())],
            )
            self.assertEqual(argv[:check_index], ["+nightly", "--config", "profile.dev.debug=0", "-C", "codex-rs", "-Zunstable-options"])

    def test_cli_rejects_target_dir_outside_the_reserved_lane(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "unit",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(root / "lanes"),
                "--",
                "cargo",
                "check",
                "--target-dir=custom",
                env=env,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("does not match reserved lane", result.stderr)
            self.assertEqual(self._records(log), [])

    def test_cli_rejects_unsafe_cargo_watch_shell_and_exec_targets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            fake_bin = self._make_fake_bin(root, "cargo")
            log = root / "programs.jsonl"
            env = self._environment(fake_bin, FAKE_PROGRAM_LOG=str(log))
            cases = (
                ("-s", "cargo check"),
                ("--shell", "cargo check"),
                ("--shell=powershell",),
                ("-x", "check --target-dir custom"),
            )
            for index, tail in enumerate(cases, start=1):
                result = self._run_status(
                    REPO_ROOT / "scripts" / "rust_build_status.py",
                    "run-lane",
                    "--lane",
                    f"watch-{index}",
                    "--repo-root",
                    str(root / "repo"),
                    "--lanes-root",
                    str(root / "lanes"),
                    "--",
                    "cargo",
                    "watch",
                    *tail,
                    env=env,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual(self._records(log), [])

    def test_cargo_cli_observes_incremental_cache_disabled_by_default(self) -> None:
        result = subprocess.run(
            [
                "rustup",
                "run",
                "nightly",
                "cargo",
                "-Z",
                "unstable-options",
                "config",
                "get",
                "build.incremental",
            ],
            cwd=REPO_ROOT / "codex-rs",
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        self._assert_ok(result)
        self.assertEqual(result.stdout.strip(), "build.incremental = false")

    def test_lanes_cli_tolerates_concurrent_lane_disappearance(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            process_file = self._empty_process_file(root)
            lanes = root / "codex-rs" / "target" / "lanes"
            for index in range(300):
                lane = lanes / f"already-pruned-{index}"
                lane.mkdir(parents=True)
                (lane / "artifact").write_text("x", encoding="utf-8")
            env = self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(process_file))
            running = subprocess.Popen(
                [sys.executable, str(status), "lanes"],
                cwd=root,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                creationflags=CREATE_NO_WINDOW,
            )
            deleter = subprocess.Popen(
                [
                    sys.executable,
                    "-c",
                    "import pathlib,shutil,sys,time; time.sleep(0.02); shutil.rmtree(pathlib.Path(sys.argv[1]),ignore_errors=True)",
                    str(lanes),
                ],
                creationflags=CREATE_NO_WINDOW,
            )
            stdout, stderr = running.communicate(timeout=30)
            deleter.wait(timeout=10)
            self.assertEqual(running.returncode, 0, f"stdout:\n{stdout}\nstderr:\n{stderr}")
            self.assertIn("lane report", stdout)

    def test_doctor_cli_reports_cache_linker_and_process_contention(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root, "sccache")
            processes = self._process_file(
                root,
                [
                    {"Name": "cargo.exe", "ProcessId": 42, "CommandLine": "cargo nextest run -p codex-core"},
                    {"Name": "rustc.exe", "ProcessId": 43, "CommandLine": r"rustc --out-dir C:\repo\target\lanes\ui\debug"},
                ],
            )
            env = self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(processes))
            result = self._run_status(status, "doctor", env=env)
            self._assert_ok(result)
            self.assertIn("sccache:", result.stdout)
            self.assertIn("sccache.cmd", result.stdout.lower())
            self.assertIn("MSVC linker config x86_64-pc-windows-msvc: (unset)", result.stdout)
            self.assertIn("MSVC linker config aarch64-pc-windows-msvc: (unset)", result.stdout)
            self.assertIn("active Rust processes: 2 total, 1 shared-target, 1 lane", result.stdout)
            self.assertIn("shared-target jobs are active", result.stdout)

    def test_doctor_cli_uses_a_filtered_cim_process_query(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            args_log = root / "pwsh-args.jsonl"
            env = self._environment(
                fake_bin,
                FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                FAKE_PWSH_ARGS_LOG=str(args_log),
            )
            result = self._run_status(status, "doctor", env=env)
            self._assert_ok(result)
            invocations = self._records(args_log)
            self.assertEqual(len(invocations), 1)
            command = invocations[0][-1]
            self.assertIn("Get-CimInstance Win32_Process -Filter", command)
            self.assertIn("Name = 'cargo.exe'", command)
            self.assertIn("Name = 'pwsh.exe'", command)
            self.assertIn("$selfPid = $PID", command)
            self.assertIn("ProcessId != $selfPid", command)
            self.assertNotIn("Where-Object", command)

    def test_doctor_cli_warns_and_continues_when_process_discovery_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            env = self._environment(fake_bin, FAKE_PWSH_EXIT="7")
            result = self._run_status(status, "doctor", env=env)
            self._assert_ok(result)
            self.assertIn("warning: Windows Rust process scan failed", result.stderr)
            self.assertIn("active Rust processes: 0 total", result.stdout)

    def test_doctor_cli_ignores_cargo_path_substrings_but_detects_commands(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            processes = self._process_file(
                root,
                [
                    {"Name": "editor.exe", "ProcessId": 1, "CommandLine": r"editor C:\repo\codex-rs\Cargo.toml"},
                    {"Name": "sh.exe", "ProcessId": 2, "CommandLine": "sh -c 'cargo test'"},
                ],
            )
            result = self._run_status(
                status,
                "doctor",
                env=self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(processes)),
            )
            self._assert_ok(result)
            self.assertIn("active Rust processes: 1 total, 1 shared-target, 0 lane", result.stdout)

    def test_optimize_cli_reuses_one_process_snapshot_across_reports(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            args_log = root / "pwsh-args.jsonl"
            processes = self._process_file(
                root,
                [{"Name": "pwsh.exe", "ProcessId": 7, "CommandLine": "pwsh just cargo-lane ui cargo check"}],
            )
            env = self._environment(
                fake_bin,
                FAKE_PWSH_OUTPUT=str(processes),
                FAKE_PWSH_ARGS_LOG=str(args_log),
            )
            result = self._run_status(status, "optimize", "--dry-run", env=env)
            self._assert_ok(result)
            self.assertEqual(len(self._records(args_log)), 1)
            self.assertIn("active Rust processes: 1 total, 0 shared-target, 1 lane", result.stdout)
            self.assertIn("active lanes: ui", result.stdout)
            self.assertIn("active without directory: ui", result.stdout)

    def test_doctor_cli_reports_one_consistent_shared_process_classification(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            processes = self._process_file(
                root,
                [{"Name": "pwsh.exe", "ProcessId": 8, "CommandLine": "pwsh cargo check"}],
            )
            result = self._run_status(
                status,
                "doctor",
                env=self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(processes)),
            )
            self._assert_ok(result)
            self.assertIn("active Rust processes: 1 total, 1 shared-target, 0 lane", result.stdout)
            self.assertNotIn("active lanes:", result.stdout)
            self.assertIn("shared-target jobs are active", result.stdout)

    def test_cli_auto_lane_prefers_the_most_recent_warm_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lanes = root / "lanes"
            first_output = root / "first.txt"
            child_code = "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text(os.environ['CODEX_CARGO_LANE_TARGET_DIR'],encoding='utf-8')"
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "auto",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                child_code,
                str(first_output),
            )
            self._assert_ok(result)
            first_lane = Path(first_output.read_text(encoding="utf-8"))
            warmer = first_lane.with_name(f"{first_lane.name}-2")
            warmer.mkdir()
            future = time.time() + 30
            os.utime(warmer, (future, future))
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "auto",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                child_code,
                str(first_output),
            )
            self._assert_ok(result)
            self.assertEqual(Path(first_output.read_text(encoding="utf-8")), warmer.resolve())

    def test_disk_cli_warns_when_the_target_exceeds_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            artifact = root / "codex-rs" / "target" / "debug" / "artifact.bin"
            artifact.parent.mkdir(parents=True)
            artifact.write_bytes(b"abcd")
            env = self._environment(
                fake_bin,
                FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
            )
            result = self._run_status(status, "disk", "--warn-gib", "0.000000001", env=env)
            self._assert_ok(result)
            self.assertIn("target disk: 4 B", result.stdout)
            self.assertIn("target disk warning:", result.stdout)
            self.assertIn("just target-prune", result.stdout)

    def test_disk_cli_flags_only_unprotected_cargo_target_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            target = root / "codex-rs" / "target"
            self._create_cargo_artifact(target / "codex-core-registry-check" / "debug")
            self._create_cargo_artifact(target / "dev-small")
            (target / "schema-probe-plan").mkdir()
            result = self._run_status(
                status,
                "disk",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertIn("stray cargo target dirs: codex-core-registry-check", result.stdout)
            self.assertIn("just cargo-lane <lane>", result.stdout)
            self.assertNotIn("dev-small", result.stdout)
            self.assertNotIn("schema-probe-plan", result.stdout)

    def test_disk_cli_does_not_traverse_windows_reparse_points(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            target = root / "codex-rs" / "target"
            target.mkdir(parents=True)
            (target / "local.bin").write_bytes(b"abcd")
            outside = root / "outside"
            outside.mkdir()
            (outside / "large.bin").write_bytes(b"x" * 4096)
            self._make_junction(target / "outside-junction", outside)
            result = self._run_status(
                status,
                "disk",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertIn("target disk: 4 B", result.stdout)
            self.assertNotIn("target disk scan errors", result.stdout)

    def test_prune_cli_reports_but_preserves_stray_cargo_targets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            stray = root / "codex-rs" / "target" / "stray-build"
            self._create_cargo_artifact(stray)
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertTrue(stray.is_dir())
            self.assertIn(f"detected stray target (not auto-pruned): {stray}", result.stdout)
            self.assertIn("diagnostic-only and were preserved", result.stdout)

    def test_prune_cli_removes_only_inactive_lanes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            active = self._create_lane(root, "active", 1)
            stale = self._create_lane(root, "stale", 1)
            env = self._environment(
                fake_bin,
                FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                CODEX_CARGO_LANE_ACTIVE_NAMES="active",
            )
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=env,
            )
            self._assert_ok(result)
            self.assertTrue(active.is_dir())
            self.assertFalse(stale.exists())
            self.assertIn(f"pruned: {stale}", result.stdout)
            self.assertNotIn(f"pruned: {active}", result.stdout)

    def test_prune_cli_rejects_an_unmarked_custom_lane_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root / "runtime")
            fake_bin = self._make_fake_bin(root)
            custom = root / "custom-lanes"
            lane = custom / "keep-me"
            lane.mkdir(parents=True)
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    CODEX_CARGO_LANES_ROOT=str(custom),
                ),
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("refusing to prune unrecognized Cargo lanes root", result.stderr)
            self.assertTrue(lane.is_dir())

    def test_prune_cli_accepts_a_provenance_marked_custom_lane_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root / "runtime")
            fake_bin = self._make_fake_bin(root)
            custom = root / "custom-lanes"
            self._mark_lanes_root(custom)
            lane = custom / "stale"
            lane.mkdir()
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    CODEX_CARGO_LANES_ROOT=str(custom),
                ),
            )
            self._assert_ok(result)
            self.assertFalse(lane.exists())
            self.assertIn(f"pruned: {lane}", result.stdout)

    def test_lanes_and_prune_clis_treat_a_cargo_locked_lane_as_active(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            lane = self._create_lane(root, "locked", 1)
            holder, _ready, release = self._start_lock_holder(root, lane / ".cargo-lock")
            env = self._environment(
                fake_bin,
                FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
            )
            try:
                report = self._run_status(status, "lanes", env=env)
                self._assert_ok(report)
                self.assertIn("active: locked", report.stdout)
                result = self._run_status(
                    status,
                    "prune",
                    "--all",
                    "--skip-disk-report",
                    env=env,
                )
                self._assert_ok(result)
                self.assertTrue(lane.is_dir())
                self.assertNotIn(f"pruned: {lane}", result.stdout)
            finally:
                self._stop_lock_holder(holder, release)

    def test_prune_cli_treats_a_contended_lock_as_busy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            lane = self._create_lane(root, "busy", 1)
            holder, _ready, release = self._start_lock_holder(root, lane / ".cargo-lock")
            try:
                result = self._run_status(
                    status,
                    "prune",
                    "--all",
                    "--skip-disk-report",
                    env=self._environment(
                        fake_bin,
                        FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    ),
                )
                self._assert_ok(result)
                self.assertTrue(lane.is_dir())
                self.assertIn("no stale lanes to prune", result.stdout)
            finally:
                self._stop_lock_holder(holder, release)

    def test_destructive_prune_cli_preserves_a_lane_with_a_cargo_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            locked = self._create_lane(root, "late-cargo-lock", 1)
            removable = self._create_lane(root, "removable", 1)
            holder, _ready, release = self._start_lock_holder(root, locked / ".cargo-lock")
            try:
                result = self._run_status(
                    status,
                    "prune",
                    "--all",
                    "--skip-disk-report",
                    env=self._environment(
                        fake_bin,
                        FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    ),
                )
                self._assert_ok(result)
                self.assertTrue(locked.is_dir())
                self.assertFalse(removable.exists())
                self.assertIn(f"pruned: {removable}", result.stdout)
            finally:
                self._stop_lock_holder(holder, release)

    def test_destructive_prune_cli_preserves_an_active_reservation_lock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            reserved = self._create_lane(root, "late-reserved", 1)
            removable = self._create_lane(root, "removable", 1)
            holder, _ready, release = self._start_lock_holder(
                root,
                reserved / ".lane-active.lock",
            )
            try:
                result = self._run_status(
                    status,
                    "prune",
                    "--all",
                    "--skip-disk-report",
                    env=self._environment(
                        fake_bin,
                        FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    ),
                )
                self._assert_ok(result)
                self.assertTrue(reserved.is_dir())
                self.assertFalse(removable.exists())
                self.assertIn(f"pruned: {removable}", result.stdout)
            finally:
                self._stop_lock_holder(holder, release)

    def test_prune_cli_never_traverses_an_indirect_lane_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            lanes = root / "codex-rs" / "target" / "lanes"
            lanes.mkdir(parents=True)
            outside = root / "outside-lane"
            outside.mkdir()
            sentinel = outside / "sentinel.txt"
            sentinel.write_text("preserve", encoding="utf-8")
            junction = lanes / "indirect"
            self._make_junction(junction, outside)
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")
            self.assertTrue(junction.exists())
            self.assertNotIn(f"pruned: {junction}", result.stdout)

    def test_prune_cli_skips_an_indirect_stray_target_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            target = root / "codex-rs" / "target"
            target.mkdir(parents=True)
            outside = root / "outside-stray"
            self._create_cargo_artifact(outside)
            sentinel = outside / "sentinel.txt"
            sentinel.write_text("preserve", encoding="utf-8")
            junction = target / "stray-junction"
            self._make_junction(junction, outside)
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")
            self.assertNotIn("detected stray target", result.stdout)

    def test_prune_cli_limits_stray_reporting_to_the_target_root(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root / "runtime")
            fake_bin = self._make_fake_bin(root)
            runtime_root = status.parent.parent
            inside = runtime_root / "codex-rs" / "target" / "inside-stray"
            self._create_cargo_artifact(inside)
            outside = root / "outside-stray"
            self._create_cargo_artifact(outside)
            sentinel = outside / "sentinel.txt"
            sentinel.write_text("preserve", encoding="utf-8")
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertIn(str(inside), result.stdout)
            self.assertNotIn(str(outside), result.stdout)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "preserve")

    def test_destructive_prune_cli_never_deletes_classified_stray_targets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            stray = root / "codex-rs" / "target" / "classified-stray"
            self._create_cargo_artifact(stray)
            sentinel = stray / "keep.txt"
            sentinel.write_text("keep", encoding="utf-8")
            result = self._run_status(
                status,
                "prune",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep")
            self.assertIn(f"detected stray target (not auto-pruned): {stray}", result.stdout)

    def test_prune_cli_keeps_two_warm_lanes_per_base(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            kept = [self._create_lane(root, "codex-core", 1), self._create_lane(root, "codex-core-2", 1)]
            removed = self._create_lane(root, "codex-core-3", 1)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "2",
                "--max-age-days",
                "100000",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertTrue(all(path.is_dir() for path in kept))
            self.assertFalse(removed.exists())
            self.assertIn(f"pruned: {removed}", result.stdout)

    def test_prune_cli_removes_timestamped_lanes_even_with_warm_capacity(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            stable = self._create_lane(root, "codex-core", 1)
            timestamped = self._create_lane(root, "codex-core-20260608183755", 1)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "2",
                "--max-age-days",
                "100000",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertTrue(stable.is_dir())
            self.assertFalse(timestamped.exists())
            self.assertIn(f"pruned: {timestamped}", result.stdout)

    def test_prune_cli_removes_lanes_over_the_age_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            old = self._create_lane(root, "old", 1)
            fresh = self._create_lane(root, "fresh", 1)
            now = time.time()
            self._set_tree_mtime(old, now - 3 * 86400)
            self._set_tree_mtime(fresh, now)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "2",
                "--max-age-days",
                "1",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertFalse(old.exists())
            self.assertTrue(fresh.is_dir())
            self.assertIn(f"pruned: {old}", result.stdout)

    def test_prune_cli_applies_warm_selection_before_size_policy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            protected = self._create_lane(root, "codex-core", 1)
            warm_victim = self._create_lane(root, "codex-core-2", 1)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "1",
                "--max-age-days",
                "100000",
                "--max-lane-bytes",
                "10",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertTrue(protected.is_dir())
            self.assertFalse(warm_victim.exists())
            self.assertIn(f"pruned: {warm_victim}", result.stdout)

    def test_prune_cli_applies_the_global_lane_ceiling_by_inactive_lru(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            oldest = self._create_lane(root, "oldest", 60)
            newest = self._create_lane(root, "newest", 60)
            active = self._create_lane(root, "active", 60)
            now = time.time()
            self._set_tree_mtime(oldest, now - 300)
            self._set_tree_mtime(newest, now - 200)
            self._set_tree_mtime(active, now - 100)
            processes = self._process_file(
                root,
                [{"Name": "rustc.exe", "ProcessId": 7, "CommandLine": f"rustc --out-dir {active}\\debug"}],
            )
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "1",
                "--max-age-days",
                "100000",
                "--max-total-lane-bytes",
                "120",
                "--skip-disk-report",
                env=self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(processes)),
            )
            self._assert_ok(result)
            self.assertFalse(oldest.exists())
            self.assertTrue(newest.is_dir())
            self.assertTrue(active.is_dir())
            self.assertIn(f"pruned: {oldest}", result.stdout)

    def test_global_ceiling_accounts_for_lanes_already_selected_by_policy(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            protected = self._create_lane(root, "codex-core", 60)
            warm_victim = self._create_lane(root, "codex-core-2", 60)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "1",
                "--max-age-days",
                "100000",
                "--max-total-lane-bytes",
                "60",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertTrue(protected.is_dir())
            self.assertFalse(warm_victim.exists())
            self.assertEqual(result.stdout.count("pruned:"), 1)

    def test_target_ceiling_subtracts_non_lane_target_usage(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            oldest = self._create_lane(root, "oldest", 60)
            newest = self._create_lane(root, "newest", 60)
            debug = root / "codex-rs" / "target" / "debug"
            debug.mkdir()
            (debug / "artifact.bin").write_bytes(b"x" * 80)
            now = time.time()
            self._set_tree_mtime(oldest, now - 200)
            self._set_tree_mtime(newest, now - 100)
            result = self._run_status(
                status,
                "prune",
                "--keep-warm-per-base",
                "1",
                "--max-age-days",
                "100000",
                "--max-total-target-bytes",
                "140",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertFalse(oldest.exists())
            self.assertTrue(newest.is_dir())
            self.assertIn(f"pruned: {oldest}", result.stdout)

    def test_prune_cli_can_skip_the_disk_report(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            stale = self._create_lane(root, "stale", 1)
            result = self._run_status(
                status,
                "prune",
                "--dry-run",
                "--all",
                "--skip-disk-report",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            self.assertIn(f"would prune: {stale}", result.stdout)
            self.assertNotIn("target root:", result.stdout)
            self.assertTrue(stale.is_dir())

    def test_prune_plan_cli_bounds_large_worker_requests_and_stays_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            now = time.time()
            lanes = []
            for index in range(12):
                lane = self._create_lane(root, f"lane-{index:02d}", 1)
                self._set_tree_mtime(lane, now - (12 - index))
                lanes.append(lane)
            result = self._run_status(
                status,
                "prune",
                "--json-plan",
                "--keep-warm-per-base",
                "1",
                "--max-age-days",
                "100000",
                "--max-total-lane-bytes",
                "5",
                "--size-workers",
                "99",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                ),
            )
            self._assert_ok(result)
            plan = json.loads(result.stdout)
            self.assertEqual(plan["type"], "codexKdCargoLanePrunePlan")
            self.assertEqual(plan["lanes"], [str(path) for path in lanes[1:]])
            self.assertTrue(all(path.is_dir() for path in lanes))

    def test_prune_cli_rejects_negative_or_zero_destructive_budgets(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            env = self._environment(fake_bin)
            for option, value in (
                ("--keep-warm-per-base", "-1"),
                ("--max-age-days", "-1"),
                ("--max-lane-gib", "-1"),
                ("--max-lane-bytes", "-1"),
                ("--max-total-lane-gib", "-1"),
                ("--max-total-lane-bytes", "-1"),
                ("--max-total-target-gib", "-1"),
                ("--max-total-target-bytes", "-1"),
                ("--size-workers", "0"),
            ):
                with self.subTest(option=option):
                    result = self._run_status(status, "prune", option, value, env=env)
                    self.assertEqual(result.returncode, 2)
                    self.assertIn("error:", result.stderr)

    def test_doctor_cli_recognizes_every_shared_lane_command_pattern(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            command_lines = [
                r"cargo check --target-dir C:\repo\target\lanes\path-lane",
                "powershell -File scripts/cargo-lane.ps1 -Lane script-lane cargo check",
                "just watch-lane recipe-lane",
                "just test-lane-main",
                "just release-lane",
            ]
            processes = self._process_file(
                root,
                [
                    {"Name": "pwsh.exe", "ProcessId": index, "CommandLine": command}
                    for index, command in enumerate(command_lines, start=1)
                ],
            )
            result = self._run_status(
                status,
                "doctor",
                env=self._environment(fake_bin, FAKE_PWSH_OUTPUT=str(processes)),
            )
            self._assert_ok(result)
            self.assertIn(
                "active lanes: main, path-lane, recipe-lane, release, script-lane",
                result.stdout,
            )
            self.assertIn("active Rust processes: 5 total, 0 shared-target, 5 lane", result.stdout)

    def test_public_cargo_lane_main_recipe_uses_the_parameterized_lane(self) -> None:
        dry_run = subprocess.run(
            ["just", "--dry-run", "cargo-lane", "main", "cargo", "check"],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        self._assert_ok(dry_run)
        rendered = dry_run.stdout + dry_run.stderr
        self.assertIn('run-lane --lane "main"', rendered)
        self.assertNotIn("cargo-lane-main", rendered)

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            output = root / "lane.txt"
            lanes = root / "lanes"
            result = self._run_status(
                REPO_ROOT / "scripts" / "rust_build_status.py",
                "run-lane",
                "--lane",
                "main",
                "--repo-root",
                str(root / "repo"),
                "--lanes-root",
                str(lanes),
                "--",
                sys.executable,
                "-c",
                "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text(os.environ['CODEX_CARGO_LANE_TARGET_DIR'],encoding='utf-8')",
                str(output),
            )
            self._assert_ok(result)
            self.assertEqual(Path(output.read_text(encoding="utf-8")).name, "main")

    def test_pattern_registry_drives_python_diagnostics_and_powershell_runtime(self) -> None:
        shell = shutil.which("pwsh") or shutil.which("powershell")
        self.assertIsNotNone(shell, "PowerShell is required for Windows build tooling")
        assert shell is not None
        command_lines = [
            r"cargo check --target-dir C:\repo\target\lanes\path-lane",
            "powershell -File scripts/cargo-lane.ps1 -Lane script-lane cargo check",
            "just watch-lane recipe-lane",
            "just test-lane-main",
            "just release-lane",
        ]
        pattern_script = REPO_ROOT / "scripts" / "cargo-lane-patterns.ps1"
        command = (
            f". '{str(pattern_script).replace("'", "''")}'; "
            f"$commandLines = ConvertFrom-Json '{json.dumps(command_lines).replace("'", "''")}'; "
            "$names = @(Get-CargoLaneNamesFromCommandLines -CommandLines $commandLines); "
            "ConvertTo-Json -Compress -InputObject $names"
        )
        powershell_result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            cwd=REPO_ROOT,
            text=True,
            capture_output=True,
            check=False,
            creationflags=CREATE_NO_WINDOW,
            timeout=30,
        )
        self._assert_ok(powershell_result)
        self.assertEqual(
            set(json.loads(powershell_result.stdout)),
            {"path-lane", "script-lane", "recipe-lane", "main", "release"},
        )

        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            lanes = root / "powershell-lanes"
            output = root / "wrapper.txt"
            child = (
                "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text("
                "os.environ['CODEX_CARGO_LANE_TARGET_DIR'],encoding='utf-8')"
            )
            env = os.environ.copy()
            env["RUSTC_WRAPPER"] = "disabled-for-test"
            env["CODEX_CARGO_LANE_ACTIVE_NAMES"] = "unrelated"
            wrapper_result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    str(REPO_ROOT / "scripts" / "cargo-lane.ps1"),
                    "-Lane",
                    "registry-runtime",
                    "-LanesRoot",
                    str(lanes),
                    "--",
                    sys.executable,
                    "-c",
                    child,
                    str(output),
                ],
                cwd=REPO_ROOT,
                env=env,
                text=True,
                capture_output=True,
                check=False,
                creationflags=CREATE_NO_WINDOW,
                timeout=40,
            )
            self._assert_ok(wrapper_result)
            self.assertEqual(
                Path(output.read_text(encoding="utf-8")),
                (lanes / "registry-runtime").resolve(),
            )

    def test_lanes_cli_marks_active_warm_and_prunable_lanes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            self._create_lane(root, "stale", 1)
            self._create_lane(root, "stale-2", 1)
            self._create_lane(root, "active", 1)
            result = self._run_status(
                status,
                "lanes",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    CODEX_CARGO_LANE_ACTIVE_NAMES="active",
                ),
            )
            self._assert_ok(result)
            self.assertIn("active: active", result.stdout)
            self.assertIn("stale: stale, stale-2", result.stdout)
            self.assertIn("warm-protected: stale", result.stdout)
            self.assertIn("prunable:", result.stdout)
            self.assertIn("  stale-2", result.stdout)
            self.assertIn("safe prune suggestions:", result.stdout)
            self.assertIn("just target-prune", result.stdout)
            self.assertNotIn("Remove-Item -Recurse -Force", result.stdout)

    def test_doctor_cli_displays_a_reserved_lane_without_a_process(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            status = self._copy_status_runtime(root)
            fake_bin = self._make_fake_bin(root)
            result = self._run_status(
                status,
                "doctor",
                env=self._environment(
                    fake_bin,
                    FAKE_PWSH_OUTPUT=str(self._empty_process_file(root)),
                    CODEX_CARGO_LANE_ACTIVE_NAMES="reserved",
                ),
            )
            self._assert_ok(result)
            self.assertIn("active Rust processes: 0 total", result.stdout)
            self.assertIn("active lanes: reserved", result.stdout)
            self.assertIn("active without directory: reserved", result.stdout)


if __name__ == "__main__":
    unittest.main()
