"""Windows managed-build admission. Lane custody remains with the lane owner.

Cargo jobs are selected once. Named owning jobs enforce a shared, adaptive CPU
ceiling for whole trees; physical/commit admission is predictive, not an OOM
guarantee. No daemon, priority changes, machine setup, or hard memory limits.
"""
from __future__ import annotations

import contextlib
import ctypes
from ctypes import wintypes as w
import json
import os
from pathlib import Path
import sys
import time
import uuid

from scripts.common.atomic_json import write_json_atomic
from scripts.common.process_owner import CleanupFailed, check_operation, run_owned

GIB = 1024 ** 3
PREFIX = "CODEX_CARGO_BUILD_"


def _integer(env, name, default, minimum=1, maximum=None):
    value = int(env.get(PREFIX + name, str(default)))
    if value < minimum or (maximum is not None and value > maximum):
        raise ValueError(f"{PREFIX + name} is outside its supported range")
    return value


def resource_root(env):
    owner = Path(__file__).resolve().parents[2]
    kdr = owner if owner.name.lower() == "kdr" else owner.parent / "KDR"
    if PREFIX + "RESOURCE_ROOT" in env:
        return Path(env[PREFIX + "RESOURCE_ROOT"]).absolute()
    legacy = kdr / ".local/benchmarks/builds"
    if legacy.exists():
        raise ValueError(f"managed build storage must be moved from {legacy} to {kdr / '.builds'}; refusing a second resource pool")
    return (kdr / ".builds/resources").absolute()


def policy(env):
    max_active = _integer(env, "MAX_ACTIVE", 3)
    return {
        "maxActive": max_active,
        "maxCold": _integer(env, "MAX_COLD", max_active),
        "cpuPercent": _integer(env, "CPU_PERCENT", 100, maximum=100),
        "memoryPerJobBytes": _integer(env, "MEMORY_PER_JOB_MIB", 512) * 1024 ** 2,
        "coldBaseBytes": _integer(env, "COLD_BASE_MIB", 4096) * 1024 ** 2,
        "warmBaseBytes": _integer(env, "WARM_BASE_MIB", 2048) * 1024 ** 2,
        "physicalReserveBytes": _integer(env, "PHYSICAL_RESERVE_MIB", 2048) * 1024 ** 2,
        "commitReserveBytes": _integer(env, "COMMIT_RESERVE_MIB", 2048) * 1024 ** 2,
    }


