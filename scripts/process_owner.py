"""Own a command's descendants through completion and bounded cleanup."""

from __future__ import annotations

import codecs
import contextlib
import ctypes
import os
import queue
import signal
import subprocess
import threading
import time
from concurrent.futures import CancelledError, ThreadPoolExecutor
from contextvars import ContextVar, copy_context
from dataclasses import dataclass


class Operation:
    def __init__(self, timeout=None):
        self.deadline = time.monotonic() + timeout if timeout else None
        self.cancelled = threading.Event()
        self.error = None
        self.lock = threading.Lock()

    def check(self):
        if self.cancelled.is_set():
            raise CancelledError("operation cancelled after sibling failure")
        if self.deadline is not None and time.monotonic() >= self.deadline:
            raise TimeoutError("operation deadline expired")


_operation = ContextVar("owned_operation", default=None)


def check_operation():
    current = _operation.get()
    if current is not None:
        current.check()


@contextlib.contextmanager
def operation(timeout=None):
    current = _operation.get()
    if current is not None:
        yield current
        return
    token = _operation.set(Operation(timeout))
    try:
        yield _operation.get()
    finally:
        _operation.reset(token)


class OwnedThreadPoolExecutor(ThreadPoolExecutor):
    def __enter__(self):
        self.scope = operation()
        self.operation = self.scope.__enter__()
        return super().__enter__()

    def submit(self, fn, /, *args, **kwargs):
        context = copy_context()

        def execute():
            self.operation.check()
            try:
                return fn(*args, **kwargs)
            except BaseException as error:
                with self.operation.lock:
                    if self.operation.error is None:
                        self.operation.error = error
                    self.operation.cancelled.set()
                raise

        return super().submit(context.run, execute)

    def __exit__(self, kind, value, traceback):
        if kind:
            self.operation.cancelled.set()
        try:
            self.shutdown(wait=True, cancel_futures=bool(kind))
            if isinstance(value, CancelledError) and self.operation.error:
                raise self.operation.error
        finally:
            self.scope.__exit__(kind, value, traceback)


class CleanupFailed(RuntimeError):
    pass


class WindowsJob:
    def __init__(self):
        from ctypes import wintypes as w

        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateJobObjectW": ([w.LPVOID, w.LPCWSTR], w.HANDLE),
            "SetInformationJobObject": (
                [w.HANDLE, ctypes.c_int, w.LPVOID, w.DWORD],
                w.BOOL,
            ),
            "AssignProcessToJobObject": ([w.HANDLE, w.HANDLE], w.BOOL),
            "TerminateJobObject": ([w.HANDLE, w.UINT], w.BOOL),
            "QueryInformationJobObject": (
                [w.HANDLE, ctypes.c_int, w.LPVOID, w.DWORD, w.LPVOID],
                w.BOOL,
            ),
            "CloseHandle": ([w.HANDLE], w.BOOL),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.api, name)
            function.argtypes, function.restype = args, result

        class BasicLimits(ctypes.Structure):
            _fields_ = [
                ("process_time", ctypes.c_int64),
                ("job_time", ctypes.c_int64),
                ("flags", w.DWORD),
                ("min_ws", ctypes.c_size_t),
                ("max_ws", ctypes.c_size_t),
                ("active_limit", w.DWORD),
                ("affinity", ctypes.c_size_t),
                ("priority", w.DWORD),
                ("scheduling", w.DWORD),
            ]

        class ExtendedLimits(ctypes.Structure):
            _fields_ = [
                ("basic", BasicLimits),
                ("io", ctypes.c_uint64 * 6),
                ("process_memory", ctypes.c_size_t),
                ("job_memory", ctypes.c_size_t),
                ("peak_process_memory", ctypes.c_size_t),
                ("peak_job_memory", ctypes.c_size_t),
            ]

        self.handle = self.api.CreateJobObjectW(None, None)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = ExtendedLimits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not self.api.SetInformationJobObject(
            self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)
        ):
            self.close()
            raise ctypes.WinError(ctypes.get_last_error())

    def assign_and_resume(self, process):
        if not self.api.AssignProcessToJobObject(self.handle, int(process._handle)):
            raise ctypes.WinError(ctypes.get_last_error())
        # Popen closes the initial thread handle. Resume the suspended process
        # only after the job owns it, so no descendant can escape assignment.
        resume = ctypes.WinDLL("ntdll").NtResumeProcess
        resume.argtypes, resume.restype = [ctypes.c_void_p], ctypes.c_long
        if resume(int(process._handle)) != 0:
            raise OSError("could not resume owned process")

    def stop(self, deadline):
        if not self.api.TerminateJobObject(self.handle, 1):
            raise ctypes.WinError(ctypes.get_last_error())
        # JOBOBJECT_BASIC_ACCOUNTING_INFORMATION has four LARGE_INTEGERs and
        # four DWORD counters; ActiveProcesses is the third DWORD.
        accounting = ctypes.create_string_buffer(48)
        while True:
            if not self.api.QueryInformationJobObject(
                self.handle, 1, accounting, 48, None
            ):
                raise ctypes.WinError(ctypes.get_last_error())
            if int.from_bytes(accounting.raw[40:44], "little") == 0:
                return
            if time.monotonic() >= deadline:
                raise CleanupFailed("owned job still has active processes")
            time.sleep(0.02)

    def close(self):
        if self.handle:
            self.api.CloseHandle(self.handle)
            self.handle = None


