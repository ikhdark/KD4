"""Opt-in Cargo diagnostics. OS observations are never replaced by intended env.

No compiler/cache flags are changed. Very verbose Cargo diagnostics retain the
compiler command lines; fingerprint trace retains historical dirty reasons.
Windows environment reads require a native 64-bit Python and 64-bit child.
Unsupported/short-lived/inaccessible children are explicitly unverified.
"""

from __future__ import annotations

import ctypes
import hashlib
import json
import os
import subprocess
import time
from pathlib import Path


def diagnostic_command(args, env):
    """Return diagnostic-only changes, only for an explicitly opted-in build."""
    args, env = list(args), dict(env)
    enabled = env.get("CODEX_CARGO_DIAGNOSTICS") == "1"
    if not enabled or not args or Path(args[0]).stem.lower() != "cargo":
        return args, env, False
    if "--binaries-metadata" in args:
        return args, env, False
    if args[1:2] == ["nextest"] and args[2:3] in (["run"], ["list"]):
        args[3:3] = ["--cargo-verbose", "--cargo-verbose"]
    elif args[1:2] == ["build"]:
        args[2:2] = ["-vv"]
    else:
        return args, env, False
    trace = "cargo::core::compiler::fingerprint=trace"
    env["CARGO_LOG"] = ",".join(filter(None, [env.get("CARGO_LOG"), trace]))
    return args, env, True


def build_environment(env):
    """Keep build inputs, not unrelated credentials in the process environment."""
    names = {
        "PATH", "PATHEXT", "HOME", "USERPROFILE", "CARGO_HOME", "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC", "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER", "RUSTFLAGS", "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "CARGO_LOG",
        "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS", "LIB", "LIBPATH",
        "INCLUDE", "SCCACHE_BASEDIRS", "SCCACHE_DIR", "SCCACHE_CACHE_SIZE",
    }
    return {key: value for key, value in env.items() if key.upper() in names
            or key.upper().startswith(("CARGO_BUILD_", "CARGO_PROFILE_", "CARGO_TARGET_"))}


def _windows_api():
    from ctypes import wintypes as w

    if ctypes.sizeof(ctypes.c_void_p) != 8:
        raise OSError("OS environment capture requires native 64-bit Python")
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [w.DWORD, w.BOOL, w.DWORD]
    kernel.OpenProcess.restype = w.HANDLE
    kernel.CloseHandle.argtypes = [w.HANDLE]
    kernel.ReadProcessMemory.argtypes = [w.HANDLE, ctypes.c_void_p, ctypes.c_void_p,
                                        ctypes.c_size_t, ctypes.POINTER(ctypes.c_size_t)]
    kernel.IsWow64Process.argtypes = [w.HANDLE, ctypes.POINTER(w.BOOL)]
    kernel.QueryFullProcessImageNameW.argtypes = [w.HANDLE, w.DWORD, w.LPWSTR,
                                                ctypes.POINTER(w.DWORD)]
    return kernel