class WindowsResources:
    def __init__(self):
        if os.name != "nt":
            raise ValueError("managed resource admission requires Windows")
        self.api = ctypes.WinDLL("kernel32", use_last_error=True)
        signatures = {
            "CreateFileW": ([w.LPCWSTR, w.DWORD, w.DWORD, w.LPVOID, w.DWORD, w.DWORD, w.HANDLE], w.HANDLE),
            "CloseHandle": ([w.HANDLE], w.BOOL),
            "OpenJobObjectW": ([w.DWORD, w.BOOL, w.LPCWSTR], w.HANDLE),
            "QueryInformationJobObject": ([w.HANDLE, ctypes.c_int, w.LPVOID, w.DWORD, w.LPVOID], w.BOOL),
            "SetInformationJobObject": ([w.HANDLE, ctypes.c_int, w.LPVOID, w.DWORD], w.BOOL),
            "IsProcessInJob": ([w.HANDLE, w.HANDLE, ctypes.POINTER(w.BOOL)], w.BOOL),
            "GetCurrentProcess": ([], w.HANDLE),
            "GetSystemTimes": ([ctypes.POINTER(w.FILETIME)] * 3, w.BOOL),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.api, name)
            function.argtypes, function.restype = args, result

    def exclusive(self, path):
        handle = self.api.CreateFileW(str(path), 0xC0000000, 0, None, 4, 0x80, None)
        if handle == ctypes.c_void_p(-1).value:
            error = ctypes.get_last_error()
            if error in (32, 33):
                return None
            raise ctypes.WinError(error)
        return handle

    @contextlib.contextmanager
    def coordination(self, root, *, cancellable=True):
        deadline = time.monotonic() + 10
        handle = None
        while handle is None:
            if cancellable:
                check_operation()
            handle = self.exclusive(root / "coordination.lock")
            if handle is None:
                if time.monotonic() >= deadline:
                    raise TimeoutError("build resource coordination lock did not become free")
                time.sleep(0.02)
        try:
            yield
        finally:
            self.api.CloseHandle(handle)

    @contextlib.contextmanager
    def job(self, name):
        handle = self.api.OpenJobObjectW(6, False, name)  # QUERY | SET_ATTRIBUTES
        if not handle and ctypes.get_last_error() != 2:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            yield handle or None
        finally:
            if handle:
                self.api.CloseHandle(handle)

    def job_memory(self, handle):
        value = (ctypes.c_uint64 * 2)()
        if not self.api.QueryInformationJobObject(handle, 28, ctypes.byref(value), ctypes.sizeof(value), None):
            raise ctypes.WinError(ctypes.get_last_error())
        return value[0]

    def job_active(self, handle):
        value = ctypes.create_string_buffer(48)
        if not self.api.QueryInformationJobObject(handle, 1, value, 48, None):
            raise ctypes.WinError(ctypes.get_last_error())
        return int.from_bytes(value.raw[40:44], "little")

    def set_cpu(self, handle, rate):
        value = (w.DWORD * 2)(5, rate)  # ENABLE | HARD_CAP; 1..10000
        if not self.api.SetInformationJobObject(handle, 15, ctypes.byref(value), ctypes.sizeof(value)):
            raise ctypes.WinError(ctypes.get_last_error())

    def cpu_rate(self, handle):
        value = (w.DWORD * 2)()
        if not self.api.QueryInformationJobObject(handle, 15, ctypes.byref(value), ctypes.sizeof(value), None):
            raise ctypes.WinError(ctypes.get_last_error())
        return value[1]

    def snapshot(self):
        class Performance(ctypes.Structure):
            _fields_ = [("cb", w.DWORD)] + [(name, ctypes.c_size_t) for name in (
                "commit", "limit", "peak", "physical", "available", "cache",
                "kernel", "paged", "nonpaged", "page",
            )] + [(name, w.DWORD) for name in ("handles", "processes", "threads")]
        info = Performance()
        info.cb = ctypes.sizeof(info)
        query = ctypes.WinDLL("psapi", use_last_error=True).GetPerformanceInfo
        query.argtypes, query.restype = [ctypes.POINTER(Performance), w.DWORD], w.BOOL
        if not query(ctypes.byref(info), info.cb):
            raise ctypes.WinError(ctypes.get_last_error())
        times = [w.FILETIME() for _ in range(3)]
        if not self.api.GetSystemTimes(*(ctypes.byref(value) for value in times)):
            raise ctypes.WinError(ctypes.get_last_error())
        idle, kernel, user = [(value.dwHighDateTime << 32) | value.dwLowDateTime for value in times]
        return {
            "commitBytes": info.commit * info.page, "commitLimitBytes": info.limit * info.page,
            "physicalBytes": info.physical * info.page, "availablePhysicalBytes": info.available * info.page,
            "systemCpuIdle100ns": idle, "systemCpuTotal100ns": kernel + user,
        }


