#!/usr/bin/env python3
"""Paired frozen Rust benchmark binaries; not current-source validation.

Run inside the shared lane with scripts/build/rust_build_status.py run-lane. Identity files
must name binaries frozen after a successful named-runner build. Only the three
helper-free audit probes are allowed. Workload assertions execute in Rust.
"""
from __future__ import annotations

import argparse
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


TESTS = {
    "store": "local::tests::benchmark_live_metadata_projection",
    "empty": "tools::parallel::tests::kd4_latency_bench_empty_generation_flush",
    "turn": "session::turn::tests::request_latency::completed_poll_lifecycle_wall_clock",
}


def resources(handle: int) -> dict:
    if os.name != "nt":
        return {"cpu_seconds": None, "peak_working_set_bytes": None}
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    class Memory(ctypes.Structure):
        _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in (
                "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
                "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage",
                "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage")]
    created, exited, system, user = (wintypes.FILETIME() for _ in range(4))
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = wintypes.BOOL
    if not kernel.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited), ctypes.byref(system), ctypes.byref(user)):
        raise ctypes.WinError(ctypes.get_last_error())
    memory = Memory()
    memory.cb = ctypes.sizeof(memory)
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Memory), wintypes.DWORD]
    psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(memory), memory.cb):
        raise ctypes.WinError(ctypes.get_last_error())
    ticks = lambda value: (value.dwHighDateTime << 32) | value.dwLowDateTime
    return {"cpu_seconds": (ticks(system) + ticks(user)) / 10_000_000,
            "peak_working_set_bytes": memory.PeakWorkingSetSize}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--scenario", required=True, choices=TESTS)
    parser.add_argument("--pairs", type=int, default=3)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.pairs < 2:
        parser.error("at least two pairs required")
    if args.output.exists():
        parser.error("output already exists")
    root = Path(__file__).resolve().parents[2]
    identities = {label: json.loads(path.read_text(encoding="utf-8")) for label, path in (
        ("baseline", args.baseline), ("candidate", args.candidate))}
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith("CARGO_BIN_EXE_") and key.upper() != "KD4_AGENT_TASK_FIXTURE_SEED"}
    # Match the Windows nextest setup: clear inherited Desktop state and keep
    # the minimum worker stack. Both binaries get this identical environment.
    for key in env:
        if key.startswith("CODEX_"):
            env[key] = ""
    env.update(RUST_MIN_STACK=str(max(8388608, int(env.get("RUST_MIN_STACK", "8388608")))),
               INSTA_UPDATE="no", INSTA_FORCE_PASS="0")
    results = []
    for pair in range(args.pairs):
        for label in (["baseline", "candidate"] if pair % 2 == 0 else ["candidate", "baseline"]):
            identity = identities[label]
            if identity.get("invalid_reason"):
                raise RuntimeError(identity["invalid_reason"])
            binary = Path(identity["binary"])
            actual_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
            if actual_hash != identity["binary_sha256"]:
                raise RuntimeError(f"frozen binary changed: {binary}")
            started = time.perf_counter()
            command = [str(binary), TESTS[args.scenario], "--exact", "--nocapture"]
            if args.scenario != "turn":
                command.append("--ignored")
            with subprocess.Popen(command,
                                  cwd=root / "codex-rs", env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, encoding="utf-8") as process:
                try:
                    stdout, stderr = process.communicate(timeout=180)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()
                    raise
                wall = time.perf_counter() - started
                if process.returncode or "test result: ok. 1 passed; 0 failed" not in stdout:
                    raise RuntimeError(f"{label}: exact benchmark failed or did not execute\n{stdout}\n{stderr}")
                usage = resources(int(process._handle)) if os.name == "nt" else resources(0)
            rows = []
            for line in (stdout + "\n" + stderr).splitlines():
                if args.scenario == "turn" and line.startswith("COMPLETED_POLL_LIFECYCLE "):
                    row = json.loads(line.removeprefix("COMPLETED_POLL_LIFECYCLE "))
                    if not row["exact_receipt"] or not row["exact_answer"] or row["model_requests"] != 2:
                        raise RuntimeError("loopback turn contract changed")
                    rows.append(row)
                    continue
                if line.startswith('{"benchmark":'):
                    row = json.loads(line)
                    if Path(row["executable"]).resolve() != binary.resolve():
                        raise RuntimeError("Rust benchmark executable identity mismatch")
                    rows.append(row)
            expected_rows = {"store": 80, "empty": 4, "turn": 3}[args.scenario]
            if len(rows) != expected_rows:
                raise RuntimeError(f"incomplete benchmark rows: {len(rows)} != {expected_rows}")
            results.append({"pair": pair, "label": label, "process_wall_seconds": wall,
                            **usage, "rows": rows})
            print(json.dumps({"pair": pair, "label": label, "wall_seconds": wall, **usage}), flush=True)
    report = {"scope": "frozen production Rust path probes; not current-source validation or live-provider task timing",
              "scenario": args.scenario, "pairs": args.pairs, "identities": identities,
              "environment": {key: env[key] for key in ("RUST_MIN_STACK", "INSTA_UPDATE", "INSTA_FORCE_PASS")},
              "results": results}
    with args.output.open("x", encoding="utf-8") as output:
        json.dump(report, output, indent=2)
    print(f"Paired measurements saved: {args.output}")


if __name__ == "__main__":
    main()