@contextlib.contextmanager
def owned_process(args, **kwargs):
    job = WindowsJob() if os.name == "nt" else None
    process = None
    try:
        kwargs.pop("start_new_session", None)
        flags = kwargs.pop("creationflags", 0)
        process = subprocess.Popen(
            args,
            start_new_session=os.name != "nt",
            creationflags=(flags | 0x4 | subprocess.CREATE_NO_WINDOW) if job else flags,
            **kwargs,
        )
        if job:
            try:
                job.assign_and_resume(process)
            except BaseException:
                process.kill()
                process.wait(timeout=5)
                raise
            process._codex_owned_job = job
        yield process
    finally:
        deadline = time.monotonic() + 15
        try:
            if process is not None:
                if job:
                    job.stop(deadline)
                else:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                process.wait(timeout=max(0.01, deadline - time.monotonic()))
        except (OSError, subprocess.SubprocessError) as error:
            raise CleanupFailed(
                f"cleanup_failed for process {process.pid if process else 'unstarted'}"
            ) from error
        finally:
            if job:
                job.close()


def run_owned(args, *, timeout=None, check=False, capture_output=False, **kwargs):
    check_operation()
    if capture_output:
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    with owned_process(args, **kwargs) as process:
        deadline = time.monotonic() + timeout if timeout else None
        while True:
            check_operation()
            remaining = deadline - time.monotonic() if deadline else None
            if remaining is not None and remaining <= 0:
                raise subprocess.TimeoutExpired(args, timeout)
            try:
                stdout, stderr = process.communicate(
                    timeout=min(0.1, remaining) if remaining is not None else 0.1
                )
                break
            except subprocess.TimeoutExpired:
                continue
        result = subprocess.CompletedProcess(args, process.returncode, stdout, stderr)
    if check:
        result.check_returncode()
    return result


def check_output_owned(args, **kwargs):
    return run_owned(args, stdout=subprocess.PIPE, check=True, **kwargs).stdout


@dataclass(frozen=True)
class FiniteResult:
    args: tuple[str, ...]
    cwd: str
    status: str
    returncode: int
    elapsed: float
    stdout: str
    output_truncated: bool


def run_finite(
    args,
    *,
    timeout=3600,
    output_limit=65536,
    observe=None,
    stderr=subprocess.STDOUT,
    **kwargs,
):
    """Own a finite command, draining chunks without retaining a full transcript.

    `observe` receives decoded chunks before diagnostic-tail truncation. Full logs
    are deliberately not persisted; callers must reject truncated machine output.
    A deadline covers both process exit and EOF (including inherited child pipes).
    """
    if timeout <= 0 or output_limit <= 0:
        raise ValueError("timeout and output_limit must be positive")
    started = time.monotonic()
    tail = bytearray()
    total = 0
    status, code = "failed", 1
    chunks = queue.Queue(maxsize=16)
    stop = threading.Event()
    reader = None
    process = None
    decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def drain(pipe):
        try:
            while not stop.is_set():
                chunk = pipe.read(8192)
                while not stop.is_set():
                    try:
                        chunks.put(chunk, timeout=0.05)
                        break
                    except queue.Full:
                        pass
                if not chunk:
                    break
        finally:
            pipe.close()

    try:
        check_operation()
        with owned_process(
            args, stdout=subprocess.PIPE, stderr=stderr, **kwargs
        ) as process:
            reader = threading.Thread(target=drain, args=(process.stdout,), daemon=True)
            reader.start()
            eof = False
            while not eof or process.poll() is None:
                check_operation()
                if time.monotonic() - started >= timeout:
                    status, code = "timed_out", 124
                    break
                try:
                    chunk = chunks.get(timeout=0.05)
                except queue.Empty:
                    continue
                if not chunk:
                    eof = True
                else:
                    total += len(chunk)
                    tail.extend(chunk)
                    del tail[:-output_limit]
                if observe is not None:
                    observe(decoder.decode(chunk, final=eof))
            else:
                code = process.wait()
                status = "passed" if code == 0 else "failed"
    except (KeyboardInterrupt, CancelledError, TimeoutError) as error:
        status, code = (
            ("timed_out", 124)
            if isinstance(error, TimeoutError)
            else ("cancelled", 130)
        )
    except OSError as error:
        if process is not None:
            raise
        status, code = (
            "could_not_start",
            127 if isinstance(error, FileNotFoundError) else 1,
        )
        tail = bytearray(str(error).encode("utf-8")[-output_limit:])
    finally:
        stop.set()
        if reader is not None:
            reader.join(timeout=5)
            if reader.is_alive():
                raise CleanupFailed("owned output reader did not stop")
    return FiniteResult(
        tuple(args),
        str(kwargs.get("cwd", os.getcwd())),
        status,
        code,
        time.monotonic() - started,
        tail.decode("utf-8", "replace").replace("\r\n", "\n").replace("\r", "\n"),
        total > output_limit,
    )
