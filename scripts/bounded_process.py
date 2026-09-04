from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Mapping, Sequence


DEFAULT_STDOUT_LIMIT_BYTES = 128 * 1024 * 1024
DEFAULT_STDERR_LIMIT_BYTES = 32 * 1024 * 1024
_IO_CHUNK_BYTES = 64 * 1024
_CLEANUP_GRACE_SECONDS = 5.0


@dataclass(frozen=True)
class BoundedProcessResult:
    returncode: int | None
    stdout: bytes
    stderr: bytes
    pid: int
    supervision_error: str | None = None


class _LaunchState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.done = threading.Event()
        self.stop = threading.Event()
        self.process: subprocess.Popen[bytes] | None = None
        self.owner: _WindowsJob | None = None
        self.error: str | None = None


if os.name == "nt":
    import ctypes
    from ctypes import wintypes

    class _BasicLimitInformation(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class _IoCounters(ctypes.Structure):
        _fields_ = [
            ("ReadOperationCount", ctypes.c_ulonglong),
            ("WriteOperationCount", ctypes.c_ulonglong),
            ("OtherOperationCount", ctypes.c_ulonglong),
            ("ReadTransferCount", ctypes.c_ulonglong),
            ("WriteTransferCount", ctypes.c_ulonglong),
            ("OtherTransferCount", ctypes.c_ulonglong),
        ]

    class _ExtendedLimitInformation(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", _BasicLimitInformation),
            ("IoInfo", _IoCounters),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    class _ThreadEntry32(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ThreadID", wintypes.DWORD),
            ("th32OwnerProcessID", wintypes.DWORD),
            ("tpBasePri", wintypes.LONG),
            ("tpDeltaPri", wintypes.LONG),
            ("dwFlags", wintypes.DWORD),
        ]

    _KERNEL32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _KERNEL32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    _KERNEL32.CreateJobObjectW.restype = wintypes.HANDLE
    _KERNEL32.SetInformationJobObject.argtypes = [
        wintypes.HANDLE,
        ctypes.c_int,
        ctypes.c_void_p,
        wintypes.DWORD,
    ]
    _KERNEL32.SetInformationJobObject.restype = wintypes.BOOL
    _KERNEL32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    _KERNEL32.AssignProcessToJobObject.restype = wintypes.BOOL
    _KERNEL32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
    _KERNEL32.TerminateJobObject.restype = wintypes.BOOL
    _KERNEL32.CloseHandle.argtypes = [wintypes.HANDLE]
    _KERNEL32.CloseHandle.restype = wintypes.BOOL
    _KERNEL32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    _KERNEL32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    _KERNEL32.Thread32First.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(_ThreadEntry32),
    ]
    _KERNEL32.Thread32First.restype = wintypes.BOOL
    _KERNEL32.Thread32Next.argtypes = [
        wintypes.HANDLE,
        ctypes.POINTER(_ThreadEntry32),
    ]
    _KERNEL32.Thread32Next.restype = wintypes.BOOL
    _KERNEL32.OpenThread.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    _KERNEL32.OpenThread.restype = wintypes.HANDLE
    _KERNEL32.ResumeThread.argtypes = [wintypes.HANDLE]
    _KERNEL32.ResumeThread.restype = wintypes.DWORD


class _WindowsJob:
    def __init__(self) -> None:
        if os.name != "nt":
            raise RuntimeError("Windows Job Objects are unavailable on this platform")
        handle = _KERNEL32.CreateJobObjectW(None, None)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        information = _ExtendedLimitInformation()
        # KILL_ON_JOB_CLOSE, deliberately without BREAKAWAY_OK.
        information.BasicLimitInformation.LimitFlags = 0x00002000
        if not _KERNEL32.SetInformationJobObject(
            handle,
            9,
            ctypes.byref(information),
            ctypes.sizeof(information),
        ):
            error = ctypes.WinError(ctypes.get_last_error())
            _KERNEL32.CloseHandle(handle)
            raise error
        self._handle = handle
        self._lock = threading.Lock()

    def attach(self, process: subprocess.Popen[bytes]) -> None:
        with self._lock:
            if self._handle is None:
                raise RuntimeError("runner Job Object was already closed")
            if not _KERNEL32.AssignProcessToJobObject(
                self._handle,
                wintypes.HANDLE(process._handle),  # type: ignore[attr-defined]
            ):
                raise ctypes.WinError(ctypes.get_last_error())

    def resume(self, pid: int) -> None:
        invalid_handle = ctypes.c_void_p(-1).value
        snapshot = _KERNEL32.CreateToolhelp32Snapshot(0x00000004, 0)
        if not snapshot or snapshot == invalid_handle:
            raise ctypes.WinError(ctypes.get_last_error())
        resumed = 0
        try:
            entry = _ThreadEntry32()
            entry.dwSize = ctypes.sizeof(entry)
            present = bool(_KERNEL32.Thread32First(snapshot, ctypes.byref(entry)))
            while present:
                if entry.th32OwnerProcessID == pid:
                    thread = _KERNEL32.OpenThread(0x0002, False, entry.th32ThreadID)
                    if not thread:
                        raise ctypes.WinError(ctypes.get_last_error())
                    try:
                        if _KERNEL32.ResumeThread(thread) == 0xFFFFFFFF:
                            raise ctypes.WinError(ctypes.get_last_error())
                        resumed += 1
                    finally:
                        _KERNEL32.CloseHandle(thread)
                present = bool(_KERNEL32.Thread32Next(snapshot, ctypes.byref(entry)))
        finally:
            _KERNEL32.CloseHandle(snapshot)
        if resumed == 0:
            raise RuntimeError(f"no thread found for suspended process {pid}")

    def terminate(self) -> None:
        with self._lock:
            if self._handle and not _KERNEL32.TerminateJobObject(self._handle, 1):
                raise ctypes.WinError(ctypes.get_last_error())

    def close(self) -> None:
        with self._lock:
            if self._handle:
                handle = self._handle
                self._handle = None
                if not _KERNEL32.CloseHandle(handle):
                    raise ctypes.WinError(ctypes.get_last_error())


def _launch_options() -> dict[str, object]:
    if os.name == "nt":
        return {
            "creationflags": (
                getattr(subprocess, "CREATE_NO_WINDOW", 0)
                | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
                | getattr(subprocess, "CREATE_SUSPENDED", 0x00000004)
            )
        }
    return {"start_new_session": True}


def _terminate_tree(
    process: subprocess.Popen[bytes] | None,
    owner: _WindowsJob | None,
) -> str | None:
    errors: list[str] = []
    if os.name == "nt":
        if owner is not None:
            try:
                owner.terminate()
            except OSError as error:
                errors.append(f"cannot terminate runner Job Object: {error}")
    elif process is not None:
        try:
            # start_new_session=True makes the root pid the stable process-group id.
            # This still reaches ordinary descendants after the root has exited.
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        except OSError as error:
            errors.append(f"cannot terminate runner process group: {error}")

    if process is not None and process.poll() is None:
        try:
            process.kill()
        except OSError as error:
            errors.append(f"cannot terminate runner root process: {error}")
    return "; ".join(errors) if errors else None


def _launch(
    state: _LaunchState,
    *,
    command: list[str],
    cwd: Path,
    env: Mapping[str, str],
    has_stdin: bool,
) -> None:
    process: subprocess.Popen[bytes] | None = None
    owner: _WindowsJob | None = None
    try:
        if state.stop.is_set():
            raise TimeoutError("runner launch was cancelled before process creation")
        if os.name == "nt":
            owner = _WindowsJob()
            # Publish the Job before the potentially blocking process creation so
            # a caller-side deadline can close it even if Popen itself stalls.
            with state.lock:
                state.owner = owner
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=dict(env),
            stdin=subprocess.PIPE if has_stdin else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            **_launch_options(),
        )
        with state.lock:
            state.process = process
            if owner is not None:
                state.owner = owner
        if os.name == "nt":
            assert owner is not None
            owner.attach(process)
            if state.stop.is_set():
                owner.terminate()
                raise TimeoutError("runner launch was cancelled before process resume")
            owner.resume(process.pid)
        elif state.stop.is_set():
            _terminate_tree(process, None)
            raise TimeoutError("runner launch was cancelled after process creation")
    except BaseException as error:
        cleanup_error = _terminate_tree(process, owner)
        finalization_error = _finalize_root(
            process,
            owner,
            time.monotonic() + _CLEANUP_GRACE_SECONDS,
        )
        if finalization_error:
            cleanup_error = (
                f"{cleanup_error}; {finalization_error}"
                if cleanup_error
                else finalization_error
            )
        with state.lock:
            state.error = f"cannot launch runner: {error}"
            if cleanup_error:
                state.error += f"; {cleanup_error}"
    finally:
        state.done.set()


