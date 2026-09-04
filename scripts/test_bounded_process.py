from __future__ import annotations

import ctypes
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import root_maintenance as ROOT_MAINTENANCE
from scripts import bounded_process as BOUNDED_PROCESS
from scripts.bounded_process import run_bounded_process
from scripts.root_maintenance import test_modules_for_changed_path


def _pid_is_running(pid: int) -> bool:
    if os.name == "nt":
        synchronize = 0x00100000
        wait_timeout = 0x00000102
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        handle = kernel32.OpenProcess(synchronize, False, pid)
        if not handle:
            return False
        try:
            return kernel32.WaitForSingleObject(handle, 0) == wait_timeout
        finally:
            kernel32.CloseHandle(handle)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _wait_for_pid_exit(test: unittest.TestCase, pid: int) -> None:
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not _pid_is_running(pid):
            return
        time.sleep(0.02)
    test.fail(f"descendant process {pid} survived bounded cleanup")


class BoundedProcessTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="bounded-process-test-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)

    def _run(
        self,
        source: str,
        *,
        timeout: float = 5,
        stdin_bytes: bytes | None = None,
        stdout_limit: int = 1024 * 1024,
        stderr_limit: int = 1024 * 1024,
    ):
        return run_bounded_process(
            [sys.executable, "-c", source],
            cwd=self.base,
            env=os.environ.copy(),
            timeout_seconds=timeout,
            stdin_bytes=stdin_bytes,
            stdout_limit_bytes=stdout_limit,
            stderr_limit_bytes=stderr_limit,
        )

    def test_normal_root_exit_sweeps_descendant_holding_output_pipes(self) -> None:
        source = (
            "import subprocess,sys; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); "
            "print(child.pid, flush=True)"
        )
        started = time.monotonic()
        result = self._run(source)
        elapsed = time.monotonic() - started

        self.assertEqual(result.returncode, 0, result.supervision_error)
        self.assertIsNone(result.supervision_error)
        self.assertLess(elapsed, 4)
        descendant_pid = int(result.stdout.strip())
        _wait_for_pid_exit(self, descendant_pid)

    def test_timeout_kills_the_whole_process_tree(self) -> None:
        pid_file = self.base / "timeout-child.pid"
        source = (
            "import pathlib,subprocess,sys,time; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); "
            f"pathlib.Path({str(pid_file)!r}).write_text(str(child.pid)); "
            "time.sleep(30)"
        )
        started = time.monotonic()
        result = self._run(source, timeout=0.75)

        self.assertIsNone(result.returncode)
        self.assertIn("timed out", result.supervision_error or "")
        self.assertLess(time.monotonic() - started, 2)
        descendant_pid = int(pid_file.read_text(encoding="utf-8"))
        _wait_for_pid_exit(self, descendant_pid)

    def test_output_cap_is_a_hard_error_and_kills_descendants(self) -> None:
        pid_file = self.base / "overflow-child.pid"
        source = (
            "import pathlib,subprocess,sys,time; "
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); "
            f"pathlib.Path({str(pid_file)!r}).write_text(str(child.pid)); "
            "sys.stdout.buffer.write(b'x'*8192); sys.stdout.buffer.flush(); time.sleep(30)"
        )
        result = self._run(source, stdout_limit=1024)

        self.assertIsNone(result.returncode)
        self.assertIn("stdout exceeded", result.supervision_error or "")
        self.assertEqual(len(result.stdout), 1024)
        descendant_pid = int(pid_file.read_text(encoding="utf-8"))
        _wait_for_pid_exit(self, descendant_pid)

    def test_stdin_is_devnull_by_default_and_explicit_when_requested(self) -> None:
        source = "import sys; data=sys.stdin.buffer.read(); print(len(data))"
        without_input = self._run(source)
        with_input = self._run(source, stdin_bytes=b"bounded input")

        self.assertEqual(without_input.returncode, 0, without_input.supervision_error)
        self.assertEqual(without_input.stdout.strip(), b"0")
        self.assertEqual(with_input.returncode, 0, with_input.supervision_error)
        self.assertEqual(with_input.stdout.strip(), b"13")

    def test_root_maintenance_routes_helper_to_direct_process_tests(self) -> None:
        self.assertEqual(
            test_modules_for_changed_path("scripts/bounded_process.py"),
            ("scripts.test_bounded_process",),
        )

    def test_tracked_script_discovery_fails_closed_on_bytes_and_read_errors(self) -> None:
        invalid_utf8 = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=b"\xff\0",
            stderr=b"",
        )
        with (
            mock.patch.object(
                ROOT_MAINTENANCE.subprocess,
                "run",
                return_value=invalid_utf8,
            ),
            self.assertRaisesRegex(
                ROOT_MAINTENANCE.ScriptInventoryDiscoveryError,
                "non-UTF-8",
            ),
        ):
            ROOT_MAINTENANCE.tracked_script_entrypoints()

        unreadable = subprocess.CompletedProcess(
            args=[],
            returncode=0,
            stdout=b"100755 " + b"0" * 40 + b" 0\toutside-script\0",
            stderr=b"",
        )
        with (
            mock.patch.object(
                ROOT_MAINTENANCE.subprocess,
                "run",
                return_value=unreadable,
            ),
            mock.patch.object(Path, "open", side_effect=OSError("denied")),
            self.assertRaisesRegex(
                ROOT_MAINTENANCE.ScriptInventoryDiscoveryError,
                "could not read outside-script",
            ),
        ):
            ROOT_MAINTENANCE.tracked_script_entrypoints()

    @unittest.skipUnless(os.name == "nt", "Windows Job lifecycle test")
    def test_windows_launch_failure_closes_the_job_owner(self) -> None:
        owners = []

        class FailingJob:
            def __init__(self) -> None:
                self.close_count = 0
                self.terminate_count = 0
                owners.append(self)

            def attach(self, _process: subprocess.Popen[bytes]) -> None:
                return None

            def resume(self, _pid: int) -> None:
                raise OSError("injected resume failure")

            def terminate(self) -> None:
                self.terminate_count += 1

            def close(self) -> None:
                self.close_count += 1

        with mock.patch.object(BOUNDED_PROCESS, "_WindowsJob", FailingJob):
            result = self._run("print('must remain suspended')")

        self.assertIsNone(result.returncode)
        self.assertIn("injected resume failure", result.supervision_error or "")
        self.assertEqual(len(owners), 1)
        self.assertGreaterEqual(owners[0].terminate_count, 1)
        self.assertGreaterEqual(owners[0].close_count, 1)

    @unittest.skipUnless(os.name == "nt", "Windows delayed launch lifecycle test")
    def test_windows_delayed_launch_timeout_closes_the_job_owner(self) -> None:
        original_popen = BOUNDED_PROCESS.subprocess.Popen
        owners = []

        class TrackingJob(BOUNDED_PROCESS._WindowsJob):
            def __init__(self) -> None:
                super().__init__()
                self.close_count = 0
                owners.append(self)

            def close(self) -> None:
                self.close_count += 1
                super().close()

        def delayed_popen(*args, **kwargs):
            time.sleep(0.25)
            return original_popen(*args, **kwargs)

        with (
            mock.patch.object(
                BOUNDED_PROCESS,
                "_WindowsJob",
                TrackingJob,
            ),
            mock.patch.object(
                BOUNDED_PROCESS.subprocess,
                "Popen",
                side_effect=delayed_popen,
            ),
        ):
            result = self._run(
                "print('a timed-out suspended child must never run')",
                timeout=0.05,
            )

        self.assertIsNone(result.returncode)
        self.assertIn("timed out", result.supervision_error or "")
        self.assertEqual(len(owners), 1)
        self.assertGreaterEqual(owners[0].close_count, 1)
        self.assertGreater(result.pid, 0)
        _wait_for_pid_exit(self, result.pid)

    @unittest.skipUnless(os.name == "nt", "Windows stalled Popen lifecycle test")
    def test_windows_popen_stall_beyond_cleanup_grace_cannot_leak_job(self) -> None:
        original_popen = BOUNDED_PROCESS.subprocess.Popen
        owners = []
        late_processes: list[subprocess.Popen[bytes]] = []

        class TrackingJob(BOUNDED_PROCESS._WindowsJob):
            def __init__(self) -> None:
                super().__init__()
                self.closed = threading.Event()
                owners.append(self)

            def close(self) -> None:
                super().close()
                self.closed.set()

        def stalled_popen(*args, **kwargs):
            time.sleep(0.4)
            process = original_popen(*args, **kwargs)
            late_processes.append(process)
            return process

        with (
            mock.patch.object(BOUNDED_PROCESS, "_WindowsJob", TrackingJob),
            mock.patch.object(
                BOUNDED_PROCESS.subprocess,
                "Popen",
                side_effect=stalled_popen,
            ),
            mock.patch.object(BOUNDED_PROCESS, "_CLEANUP_GRACE_SECONDS", 0.1),
        ):
            result = self._run(
                "print('a late suspended child must never run')",
                timeout=0.05,
            )

        self.assertIsNone(result.returncode)
        self.assertIn("runner launcher did not stop", result.supervision_error or "")
        self.assertEqual(len(owners), 1)
        self.assertTrue(owners[0].closed.is_set())
        deadline = time.monotonic() + 2
        while not late_processes and time.monotonic() < deadline:
            time.sleep(0.02)
        self.assertEqual(len(late_processes), 1)
        _wait_for_pid_exit(self, late_processes[0].pid)


if __name__ == "__main__":
    unittest.main()