class PagingCounters:
    """English PDH names work on localized Windows; failure is explicit telemetry."""
    def __init__(self):
        self.api = ctypes.WinDLL("pdh")
        self.handle = w.HANDLE()
        self.counters = {}
        self.error = None
        signatures = {
            "PdhOpenQueryW": ([w.LPCWSTR, ctypes.c_size_t, ctypes.POINTER(w.HANDLE)], w.LONG),
            "PdhAddEnglishCounterW": ([w.HANDLE, w.LPCWSTR, ctypes.c_size_t, ctypes.POINTER(w.HANDLE)], w.LONG),
            "PdhCollectQueryData": ([w.HANDLE], w.LONG),
            "PdhGetFormattedCounterValue": ([w.HANDLE, w.DWORD, w.LPVOID, w.LPVOID], w.LONG),
            "PdhCloseQuery": ([w.HANDLE], w.LONG),
        }
        for name, (args, result) in signatures.items():
            function = getattr(self.api, name)
            function.argtypes, function.restype = args, result
        try:
            self._ok(self.api.PdhOpenQueryW(None, 0, ctypes.byref(self.handle)))
            for name, path in (("pagesInputPerSecond", r"\Memory\Pages Input/sec"),
                               ("pagesOutputPerSecond", r"\Memory\Pages Output/sec")):
                counter = w.HANDLE()
                self._ok(self.api.PdhAddEnglishCounterW(self.handle, path, 0, ctypes.byref(counter)))
                self.counters[name] = counter
            self._ok(self.api.PdhCollectQueryData(self.handle))
        except OSError as error:
            self.error = str(error)
            self.close()

    @staticmethod
    def _ok(code):
        if code:
            raise OSError(f"PDH status 0x{code & 0xffffffff:08x}")

    def sample(self):
        if self.error:
            return {"error": self.error}
        class Value(ctypes.Structure):
            _fields_ = [("status", w.DWORD), ("value", ctypes.c_double)]
        try:
            self._ok(self.api.PdhCollectQueryData(self.handle))
            result = {}
            for name, counter in self.counters.items():
                value = Value()
                self._ok(self.api.PdhGetFormattedCounterValue(counter, 0x200, None, ctypes.byref(value)))
                if value.status not in (0, 1):
                    raise OSError(f"PDH counter status 0x{value.status:08x}")
                result[name] = value.value
            return result
        except OSError as error:
            return {"error": str(error)}

    def close(self):
        if self.handle:
            self.api.PdhCloseQuery(self.handle)
            self.handle = None


def _safe_directory(root):
    for path in (root, *root.parents):
        if path.exists() and path.lstat().st_file_attributes & 0x400:
            raise ValueError(f"resource state cannot use a reparse point: {path}")
    root.mkdir(parents=True, exist_ok=True)


def _active(api, root):
    rows = []
    for path in sorted(root.glob("*.json")):
        row = json.loads(path.read_text(encoding="utf-8"))
        if (not isinstance(row, dict)
                or len(path.stem) != 32 or any(c not in "0123456789abcdef" for c in path.stem)
                or row.get("schemaVersion") != 1
                or not {"policy", "reservedBytes", "jobs", "cold"}.issubset(row)
                or row.get("jobName") != "Global\\CodexCargoResource-" + path.stem):
            raise ValueError(f"unrecognized build resource state; refusing to reclaim {path}")
        if row.get("quarantined"):
            raise CleanupFailed(f"unconfirmed build cleanup; inspect {path}")
        lease_path = path.with_suffix(".lock")
        handle = api.exclusive(lease_path)
        if handle is None:
            rows.append(row)
            continue
        try:
            with api.job(row["jobName"]) as job:
                if job and api.job_active(job):
                    rows.append(row)  # Owner died; retain custody until its tree is gone.
                    continue
            path.unlink()
            # Close before unlinking on Windows, still under coordination.
        finally:
            api.api.CloseHandle(handle)
        lease_path.unlink(missing_ok=True)
    return rows