def process_snapshot(pid):
    """Read the live process, not Popen kwargs; failure must remain observable."""
    if os.name != "nt":
        root = Path("/proc") / str(pid)
        raw = root.joinpath("environ").read_bytes()
        env = dict(entry.decode(errors="surrogateescape").split("=", 1)
                   for entry in raw.split(b"\0") if b"=" in entry)
        return {"pid": pid, "image": os.readlink(root / "exe"),
                "command_line": root.joinpath("cmdline").read_bytes().decode(errors="replace").replace("\0", " "),
                "cwd": os.readlink(root / "cwd"), "environment": build_environment(env),
                "environment_sha256": hashlib.sha256(raw).hexdigest(),
                "source": "procfs", "observed_at_ns": time.time_ns()}
    from ctypes import wintypes as w

    kernel = _windows_api()
    handle = kernel.OpenProcess(0x0400 | 0x0010, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        wow64 = w.BOOL()
        if not kernel.IsWow64Process(handle, ctypes.byref(wow64)) or wow64.value:
            raise OSError("unsupported or unknown child architecture")
        ntdll = ctypes.WinDLL("ntdll")
        query = ntdll.NtQueryInformationProcess
        query.argtypes = [w.HANDLE, w.ULONG, ctypes.c_void_p, w.ULONG, ctypes.c_void_p]
        query.restype = ctypes.c_long
        basic = (ctypes.c_void_p * 6)()
        if query(handle, 0, ctypes.byref(basic), ctypes.sizeof(basic), None) < 0:
            raise OSError("cannot query child PEB")

        def read(address, size):
            buffer = ctypes.create_string_buffer(size)
            count = ctypes.c_size_t()
            ok = kernel.ReadProcessMemory(handle, address, buffer, size, ctypes.byref(count))
            if not ok or count.value != size:
                raise OSError("incomplete child memory observation")
            return buffer.raw

        def pointer(address):
            return int.from_bytes(read(address, 8), "little")

        parameters = pointer(basic[1] + 0x20)

        def unicode_string(offset):
            descriptor = read(parameters + offset, 16)
            size = int.from_bytes(descriptor[:2], "little")
            address = int.from_bytes(descriptor[8:], "little")
            return read(address, size).decode("utf-16-le", errors="surrogatepass")

        address = pointer(parameters + 0x80)
        # Read aligned UTF-16 blocks up to a strict cap. A block can cross an
        # inaccessible region: that is an unverified observation, never env={}.
        raw = bytearray()
        end = None
        for offset in range(0, 1024 * 1024, 128):
            raw.extend(read(address + offset, 128))
            for index in range(max(0, len(raw) - 130), len(raw) - 2, 2):
                if raw[index:index + 4] == b"\0\0\0\0":
                    end = index + 4
                    break
            if end is not None:
                break
        if end is None:
            raise OSError("child environment exceeds observation cap")
        raw = bytes(raw[:end])
        env = dict(entry.split("=", 1) for entry in raw.decode("utf-16-le", errors="surrogatepass").split("\0")
                   if "=" in entry and not entry.startswith("="))
        image = ctypes.create_unicode_buffer(32768)
        size = w.DWORD(len(image))
        if not kernel.QueryFullProcessImageNameW(handle, 0, image, ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        return {"pid": pid, "image": image.value, "command_line": unicode_string(0x70),
                "cwd": unicode_string(0x38), "environment": build_environment(env),
                "environment_sha256": hashlib.sha256(raw).hexdigest(),
                "source": "ReadProcessMemory/PEB", "observed_at_ns": time.time_ns()}
    finally:
        kernel.CloseHandle(handle)


def process_tree():
    """PID/parent/name inventory; caller retains only the owned descendants."""
    if os.name != "nt":
        rows = []
        for root in Path("/proc").iterdir():
            if not root.name.isdigit():
                continue
            try:
                data = root.joinpath("stat").read_text()
                tail = data.rsplit(")", 1)[1].split()
                rows.append((int(root.name), int(tail[1]), data.split("(", 1)[1].rsplit(")", 1)[0]))
            except (OSError, ValueError, IndexError):
                continue
        return rows
    from ctypes import wintypes as w

    class Entry(ctypes.Structure):
        _fields_ = [("size", w.DWORD), ("usage", w.DWORD), ("pid", w.DWORD),
                    ("heap", ctypes.c_size_t), ("module", w.DWORD), ("threads", w.DWORD),
                    ("parent", w.DWORD), ("priority", w.LONG), ("flags", w.DWORD),
                    ("name", w.WCHAR * 260)]

    kernel = _windows_api()
    kernel.CreateToolhelp32Snapshot.argtypes = [w.DWORD, w.DWORD]
    kernel.CreateToolhelp32Snapshot.restype = w.HANDLE
    kernel.Process32FirstW.argtypes = [w.HANDLE, ctypes.POINTER(Entry)]
    kernel.Process32NextW.argtypes = [w.HANDLE, ctypes.POINTER(Entry)]
    handle = kernel.CreateToolhelp32Snapshot(2, 0)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        entry = Entry()
        entry.size = ctypes.sizeof(entry)
        rows = []
        more = kernel.Process32FirstW(handle, ctypes.byref(entry))
        while more:
            rows.append((entry.pid, entry.parent, entry.name))
            more = kernel.Process32NextW(handle, ctypes.byref(entry))
        return rows
    finally:
        kernel.CloseHandle(handle)


def wait_with_diagnostics(process, *, timeout, log_path):
    """Observe owned Cargo descendants during the existing wait; no new thread.

    Short-lived children can be missed. This is sampling, not proof that every
    child was captured. The existing owner still handles cancellation/timeout.
    Logging/OS failures disable observation, not the build or its cleanup.
    """
    deadline = None if timeout is None else time.monotonic() + timeout
    seen = set()
    attempts = {}
    observations = 0
    with Path(log_path).open("x", encoding="utf-8") as log:
        log.write(json.dumps({"kind": "coverage", "complete": False,
                              "reason": "sampled live children; short-lived processes may be missed"}) + "\n")
        while True:
            try:
                rows = process_tree()
                descendants = {process.pid}
                for _ in range(16):
                    expanded = descendants | {pid for pid, parent, _ in rows if parent in descendants}
                    if expanded == descendants:
                        break
                    descendants = expanded
                for pid, _, name in rows:
                    if pid not in descendants or pid in seen or Path(name).stem.lower() not in {"cargo", "cargo-nextest"}:
                        continue
                    attempts[pid] = attempts.get(pid, 0) + 1
                    try:
                        record = {"kind": "observed_child", **process_snapshot(pid)}
                        observations += 1
                        seen.add(pid)
                    except OSError as error:
                        # Windows startup can relocate the environment between
                        # pointer and buffer reads. A later live-process sample
                        # may settle it; cap observation attempts, never builds.
                        record = {"kind": "unverified_child", "pid": pid,
                                  "attempt": attempts[pid], "error": str(error)}
                        if attempts[pid] >= 3:
                            seen.add(pid)
                    log.write(json.dumps(record, ensure_ascii=True) + "\n")
                log.flush()
            except OSError as error:
                # Don't substitute the intended env or retry a failing storage
                # operation. The caller reports the diagnostic failure.
                raise RuntimeError(f"Cargo child observation failed: {error}") from error
            remaining = None if deadline is None else max(0, deadline - time.monotonic())
            try:
                code = process.wait(timeout=0.2 if remaining is None else min(0.2, remaining))
                log.write(json.dumps({"kind": "finished", "observations": observations,
                                      "exit_code": code, "complete": False}) + "\n")
                return code
            except subprocess.TimeoutExpired:
                if deadline is not None and time.monotonic() >= deadline:
                    raise