def _read_bounded(
    stream: BinaryIO,
    *,
    label: str,
    limit: int,
    chunks: list[bytes],
    failure: list[str],
    failure_lock: threading.Lock,
    failure_event: threading.Event,
) -> None:
    retained = 0
    try:
        while True:
            remaining = limit - retained
            chunk = os.read(stream.fileno(), min(_IO_CHUNK_BYTES, remaining + 1))
            if not chunk:
                return
            if len(chunk) > remaining:
                if remaining:
                    chunks.append(chunk[:remaining])
                with failure_lock:
                    if not failure:
                        failure.append(
                            f"runner {label} exceeded the {limit}-byte output limit"
                        )
                failure_event.set()
                return
            chunks.append(chunk)
            retained += len(chunk)
    except OSError as error:
        with failure_lock:
            if not failure:
                failure.append(f"cannot read runner {label}: {error}")
        failure_event.set()


def _write_stdin(
    stream: BinaryIO,
    content: bytes,
    failure: list[str],
    failure_lock: threading.Lock,
    failure_event: threading.Event,
    stop: threading.Event,
) -> None:
    view = memoryview(content)
    try:
        while view and not stop.is_set():
            written = stream.write(view[:_IO_CHUNK_BYTES])
            if written is None or written <= 0:
                raise OSError("runner stdin accepted zero bytes")
            view = view[written:]
        stream.close()
    except BrokenPipeError:
        pass
    except OSError as error:
        if not stop.is_set():
            with failure_lock:
                if not failure:
                    failure.append(f"cannot write runner stdin: {error}")
            failure_event.set()