def _rebalance(api, rows):
    if not rows:
        return
    rate = max(1, min(row["policy"]["cpuPercent"] for row in rows) * 100 // len(rows))
    for row in rows:
        with api.job(row["jobName"]) as handle:
            if handle:
                api.set_cpu(handle, rate)


def _jobs_value(value, logical):
    if value == "default":
        return logical
    jobs = int(value)
    if jobs == 0:
        raise ValueError("Cargo jobs cannot be zero")
    return max(1, logical + jobs) if jobs < 0 else jobs


def budget_command(command, jobs, env):
    """Clamp direct Cargo job options; opaque wrappers receive the same env cap."""
    from scripts.build.rust_build_status import _cargo_subcommand_index
    result = list(command)
    logical = os.cpu_count() or 1
    if env.get("CARGO_BUILD_JOBS"):
        jobs = min(jobs, _jobs_value(env["CARGO_BUILD_JOBS"], logical))
    start = 0
    if result and Path(result[0]).stem.lower() == "rustup" and result[1:2] == ["run"]:
        start = 3
    if len(result) > start and Path(result[start]).stem.lower() == "cargo":
        index = _cargo_subcommand_index(result, start_index=start + 1)
        if index is not None and result[index] in {
            "build", "b", "check", "c", "rustc", "clippy", "doc", "d", "run", "r", "test", "t", "bench",
        }:
            rebuilt = result[:index + 1]
            i = index + 1
            while i < len(result):
                token = result[i]
                if token == "--":
                    rebuilt.extend(result[i:])
                    break
                if token in ("-j", "--jobs"):
                    if i + 1 == len(result):
                        raise ValueError(f"{token} requires a Cargo job count")
                    jobs = min(jobs, _jobs_value(result[i + 1], logical))
                    i += 2
                    continue
                if token.startswith("--jobs=") or (token.startswith("-j") and len(token) > 2):
                    jobs = min(jobs, _jobs_value(token.split("=", 1)[1] if token.startswith("--") else token[2:], logical))
                else:
                    rebuilt.append(token)
                i += 1
            # Insert before -- so rustc/program arguments retain their meaning.
            result = rebuilt[:index + 1] + ["--jobs", str(jobs)] + rebuilt[index + 1:]
    return result, jobs


class BuildResourcesBusy(RuntimeError):
    pass


class BuildLease:
    def __init__(self, env, cold, record):
        self.api = WindowsResources()
        self.env, self.cold, self.record = env, cold, record
        self.root = resource_root(env)
        _safe_directory(self.root)
        self.policy = policy(env)
        self.token = uuid.uuid4().hex
        self.path = self.root / (self.token + ".json")
        self.job_name = "Global\\CodexCargoResource-" + self.token
        self.handle = None
        self.job = None
        self.paging = None
        self.last_poll = 0.0
        self.started_at = None
        self.rows = []
        self.wait_started = None
        self.wait_reasons = set()
        self.finished = False
        self.receipt_path = self.root / "receipts" / (self.token + ".json")
        self.record["receiptPath"] = str(self.receipt_path)

    def acquire(self, *, wait=True):
        if self.wait_started is None:
            self.wait_started = time.perf_counter()
        started = self.wait_started
        reasons = self.wait_reasons
        while True:
            check_operation()
            with self.api.coordination(self.root):
                rows = _active(self.api, self.root)
                policies = [self.policy] + [row["policy"] for row in rows]
                memory = self.api.snapshot()
                future = 0
                for row in rows:
                    with self.api.job(row["jobName"]) as job:
                        used = self.api.job_memory(job) if job else 0
                    future += max(0, row["reservedBytes"] - used)
                available = min(
                    memory["availablePhysicalBytes"] - max(p["physicalReserveBytes"] for p in policies),
                    memory["commitLimitBytes"] - memory["commitBytes"] - max(p["commitReserveBytes"] for p in policies),
                ) - future
                jobs = max(1, (os.cpu_count() or 1) * min(p["cpuPercent"] for p in policies) // (100 * (len(rows) + 1)))
                base = self.policy["coldBaseBytes" if self.cold else "warmBaseBytes"]
                jobs = min(jobs, (available - base) // self.policy["memoryPerJobBytes"])
                reason = (
                    "active_build_limit" if len(rows) >= min(p["maxActive"] for p in policies) else
                    "cold_build_limit" if self.cold and sum(row["cold"] for row in rows) >= min(p["maxCold"] for p in policies) else
                    "physical_or_commit_headroom" if jobs < 1 else None
                )
                if reason is None:
                    self.command, jobs = budget_command(self.record["command"], jobs, self.env)
                    self.row = {"schemaVersion": 1, "jobName": self.job_name, "pid": os.getpid(),
                                "cold": self.cold, "jobs": jobs, "policy": self.policy,
                                "reservedBytes": base + jobs * self.policy["memoryPerJobBytes"]}
                    self.handle = self.api.exclusive(self.path.with_suffix(".lock"))
                    if self.handle is None:
                        raise RuntimeError("new resource lease unexpectedly exists")
                    write_json_atomic(self.path, self.row, exclusive=True)
                    _rebalance(self.api, rows + [self.row])
                    self.record.update(status="admitted", jobs=jobs, cold=self.cold,
                        policy=self.policy, admissionMemory=memory, reservedBytes=self.row["reservedBytes"],
                        resourceWaitMs=(time.perf_counter() - started) * 1000,
                        waitReasons=sorted(reasons), activeAtAdmission=len(rows) + 1)
                    return True
                if reason not in reasons:
                    print(f"waiting for global Cargo resources: {reason}", file=sys.stderr)
                    reasons.add(reason)
                self.record.update(status="waiting", waitReasons=sorted(reasons),
                                   resourceWaitMs=(time.perf_counter() - started) * 1000)
            if not wait:
                return False
            # Never hold coordination while sleeping or waiting on a lane/tree.
            time.sleep(0.25)

    def ready(self, job):
        self.job = job
        self.started_at = time.perf_counter()
        with self.api.coordination(self.root):
            _rebalance(self.api, _active(self.api, self.root))
        self.paging = PagingCounters()
        self.poll()

    def poll(self, force=False):
        now = time.perf_counter()
        if not self.job or (not force and now - self.last_poll < 0.5):
            return
        self.last_poll = now
        with self.api.coordination(self.root):
            rows = _active(self.api, self.root)
            _rebalance(self.api, rows)
            rate = self.api.cpu_rate(self.job.handle)
        sample = {"elapsedSeconds": now - self.started_at, "activeBuilds": len(rows),
                  "cpuRate": rate,
                  "job": self.job.build_resource_receipt(), "system": self.api.snapshot(),
                  "paging": self.paging.sample() if self.paging else {"error": "not initialized"}}
        if self.rows:
            previous = self.rows[-1]
            elapsed = sample["elapsedSeconds"] - previous["elapsedSeconds"]
            sample["buildCpuPercentOfHost"] = 100 * (
                sample["job"]["totalCpuSeconds"] - previous["job"]["totalCpuSeconds"]
            ) / (elapsed * (os.cpu_count() or 1)) if elapsed else None
            total = sample["system"]["systemCpuTotal100ns"] - previous["system"]["systemCpuTotal100ns"]
            idle = sample["system"]["systemCpuIdle100ns"] - previous["system"]["systemCpuIdle100ns"]
            sample["systemCpuPercent"] = 100 * (total - idle) / total if total else None
        self.rows.append(sample)

    def stopped(self, job):
        try:
            try:
                self.poll(force=True)
            except Exception as error:
                self.record["measurementError"] = f"{type(error).__name__}: {error}"
            terminal = job.build_resource_receipt()
            elapsed = time.perf_counter() - self.started_at if self.started_at is not None else 0
            self.record.update(terminal=terminal, compileTreeMs=elapsed * 1000,
                averageCpuPercentOfHost=100 * terminal["totalCpuSeconds"] / (elapsed * (os.cpu_count() or 1)) if elapsed else None)
        except Exception as error:
            # Telemetry failure must not mask the command or cleanup failure.
            self.record["measurementError"] = f"{type(error).__name__}: {error}"
        finally:
            if self.paging:
                self.paging.close()
            self.job = None

    def release(self, quarantined=False):
        if self.handle is None:
            return
        try:
            with self.api.coordination(self.root, cancellable=False):
                if quarantined:
                    write_json_atomic(self.path, {**self.row, "quarantined": True})
                else:
                    self.path.unlink(missing_ok=True)
                self.api.api.CloseHandle(self.handle)
                self.handle = None
                if not quarantined:
                    self.path.with_suffix(".lock").unlink(missing_ok=True)
                    _rebalance(self.api, _active(self.api, self.root))
        finally:
            if self.handle is not None:
                self.api.api.CloseHandle(self.handle)
                self.handle = None

    def finish(self, quarantined=False):
        if self.finished:
            return
        try:
            self.release(quarantined=quarantined)
        finally:
            record = self.record
            record["samples"] = self.rows
            record["measurementScope"] = {
                "jobPeaks": "kernel lifetime committed-memory peaks; not resident working set or pagefile I/O",
                "systemMemory": "0.5s samples; observed peak is not a lifetime system peak",
                "paging": "PDH system-wide physical page input/output rates; not attributable to this build",
                "pageFaults": "job faults include soft faults; not a hard-paging count",
                "resourceWait": "elapsed resource admission interval, including retried lane probes",
            }
            if self.rows:
                record["sampledPeakSystemCommitBytes"] = max(row["system"]["commitBytes"] for row in self.rows)
                record["sampledMinimumAvailablePhysicalBytes"] = min(row["system"]["availablePhysicalBytes"] for row in self.rows)
            write_json_atomic(self.receipt_path, record, exclusive=True)
            self.finished = True


def _inherited_lease(env):
    token = env.get(PREFIX + "LEASE")
    if not token:
        return None
    if len(token) != 32 or any(char not in "0123456789abcdef" for char in token):
        raise ValueError("invalid inherited Cargo resource lease")
    api = WindowsResources()
    row = json.loads((resource_root(env) / (token + ".json")).read_text(encoding="utf-8"))
    if not isinstance(row, dict) or row.get("quarantined") or row.get("jobName") != "Global\\CodexCargoResource-" + token:
        raise ValueError("inherited Cargo resource lease is invalid or quarantined")
    with api.job(row["jobName"]) as job:
        member = w.BOOL()
        if not job or not api.api.IsProcessInJob(api.api.GetCurrentProcess(), job, ctypes.byref(member)) or not member.value:
            raise ValueError("inherited Cargo resource lease does not own this process")
    return row


def run_build(command, *, cold=True, resource_record=None, lane_wait_ms=0.0, lease=None, wait=True, **kwargs):
    """One owned compilation; nested managed wrappers share their owning lease."""
    if os.name != "nt":
        raise ValueError("adaptive build resources require Windows")
    inherited_env = kwargs.pop("env", None)
    env = dict(os.environ if inherited_env is None else inherited_env)
    record = resource_record if resource_record is not None else {}
    record.update(schemaVersion=1, command=list(command), laneWaitMs=lane_wait_ms,
                  status="pending", samples=[], startedUtc=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()))
    inherited = _inherited_lease(env) if lease is None else None
    if inherited is not None:
        adjusted, jobs = budget_command(command, inherited["jobs"], env)
        record.update(status="inherited", jobs=jobs, parentJob=inherited["jobName"], executedCommand=adjusted)
        return run_owned(adjusted, env={**env, "CARGO_BUILD_JOBS": str(jobs)}, **kwargs)
    lease = lease if lease is not None else BuildLease(env, cold, record)
    quarantine = False
    try:
        if lease.handle is None and not lease.acquire(wait=wait):
            raise BuildResourcesBusy("global Cargo resources are busy; command was not executed")
        # Admission may precede target-dir/message-format insertion by the lane
        # owner. Reapply the selected job cap to the final, owned command.
        lease.command, _ = budget_command(command, record["jobs"], env)
        env.update({PREFIX + "LEASE": lease.token, PREFIX + "RESOURCE_ROOT": str(lease.root),
                    "CARGO_BUILD_JOBS": str(record["jobs"])})
        record["executedCommand"] = lease.command
        print(f"Cargo resource budget: jobs={record['jobs']}, cold={lease.cold}; {lease.receipt_path}", file=sys.stderr)
        result = run_owned(lease.command, env=env, job_name=lease.job_name,
                           on_job_ready=lease.ready, on_job_stopped=lease.stopped, on_poll=lease.poll, **kwargs)
        record.update(status="complete" if result.returncode == 0 else "failed", exitCode=result.returncode)
        return result
    except BaseException as error:
        quarantine = isinstance(error, CleanupFailed)
        record.update(status="busy" if isinstance(error, BuildResourcesBusy) else "failed",
                      error=f"{type(error).__name__}: {error}")
        raise
    finally:
        lease.finish(quarantined=quarantine)