def _join_until(thread: threading.Thread, deadline: float) -> bool:
    remaining = deadline - time.monotonic()
    if remaining > 0:
        thread.join(remaining)
    return not thread.is_alive()


def _finalize_root(
    process: subprocess.Popen[bytes] | None,
    owner: _WindowsJob | None,
    deadline: float,
) -> str | None:
    errors: list[str] = []
    if process is not None:
        try:
            process.wait(timeout=max(0.001, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            errors.append("runner root did not exit during bounded cleanup")
        except OSError as error:
            errors.append(f"cannot reap runner root: {error}")
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                try:
                    stream.close()
                except OSError:
                    pass
    if owner is not None:
        try:
            owner.close()
        except OSError as error:
            errors.append(f"cannot close runner Job Object: {error}")
    return "; ".join(errors) if errors else None


def run_bounded_process(
    command: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    timeout_seconds: float,
    stdin_bytes: bytes | None = None,
    stdout_limit_bytes: int = DEFAULT_STDOUT_LIMIT_BYTES,
    stderr_limit_bytes: int = DEFAULT_STDERR_LIMIT_BYTES,
) -> BoundedProcessResult:
    """Run one command with a bounded lifetime, output, and ordinary descendant tree.

    On Windows the root is created suspended and assigned to a non-breakaway,
    kill-on-close Job Object before it is resumed. On Unix it owns a new session
    and process group. A descendant that deliberately creates a new Unix session
    is outside this ordinary-descendant contract.
    """
    launched_command = [str(part) for part in command]
    if not launched_command or not launched_command[0]:
        raise ValueError("command must contain a nonempty executable")
    if timeout_seconds <= 0:
        raise ValueError("timeout_seconds must be positive")
    if stdout_limit_bytes < 0 or stderr_limit_bytes < 0:
        raise ValueError("output limits must be nonnegative")

    deadline = time.monotonic() + timeout_seconds
    state = _LaunchState()
    launcher = threading.Thread(
        target=_launch,
        kwargs={
            "state": state,
            "command": launched_command,
            "cwd": cwd,
            "env": env,
            "has_stdin": stdin_bytes is not None,
        },
        name="bounded-process-launcher",
        daemon=True,
    )
    launcher.start()

    supervision_error: str | None = None
    try:
        while not state.done.is_set():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                supervision_error = f"runner timed out after {timeout_seconds:g} seconds"
                break
            state.done.wait(min(remaining, 0.02))

        if supervision_error is not None:
            state.stop.set()
            with state.lock:
                process = state.process
                owner = state.owner
            cleanup_error = _terminate_tree(process, owner)
            cleanup_deadline = time.monotonic() + _CLEANUP_GRACE_SECONDS
            if not _join_until(launcher, cleanup_deadline):
                supervision_error += "; runner launcher did not stop"
            with state.lock:
                process = state.process
                owner = state.owner
            later_termination_error = _terminate_tree(process, owner)
            finalization_error = _finalize_root(process, owner, cleanup_deadline)
            if cleanup_error:
                supervision_error += f"; {cleanup_error}"
            if later_termination_error:
                supervision_error += f"; {later_termination_error}"
            if finalization_error:
                supervision_error += f"; {finalization_error}"
            return BoundedProcessResult(
                returncode=None,
                stdout=b"",
                stderr=b"",
                pid=process.pid if process is not None else 0,
                supervision_error=supervision_error,
            )

        with state.lock:
            process = state.process
            owner = state.owner
            launch_error = state.error
        if launch_error is not None or process is None:
            finalization_error = _finalize_root(
                process,
                owner,
                time.monotonic() + _CLEANUP_GRACE_SECONDS,
            )
            if finalization_error:
                launch_error = (
                    f"{launch_error or 'runner launch failed'}; {finalization_error}"
                )
            return BoundedProcessResult(
                returncode=None,
                stdout=b"",
                stderr=b"",
                pid=process.pid if process is not None else 0,
                supervision_error=launch_error or "runner launch produced no process",
            )

        assert process.stdout is not None
        assert process.stderr is not None
        stdout_chunks: list[bytes] = []
        stderr_chunks: list[bytes] = []
        failures: list[str] = []
        failure_lock = threading.Lock()
        failure_event = threading.Event()
        readers = [
            threading.Thread(
                target=_read_bounded,
                kwargs={
                    "stream": process.stdout,
                    "label": "stdout",
                    "limit": stdout_limit_bytes,
                    "chunks": stdout_chunks,
                    "failure": failures,
                    "failure_lock": failure_lock,
                    "failure_event": failure_event,
                },
                name="bounded-process-stdout",
                daemon=True,
            ),
            threading.Thread(
                target=_read_bounded,
                kwargs={
                    "stream": process.stderr,
                    "label": "stderr",
                    "limit": stderr_limit_bytes,
                    "chunks": stderr_chunks,
                    "failure": failures,
                    "failure_lock": failure_lock,
                    "failure_event": failure_event,
                },
                name="bounded-process-stderr",
                daemon=True,
            ),
        ]
        for reader in readers:
            reader.start()

        writer: threading.Thread | None = None
        if stdin_bytes is not None:
            assert process.stdin is not None
            writer = threading.Thread(
                target=_write_stdin,
                args=(
                    process.stdin,
                    stdin_bytes,
                    failures,
                    failure_lock,
                    failure_event,
                    state.stop,
                ),
                name="bounded-process-stdin",
                daemon=True,
            )
            writer.start()

        observed_returncode: int | None = None
        while True:
            with failure_lock:
                if failures:
                    supervision_error = failures[0]
                    break
            observed_returncode = process.poll()
            if observed_returncode is not None:
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                supervision_error = f"runner timed out after {timeout_seconds:g} seconds"
                break
            failure_event.wait(min(remaining, 0.02))

        if supervision_error is not None:
            state.stop.set()
        cleanup_error = _terminate_tree(process, owner)
        if cleanup_error:
            supervision_error = (
                f"{supervision_error}; {cleanup_error}"
                if supervision_error
                else cleanup_error
            )

        cleanup_deadline = time.monotonic() + _CLEANUP_GRACE_SECONDS
        try:
            process.wait(timeout=max(0.001, cleanup_deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            supervision_error = (
                f"{supervision_error}; runner root did not exit during bounded cleanup"
                if supervision_error
                else "runner root did not exit during bounded cleanup"
            )
        if writer is not None and not _join_until(writer, cleanup_deadline):
            supervision_error = (
                f"{supervision_error}; runner stdin writer did not stop"
                if supervision_error
                else "runner stdin writer did not stop"
            )
        for reader in readers:
            if not _join_until(reader, cleanup_deadline):
                supervision_error = (
                    f"{supervision_error}; runner output reader did not stop"
                    if supervision_error
                    else "runner output reader did not stop"
                )
        with failure_lock:
            if failures and supervision_error is None:
                supervision_error = failures[0]
        finalization_error = _finalize_root(process, owner, cleanup_deadline)
        if finalization_error:
            supervision_error = (
                f"{supervision_error}; {finalization_error}"
                if supervision_error
                else finalization_error
            )

        return BoundedProcessResult(
            returncode=observed_returncode if supervision_error is None else None,
            stdout=b"".join(stdout_chunks),
            stderr=b"".join(stderr_chunks),
            pid=process.pid,
            supervision_error=supervision_error,
        )
    except BaseException:
        state.stop.set()
        with state.lock:
            process = state.process
            owner = state.owner
        _terminate_tree(process, owner)
        cleanup_deadline = time.monotonic() + _CLEANUP_GRACE_SECONDS
        _finalize_root(process, owner, cleanup_deadline)
        _join_until(launcher, cleanup_deadline)
        raise
