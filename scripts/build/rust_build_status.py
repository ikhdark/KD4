#!/usr/bin/env python3

from __future__ import annotations

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))


import argparse
from contextlib import contextmanager
from dataclasses import dataclass
from dataclasses import field
import errno
import hashlib
import json
import math
import os
from pathlib import Path
from pathlib import PureWindowsPath
import re
import shlex
import shutil
import stat
import subprocess
import sys
import time
import uuid
from typing import Callable
from typing import BinaryIO
from typing import Iterator
from typing import Mapping
from typing import Sequence
from typing import TextIO

REPO_ROOT = Path(__file__).resolve().parents[2]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.common.process_owner import run_owned, CleanupFailed  # noqa: E402

from scripts.common.tool_versions import cargo_lane_patterns  # noqa: E402
from scripts.build.rust_tool_env import cargo_package_specs, local_rust_env  # noqa: E402

from scripts.build.rust_build_status_support import (  # noqa: E402
    add_prune_arguments,
    bounded_size_workers,
    bytes_from_gib,
    directory_size_bytes,
    directory_sizes_bytes,
    format_bytes,
    lane_report,
    lane_report_lines,
    max_lane_bytes_from_args,
    max_total_lane_bytes_from_args,
    max_total_target_bytes_from_args,
    non_negative_int,
    positive_float,
    positive_int,
    target_disk_report,
    target_disk_report_lines,
    target_non_lane_size_bytes,
    target_optimize_report,
    warn_bytes_from_gib,
)

__all__ = [
    "add_prune_arguments",
    "bounded_size_workers",
    "bytes_from_gib",
    "directory_size_bytes",
    "directory_sizes_bytes",
    "format_bytes",
    "lane_report",
    "lane_report_lines",
    "max_lane_bytes_from_args",
    "max_total_lane_bytes_from_args",
    "max_total_target_bytes_from_args",
    "non_negative_int",
    "positive_float",
    "positive_int",
    "target_disk_report",
    "target_disk_report_lines",
    "target_non_lane_size_bytes",
    "target_optimize_report",
    "warn_bytes_from_gib",
]


_CARGO_LANE_PATTERNS = cargo_lane_patterns()
LANE_RE = re.compile(str(_CARGO_LANE_PATTERNS["lane_path_pattern"]), re.IGNORECASE)
SCRIPT_LANE_RE = re.compile(str(_CARGO_LANE_PATTERNS["script_lane_pattern"]))
JUST_LANE_RE = re.compile(str(_CARGO_LANE_PATTERNS["just_lane_pattern"]))
JUST_FIXED_LANE_RE = re.compile(str(_CARGO_LANE_PATTERNS["just_fixed_lane_pattern"]))
JUST_FIXED_LANE_NAMES = dict(_CARGO_LANE_PATTERNS["just_fixed_lane_names"])
RUST_PROCESS_NAMES = (
    "cargo",
    "cargo-clippy",
    "cargo-nextest",
    "clippy-driver",
    "rustc",
    "rustup",
)
RUST_WRAPPER_PROCESS_NAMES = (
    "just",
    "powershell",
    "pwsh",
)
RUST_PROCESS_CANDIDATE_NAMES = frozenset({
    *RUST_PROCESS_NAMES,
    *RUST_WRAPPER_PROCESS_NAMES,
    "python", "python3", "pythonw", "sh", "bash", "zsh", "cmd",
})
BYTES_PER_KIB = 1024
BYTES_PER_MIB = BYTES_PER_KIB * 1024
BYTES_PER_GIB = BYTES_PER_MIB * 1024
DEFAULT_TARGET_WARN_BYTES = 250 * BYTES_PER_GIB
TIMESTAMPED_LANE_RE = re.compile(r"^(?P<base>.+)-\d{14}$")
# Reservations create siblings `<base>-2` through `<base>-65` only. A wider
# match would group names like `cargo-12345678` (an all-digit auto-lane
# digest) as siblings and let the warm budget prune them before they expire.
LANE_SUFFIX_RE = re.compile(r"^(?P<base>.+)-(?P<suffix>[2-9]|[1-5][0-9]|6[0-5])$")
# Pruning renames a lane to `<name>.trash-<17 digits>` before deleting it.
LANE_TRASH_RE = re.compile(r"\.trash-\d{17}$", re.IGNORECASE)
WINDOWS_RUST_PROCESS_FILTER = " OR ".join(
    f"Name = '{name}.exe'"
    for name in (*RUST_PROCESS_NAMES, *RUST_WRAPPER_PROCESS_NAMES)
)
WINDOWS_PROCESS_SCAN_TIMEOUT_SECONDS = 10
DEFAULT_LANE_SIZE_WORKERS = 2
MAX_LANE_SIZE_WORKERS = 4
DEFAULT_PRUNE_KEEP_WARM_PER_BASE = 1
DEFAULT_PRUNE_MAX_AGE_DAYS = 7.0
LANE_LAST_USED_STAMP = ".lane-last-used"
CARGO_LANES_ROOT_MARKER = ".codex-cargo-lanes-root"
CARGO_LANES_ROOT_MARKER_CONTENT = "codex-kd cargo lanes root v1"
PROTECTED_TARGET_DIR_NAMES = frozenset(
    {
        "debug",
        "dev-small",
        "doc",
        "lanes",
        "nextest",
        "package",
        "release",
    }
)
PROTECTED_TARGET_DIR_PREFIXES = ("ci-test", "publish-")
RUST_PROCESS_TOKEN_RE = re.compile(
    r"(?<![A-Za-z0-9_.-])(?:cargo|cargo-clippy|cargo-nextest|clippy-driver|rustc|rustup)(?:\.exe)?(?![A-Za-z0-9_.-])",
    re.IGNORECASE,
)
RUST_MIN_STACK_BYTES = "8388608"
LANE_COORDINATION_GRACE_SECONDS = 1.0
# Lanes of one family (`core-tests`, `core-tests-2`, ...) that may build at
# once when cold overflow is allowed. Each lane holds a whole Cargo target, so
# the bound limits memory and disk; maintenance keeps as many lanes warm.
DEFAULT_MAX_ACTIVE_LANES_PER_BASE = 2


def max_active_lanes_per_base(env: Mapping[str, str] = os.environ) -> int:
    """`CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE`, at least one lane."""
    try:
        value = int(env.get("CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE", "").strip())
    except ValueError:
        return DEFAULT_MAX_ACTIVE_LANES_PER_BASE
    return max(1, value)







class CargoLanesRootValidationError(ValueError):
    pass


class RustProcessScanError(RuntimeError):
    """Process discovery failed: Rust activity is unknown, not absent."""


def default_cargo_lanes_root(repo_root: Path = REPO_ROOT) -> Path:
    return (repo_root / "codex-rs" / "target" / "lanes").absolute()


def cargo_lanes_root(
    repo_root: Path = REPO_ROOT,
    env: Mapping[str, str] = os.environ,
) -> Path:
    raw = env.get("CODEX_CARGO_LANES_ROOT", "").strip()
    if not raw:
        return default_cargo_lanes_root(repo_root)
    path = Path(raw).expanduser()
    if not path.is_absolute():
        path = repo_root / path
    return path.absolute()


def _checked_cargo_lanes_path(path: Path) -> Path:
    # Inspect the original spelling, including ancestors, before resolving it.
    # Otherwise a default-path junction can authorize cleanup of its destination.
    path = path.expanduser().absolute()
    for component in (path, *path.parents):
        observation = _lane_path_observation(component)
        if observation is not None and _observation_is_indirect(observation):
            raise CargoLanesRootValidationError(
                f"refusing indirect Cargo lanes root {path}: {component}"
            )
    return path.resolve()


def validate_cargo_lanes_root(
    repo_root: Path = REPO_ROOT,
    env: Mapping[str, str] = os.environ,
) -> Path:
    lane_root = _checked_cargo_lanes_path(cargo_lanes_root(repo_root, env))
    if lane_root == default_cargo_lanes_root(repo_root):
        return lane_root

    marker = lane_root / CARGO_LANES_ROOT_MARKER
    try:
        marker_is_valid = (
            lane_root.is_dir()
            and not marker.is_symlink()
            and stat.S_ISREG(marker.stat().st_mode)
            and marker.read_text(encoding="utf-8").strip()
            == CARGO_LANES_ROOT_MARKER_CONTENT
        )
    except (OSError, UnicodeError):
        marker_is_valid = False
    if not marker_is_valid:
        raise CargoLanesRootValidationError(
            f"refusing to prune unrecognized Cargo lanes root {lane_root}; "
            f"custom roots require the {CARGO_LANES_ROOT_MARKER} marker"
        )
    return lane_root


@dataclass(frozen=True)
class RustProcessClassification:
    is_rust: bool
    lane_name: str | None
    target_dir: str | None = None
    checkout_root: str | None = None


@dataclass(frozen=True)
class RustProcess:
    pid: int
    name: str
    command_line: str
    cwd: str | None = None
    classification: RustProcessClassification = field(
        init=False,
        compare=False,
        repr=False,
    )

    def __post_init__(self) -> None:
        object.__setattr__(self, "classification", _classify_rust_process(self))


def rust_process_key(process: RustProcess) -> tuple[int, str, str]:
    return process.pid, process.name, process.command_line


@dataclass
class BuildStatusSnapshot:
    repo_root: Path
    processes: list[RustProcess]
    lane_dirs: list[Path]
    lane_names_by_process: dict[tuple[int, str, str], str]
    active_lanes: set[str]
    stale_lanes: list[Path]
    _lane_mtime: Callable[[Path], float] = field(repr=False)
    # Set when process evidence is missing; lanes then cannot be proven idle.
    process_scan_error: str | None = None
    # Kept in active_lanes for safety, but reported as quarantined, not active.
    quarantined_lanes: set[str] = field(default_factory=set)
    _lane_mtimes: dict[Path, float] = field(default_factory=dict, repr=False)
    _lane_generations: dict[Path, tuple[int, ...] | None] = field(
        default_factory=dict, repr=False
    )
    _lane_sizes: dict[Path, tuple[int, int]] = field(default_factory=dict, repr=False)
    _non_lane_sizes: dict[Path, tuple[int, int]] = field(
        default_factory=dict, repr=False
    )
    _target_size: tuple[int, int] | None = field(default=None, repr=False)

    @classmethod
    def collect(
        cls,
        *,
        repo_root: Path = REPO_ROOT,
        processes: Sequence[RustProcess] | None = None,
        lane_mtime: Callable[[Path], float] | None = None,
    ) -> "BuildStatusSnapshot":
        process_scan_error = None
        if processes is None:
            try:
                processes = active_rust_processes()
            except RustProcessScanError as exc:
                processes, process_scan_error = [], str(exc)
        process_list = list(processes)
        # A hidden command line (e.g. an elevated cargo) could name any lane.
        hidden = [
            process.pid
            for process in process_list
            if process.classification.is_rust and not process.command_line.strip()
        ]
        if hidden and process_scan_error is None:
            process_scan_error = (
                "command line unreadable for Rust process id(s) "
                + ", ".join(str(pid) for pid in sorted(hidden))
            )
        lane_root = cargo_lanes_root(repo_root)
        lane_dirs = existing_lane_dirs(lane_root)
        lane_names_by_target: dict[str, str] | None = None
        lane_names_by_process: dict[tuple[int, str, str], str] = {}
        for process in process_list:
            lane_name = lane_name_for_process(process)
            observation = process.classification
            target = observation.target_dir
            root = observation.checkout_root
            unresolved_target = False
            if target is not None:
                absolute = Path(target).is_absolute() or PureWindowsPath(target).is_absolute()
                if not absolute and process.cwd:
                    target = str(Path(process.cwd) / target)
                    absolute = True
                if absolute:
                    if lane_names_by_target is None:
                        lane_names_by_target = {}
                        for path in lane_dirs:
                            lane_names_by_target.setdefault(_process_path_key(path), path.name)
                    lane_name = lane_names_by_target.get(_process_path_key(target))
                else:
                    lane_name = None
                    unresolved_target = True
            elif root is None or _process_path_key(root) != _process_path_key(repo_root):
                lane_name = None
            if lane_name is not None:
                lane_names_by_process[rust_process_key(process)] = lane_name
            elif observation.is_rust and (unresolved_target or target is None and root is None) and process_scan_error is None:
                process_scan_error = "Rust process checkout/target ownership is unknown; pruning remains unavailable"
        active_lanes = (
            set(lane_names_by_process.values())
            | env_active_lane_names()
            | locked_lane_names(lane_dirs)
        )
        # Compare case-insensitively: NTFS reuses an existing dir's on-disk
        # casing while processes/env advertise the invocation's casing
        # (cargo-lane.ps1 matches with OrdinalIgnoreCase).
        active_lanes_folded = {name.casefold() for name in active_lanes}
        stale_lanes = [
            path
            for path in lane_dirs
            if path.name.casefold() not in active_lanes_folded
        ]
        return cls(
            repo_root=repo_root,
            processes=process_list,
            lane_dirs=lane_dirs,
            lane_names_by_process=lane_names_by_process,
            active_lanes=active_lanes,
            stale_lanes=stale_lanes,
            _lane_mtime=(lane_last_used_mtime if lane_mtime is None else lane_mtime),
            _lane_generations={
                path: _lane_prune_generation(path) for path in lane_dirs
            },
            process_scan_error=process_scan_error,
            quarantined_lanes={
                path.name
                for path in lane_dirs
                if os.path.exists(path / ".lane-cleanup-unconfirmed")
            },
        )

    def lane_name_for(self, process: RustProcess) -> str | None:
        return self.lane_names_by_process.get(rust_process_key(process))

    def lane_mtime(self, path: Path) -> float:
        if path not in self._lane_mtimes:
            self._lane_mtimes[path] = self._lane_mtime(path)
        return self._lane_mtimes[path]

    def target_size(self) -> tuple[int, int]:
        if self._target_size is not None:
            return self._target_size
        target_root = (self.repo_root / "codex-rs" / "target").resolve()
        lane_root = cargo_lanes_root(self.repo_root).resolve()
        sizes = {
            path: (0, 0)
            for path in self.lane_dirs
            if path.resolve().is_relative_to(target_root)
        }
        partitions: dict[Path, tuple[int, int]] = {}
        result = directory_size_bytes(
            target_root,
            subtree_sizes=sizes,
            partition_sizes=partitions,
            partition_exclude=lane_root,
        )
        self._lane_sizes.update(sizes)
        self._non_lane_sizes[lane_root] = (
            sum(size for size, _ in partitions.values()),
            sum(errors for _, errors in partitions.values()),
        )
        self._target_size = result
        return result

    def non_lane_size(self, lane_root: Path, *, size_workers: int) -> tuple[int, int]:
        lane_root = lane_root.resolve()
        if lane_root not in self._non_lane_sizes:
            self._non_lane_sizes[lane_root] = target_non_lane_size_bytes(
                repo_root=self.repo_root, lane_root=lane_root, size_workers=size_workers
            )
        return self._non_lane_sizes[lane_root]

    def lane_sizes(
        self,
        paths: Sequence[Path],
        *,
        size_workers: int,
        lane_size: Callable[[Path], tuple[int, int]] | None = None,
    ) -> dict[Path, tuple[int, int]]:
        missing = [path for path in paths if path not in self._lane_sizes]
        if missing:
            size_func = directory_size_bytes if lane_size is None else lane_size
            self._lane_sizes.update(
                directory_sizes_bytes(
                    missing,
                    size_workers=size_workers,
                    size_func=size_func,
                )
            )
        return {path: self._lane_sizes[path] for path in paths}


def active_rust_processes() -> list[RustProcess]:
    if os.name == "nt":
        return active_rust_processes_windows()
    return active_rust_processes_posix()


def active_rust_processes_posix() -> list[RustProcess]:
    """Observe full command lines; a failed or malformed scan is never idle."""

    try:
        result = subprocess.run(
            ["ps", "-A", "-o", "pid=", "-o", "comm=", "-o", "args="],
            check=True,
            capture_output=True,
            text=True,
            timeout=WINDOWS_PROCESS_SCAN_TIMEOUT_SECONDS,
            env={**os.environ, "LC_ALL": "C"},
        )

        if not result.stdout.strip():
            raise ValueError("empty process snapshot")

        processes = []

        for line in result.stdout.splitlines():
            if not line.strip():
                continue

            fields = line.split(None, 2)

            if len(fields) < 2:
                raise ValueError("missing process fields")

            pid = int(fields[0])

            if pid == os.getpid():
                continue

            process = RustProcess(
                pid=pid,
                name=Path(fields[1]).name,
                command_line=fields[2] if len(fields) == 3 else "",
            )

            if process.classification.is_rust:
                processes.append(process)

        return processes

    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        raise RustProcessScanError(f"POSIX Rust process scan failed: {exc}") from exc


def active_rust_processes_windows() -> list[RustProcess]:
    shell = shutil.which("pwsh") or shutil.which("powershell") or "powershell"
    command = (
        "$selfPid = $PID; "
        f'Get-CimInstance Win32_Process -Filter "({WINDOWS_RUST_PROCESS_FILTER}) '
        'AND ProcessId != $selfPid" -ErrorAction Stop | '
        # CIM can retain a row after exit while its command line is already gone.
        # Requery only ambiguous rows; a still-hidden process remains fail-closed.
        "ForEach-Object { if ([string]::IsNullOrWhiteSpace($_.CommandLine)) { "
        'Get-CimInstance Win32_Process -Filter "ProcessId=$($_.ProcessId)" -ErrorAction Stop '
        "} else { $_ } } | "
        "Select-Object Name,ProcessId,CommandLine | ConvertTo-Json -Compress"
    )
    try:
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", command],
            check=True,
            capture_output=True,
            text=True,
            timeout=WINDOWS_PROCESS_SCAN_TIMEOUT_SECONDS,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
        raise RustProcessScanError(f"Windows Rust process scan failed: {exc}") from exc

    if not result.stdout.strip():
        return []
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RustProcessScanError(
            f"Windows Rust process scan returned invalid JSON: {exc}"
        ) from exc
    rows = payload if isinstance(payload, list) else [payload]
    processes = []
    for row in rows:
        if not isinstance(row, dict):
            continue
        name = str(row.get("Name") or "")
        command_line = str(row.get("CommandLine") or "")
        try:
            pid = int(row.get("ProcessId"))
        except (TypeError, ValueError):
            continue
        process = RustProcess(pid=pid, name=name, command_line=command_line)
        if process.classification.is_rust:
            processes.append(process)
    return processes


def _lane_name_from_command_line(command_line: str) -> str | None:
    if match := LANE_RE.search(command_line):
        return match.group(1)
    if match := SCRIPT_LANE_RE.search(command_line):
        return match.group(1)
    if match := JUST_LANE_RE.search(command_line):
        return match.group(1)
    if match := JUST_FIXED_LANE_RE.search(command_line):
        return JUST_FIXED_LANE_NAMES[match.group(1)]
    return None


def _process_path_key(path: str | Path) -> str:
    value = str(path)
    if PureWindowsPath(value).is_absolute():
        return str(PureWindowsPath(value)).replace("\\", "/").rstrip("/").casefold()
    return str(Path(value).resolve()).replace("\\", "/").rstrip("/")


def _classify_rust_process(process: RustProcess) -> RustProcessClassification:
    executable = process.name.lower().removesuffix(".exe")
    if executable not in RUST_PROCESS_CANDIDATE_NAMES:
        return RustProcessClassification(False, None)
    try:
        tokens = [token.strip('"') for token in shlex.split(process.command_line, posix=False)]
    except ValueError:
        tokens = []
    scripts = {"cargo-lane.ps1", "rust_build_status.py"}
    script = None
    if executable in {"python", "python3", "pythonw", "powershell", "pwsh"}:
        for token in tokens[1:]:
            if token.lower() in {"-command", "-c", "-encodedcommand"}:
                break
            if Path(token.replace("\\", "/")).name.lower() in scripts:
                script = token
                break
    is_rust = executable in RUST_PROCESS_NAMES or script is not None or (
        executable == "just"
        and (JUST_LANE_RE.search(process.command_line) is not None
             or JUST_FIXED_LANE_RE.search(process.command_line) is not None)
    )
    if executable in {*RUST_WRAPPER_PROCESS_NAMES, "sh", "bash", "zsh", "cmd"}:
        candidates = tokens
        for index, token in enumerate(tokens):
            if token.lower() in {"-command", "-c", "/c"} and index + 1 < len(tokens):
                try:
                    candidates = shlex.split(tokens[index + 1].strip("\"'"))[:1]
                except ValueError:
                    candidates = []
                break
        is_rust = is_rust or any(
            Path(token.replace("\\", "/")).name.lower().removesuffix(".exe")
            in RUST_PROCESS_NAMES
            for token in candidates
        )
    if not is_rust:
        return RustProcessClassification(False, None)
    target = None
    for index, token in enumerate(tokens):
        if token == "--":
            break
        if token == "--target-dir" and index + 1 < len(tokens):
            target = tokens[index + 1]
        elif token.startswith("--target-dir="):
            target = token.split("=", 1)[1].strip('"')
        elif executable == "rustc" and token == "--out-dir" and index + 1 < len(tokens):
            output = tokens[index + 1]
            if match := LANE_RE.search(output):
                target = output[:match.end(1)]
    root = None
    if script and (Path(script).is_absolute() or PureWindowsPath(script).is_absolute()):
        root = str((PureWindowsPath(script) if PureWindowsPath(script).is_absolute() else Path(script)).parents[2])
    return RustProcessClassification(is_rust, _lane_name_from_command_line(process.command_line), target, root)


def observe_rust_process(process: RustProcess) -> RustProcess:
    return process


def is_rust_process(process: RustProcess) -> bool:
    return process.classification.is_rust


def lane_name_for_process(process: RustProcess) -> str | None:
    return process.classification.lane_name


def shared_target_rust_processes(
    processes: Sequence[RustProcess],
    lane_names_by_process: Mapping[tuple[int, str, str], str] | None = None,
) -> list[RustProcess]:
    shared = []
    for process in processes:
        observed = observe_rust_process(process)
        lane_name = (
            lane_names_by_process.get(rust_process_key(observed))
            if lane_names_by_process is not None
            else observed.classification.lane_name
        )
        if (
            observed.classification.is_rust
            and lane_name is None
            and "nextest show-config" not in observed.command_line
        ):
            shared.append(observed)
    return shared


def has_shared_target_rust_jobs(processes: Sequence[RustProcess] | None = None) -> bool:
    processes = active_rust_processes() if processes is None else processes
    return bool(shared_target_rust_processes(processes))


def cargo_lock_is_busy(target_dir: Path, *, profile: str | None = None) -> bool:
    if _binary_file_lock_is_busy(target_dir / ".rust-test-runner.lock"):
        return True
    return _cargo_profile_locks_are_busy(target_dir, profile=profile)


def _cargo_profile_locks_are_busy(target_dir: Path, *, profile: str | None) -> bool:
    # Cargo locks each profile directory, optionally below a target triple.
    # Inspect only those two levels; never follow junctions into other trees.
    # Admission can ignore unrelated Cargo profiles; lane reservation/pruning
    # must keep the default whole-target probe. Legacy runner leases remain
    # exclusive for their whole invocation, including all child processes.
    try:
        for child in target_dir.iterdir():
            if not child.is_dir():
                continue
            if is_indirect_directory(child):
                return True
            if (profile is None or child.name == profile) and _cargo_lock_file_is_busy(
                child / ".cargo-lock"
            ):
                return True
            profiles = child.iterdir() if profile is None else (child / profile,)
            for candidate in profiles:
                if candidate.is_dir():
                    if is_indirect_directory(candidate):
                        return True
                    if _cargo_lock_file_is_busy(candidate / ".cargo-lock"):
                        return True
    except OSError:
        # A disappearing child does not prove the remaining profiles are idle.
        return True
    return False


def _cargo_lock_file_is_busy(lock_path: Path) -> bool:
    try:
        if not stat.S_ISREG(lock_path.stat().st_mode):
            return False
    except FileNotFoundError:
        return False
    except OSError:
        # Cleanup is destructive. If an existing lock cannot be inspected,
        # conservatively treat its target as busy instead of pruning it.
        return True
    handle: TextIO | None = None
    try:
        handle = lock_path.open("r+")
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
            return False
        except OSError:
            return True
    except OSError:
        return True
    finally:
        if handle is not None:
            handle.close()


def has_shared_target_cargo_lock(*, repo_root: Path = REPO_ROOT) -> bool:
    return cargo_lock_is_busy(repo_root / "codex-rs" / "target")


def locked_lane_names(lane_dirs: Sequence[Path]) -> set[str]:
    return {
        path.name
        for path in lane_dirs
        if cargo_lock_is_busy(path) or lane_active_lock_is_held(path)
    }


def lane_active_lock_is_held(lane_dir: Path) -> bool:
    try:
        if (lane_dir / ".lane-cleanup-unconfirmed").exists():
            return True
    except OSError:
        # Python 3.11 raises for an unreadable marker; unknown means busy.
        return True
    lock_path = lane_dir / ".lane-active.lock"
    try:
        if not stat.S_ISREG(lock_path.stat().st_mode):
            return False
    except FileNotFoundError:
        return False
    except OSError:
        return True
    handle: TextIO | None = None
    try:
        try:
            handle = lock_path.open("a", encoding="utf-8")
        except PermissionError:
            # cargo-lane.ps1 holds the lock open with FileShare::None, so a
            # sharing violation at open means the lane is ACTIVE. Treating it
            # as "not held" here would mark a live lane prunable.
            return True
        handle.seek(0)
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return True
        _release_binary_file_lock(handle)
        return False
    except OSError:
        return True
    finally:
        if handle is not None:
            handle.close()


def _is_file_lock_contention(exc: OSError) -> bool:
    return getattr(exc, "winerror", None) in {32, 33, 36} or exc.errno in {
        errno.EACCES,
        errno.EAGAIN,
    }


@contextmanager
def cargo_lane_coordination_lock(
    lane_root: Path,
    *,
    timeout_seconds: float = 30.0,
) -> Iterator[None]:
    """Serialize lane creation with the final prune check and rename."""

    lock_path = lane_root / ".lane-coordination.lock"
    deadline = time.monotonic() + timeout_seconds
    handle = None
    retry_delay = 0.002
    while handle is None:
        try:
            handle = lock_path.open("a+b")
        except OSError as exc:
            if not _is_file_lock_contention(exc):
                raise
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"timed out waiting for lane coordination lock {lock_path}"
                )
            time.sleep(min(retry_delay, max(0.0, deadline - time.monotonic())))
            retry_delay = min(0.02, retry_delay * 2)
    acquired = False
    try:
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b"\0")
            handle.flush()
        handle.seek(0)
        while True:
            try:
                if os.name == "nt":
                    import msvcrt

                    msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    import fcntl

                    fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                acquired = True
                break
            except OSError as exc:
                if not _is_file_lock_contention(exc):
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"timed out waiting for lane coordination lock {lock_path}"
                    )
                time.sleep(min(retry_delay, max(0.0, deadline - time.monotonic())))
                retry_delay = min(0.02, retry_delay * 2)
        yield
    finally:
        try:
            if acquired:
                _release_binary_file_lock(handle)
        finally:
            handle.close()


def _release_binary_file_lock(handle: BinaryIO) -> None:
    handle.seek(0)
    if os.name == "nt":
        import msvcrt

        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
    else:
        import fcntl

        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def _try_acquire_binary_file_lock(path: Path) -> BinaryIO | None:
    try:
        handle = path.open("a+b")
    except OSError as exc:
        if _is_file_lock_contention(exc):
            return None
        raise
    acquired = False
    try:
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b"\0")
            handle.flush()
        handle.seek(0)
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            if _is_file_lock_contention(exc):
                return None
            raise
        acquired = True
        return handle
    finally:
        if not acquired:
            handle.close()


def _binary_file_lock_is_busy(path: Path) -> bool:
    """An existing, uninspectable lease is busy; a missing lease is not."""
    try:
        if not stat.S_ISREG(path.stat().st_mode):
            return True
    except FileNotFoundError:
        return False
    except OSError:
        return True
    try:
        handle = _try_acquire_binary_file_lock(path)
        if handle is None:
            return True
        try:
            _release_binary_file_lock(handle)
        finally:
            handle.close()
        return False
    except OSError:
        return True





def _safe_lane_name(value: str) -> str:
    if (
        not value
        or re.fullmatch(r"[A-Za-z0-9_.-]+", value) is None
        or re.fullmatch(r"\.+", value) is not None
        or LANE_TRASH_RE.search(value) is not None
    ):
        raise ValueError(f"invalid Cargo lane name {value!r}")
    return value


def source_root_identity(repo_root: Path) -> str:
    return os.path.normcase(os.path.abspath(repo_root))


def source_lane_name(base_lane: str, source_identity: str) -> str:
    """Qualify a lane family by the source it compiles (`<base>-ws-<sha256>`)."""
    source_key = hashlib.sha256(source_identity.encode("utf-8")).hexdigest()
    return _safe_lane_name(f"{_safe_lane_name(base_lane)}-ws-{source_key}")


def _cargo_profile_options(command: Sequence[str]) -> list[str]:
    """Cargo artifact profile only; never Nextest policy or program arguments."""
    if len(command) >= 4 and Path(command[0]).stem.lower() == "rustup" and command[1] == "run":
        cargo_index = 4 if command[2] == "--install" else 3
        if cargo_index < len(command) and Path(command[cargo_index]).stem.lower() == "cargo":
            return _cargo_profile_options(command[cargo_index:])
    cargo_index = _cargo_subcommand_index(command) if command and Path(command[0]).stem.lower() == "cargo" else None
    subcommand = command[cargo_index] if cargo_index is not None else None
    nextest = subcommand == "nextest"
    profile: list[str] = []
    tokens = iter(command[(cargo_index + 1) if cargo_index is not None else 1:])
    for token in tokens:
        if token == "--":
            break
        if subcommand == "watch" and token.startswith("-") and not token.startswith("--"):
            short = token[1:]
            flags = len(short) - len(short.lstrip("cqNhV"))
            if short[flags:flags + 1] == "x":
                token = "-" + short[flags:]
        if subcommand == "watch" and (token in {"--exec", "-x"} or token.startswith(("--exec=", "-x"))):
            value = next(tokens, "") if token in {"--exec", "-x"} else token.split("=", 1)[1] if token.startswith("--exec=") else token[2:].removeprefix("=")
            embedded = shlex.split(value)
            profile = _cargo_profile_options(["cargo", *embedded]) or profile
        elif token in {"--release", "-r"}:
            profile = ["--profile", "release"]
        elif token == ("--cargo-profile" if nextest else "--profile"):
            profile = ["--profile", next(tokens, "")]
        elif token.startswith(("--cargo-profile=" if nextest else "--profile=")):
            profile = ["--profile", token.split("=", 1)[1]]
        elif token in {"--profile", "-P", "--config", "--target", "--features", "-F", "-p", "--package", "--target-dir", "--manifest-path", "-C", "-Z", "--watch", "-w", "--shell", "-s", "--workdir", "--ignore", "-i", "--env", "-E", "--env-file", "--delay", "-d"}:
            next(tokens, None)
    return profile


def _auto_lane_base(command: Sequence[str]) -> str:
    signature = " ".join(command).strip()
    release = _cargo_profile_options(command) == ["--profile", "release"]
    from scripts.build.rust_tool_env import CORE_BUILD_PACKAGES, cargo_package_specs

    # Affinity also covers Cargo watch's embedded --exec/-x command strings.
    packages = cargo_package_specs(signature.split())
    if packages:
        base = (
            "core-tests"
            if any(package in packages for package in CORE_BUILD_PACKAGES)
            else packages[0]
        )
    elif command:
        program = Path(command[0]).stem
        # Normalize only Cargo's own display options, never arguments passed
        # through to tests/programs or embedded plugin commands.
        subcommand = _cargo_subcommand_index(command)
        if (
            program == "cargo"
            and subcommand is not None
            and command[subcommand]
            in {
                "build",
                "b",
                "check",
                "c",
                "test",
                "t",
                "clippy",
                "doc",
                "d",
                "rustc",
                "rustdoc",
                "bench",
                "run",
                "r",
            }
        ):
            normalized = []
            tokens = iter(command)
            for token in tokens:
                if token == "--":
                    normalized.extend([token, *tokens])
                    break
                if token in {"--quiet", "-q", "--verbose", "-v", "-vv"}:
                    continue
                if token == "--color":
                    next(tokens, None)
                    continue
                if token.startswith("--color="):
                    continue
                normalized.append(token)
                if token in {
                    "--config",
                    "-C",
                    "-Z",
                    "--manifest-path",
                    "--target-dir",
                    "--target",
                    "--profile",
                    "--features",
                    "-F",
                    "--bin",
                    "--example",
                    "--test",
                    "--bench",
                    "--jobs",
                    "-j",
                    "--message-format",
                    "--exclude",
                    "--package",
                    "-p",
                }:
                    normalized.append(next(tokens, ""))
            signature = " ".join(normalized).strip()
        digest = hashlib.sha1(signature.encode("utf-8")).hexdigest()[:8]
        base = f"{program}-{digest}"
    else:
        base = "auto"
    safe = re.sub(r"[^A-Za-z0-9_.-]", "-", base).strip("-") or "auto"
    return f"{safe}-release" if release else safe


def initialize_cargo_lanes_root(repo_root: Path, lane_root: Path) -> Path:
    lane_root = _checked_cargo_lanes_path(lane_root)
    default_root = default_cargo_lanes_root(repo_root)
    if not lane_root.exists():
        lane_root.mkdir(parents=True)
    marker = lane_root / CARGO_LANES_ROOT_MARKER
    if marker.exists():
        try:
            valid_marker = (
                not marker.is_symlink()
                and stat.S_ISREG(marker.stat().st_mode)
                and marker.read_text(encoding="utf-8").strip()
                == CARGO_LANES_ROOT_MARKER_CONTENT
            )
        except (OSError, UnicodeError):
            valid_marker = False
        if not valid_marker:
            raise CargoLanesRootValidationError(
                f"invalid Cargo lanes root marker {marker}"
            )
    else:
        entries = list(lane_root.iterdir())
        if lane_root != default_root and entries:
            raise CargoLanesRootValidationError(
                f"custom Cargo lanes root must be empty or contain {CARGO_LANES_ROOT_MARKER}: "
                f"{lane_root}"
            )
        marker.write_text(f"{CARGO_LANES_ROOT_MARKER_CONTENT}\n", encoding="utf-8")
    return lane_root


@dataclass(frozen=True)
class LaneReservationCandidate:
    name: str
    path: Path
    observation: os.stat_result | None


def _observation_is_indirect(observation: os.stat_result) -> bool:
    file_attributes = getattr(observation, "st_file_attributes", 0) or 0
    return stat.S_ISLNK(observation.st_mode) or bool(
        file_attributes & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0)
    )


def _lane_path_observation(path: Path) -> os.stat_result | None:
    try:
        return os.lstat(path)
    except FileNotFoundError:
        return None


def _lane_prune_generation(path: Path) -> tuple[int, ...] | None:
    """Bind eligibility to both the directory and its last reservation."""
    observed = _lane_path_observation(path)
    if observed is None:
        return None
    stamp = _lane_path_observation(path / LANE_LAST_USED_STAMP)
    return (
        observed.st_dev,
        observed.st_ino,
        observed.st_ctime_ns,
        observed.st_mtime_ns,
        *(
            ()
            if stamp is None
            else (
                stamp.st_dev,
                stamp.st_ino,
                stamp.st_mtime_ns,
                stamp.st_ctime_ns,
                stamp.st_size,
            )
        ),
    )


def _detach_expendable_lane_cache(path: Path, trash: Path) -> bool:
    """Keep legacy published evidence paths continuously reachable.

    Called under lane coordination after the final activity/generation checks.
    Only immediate children move here; recursive deletion stays outside the lock.
    Stable lock inodes remain at the original lane path for future waiters.
    """
    if not (path / "test-runner-logs").exists():
        path.replace(trash)
        return True
    keep = {
        "test-runner-logs",
        ".lane-active.lock",
        ".rust-test-runner.lock",
        ".lane-cleanup-unconfirmed",
        LANE_LAST_USED_STAMP,
    }
    children = [child for child in path.iterdir() if child.name not in keep]
    if not children:
        return False
    trash.mkdir()
    for child in children:
        child.replace(trash / child.name)
    return True


def _lane_reservation_candidates(
    lane_root: Path,
    base_lane: str,
    *,
    prefer_warm: bool,
) -> list[LaneReservationCandidate]:
    candidates: list[LaneReservationCandidate] = []
    if prefer_warm:
        pattern = re.compile(rf"^{re.escape(base_lane)}(?:-\d+)?$")
        warm: list[LaneReservationCandidate] = []
        with os.scandir(lane_root) as entries:
            for entry in entries:
                if pattern.fullmatch(entry.name) is None:
                    continue
                try:
                    observation = entry.stat(follow_symlinks=False)
                except OSError:
                    continue
                if not stat.S_ISDIR(observation.st_mode) or _observation_is_indirect(
                    observation
                ):
                    continue
                warm.append(
                    LaneReservationCandidate(
                        name=entry.name,
                        path=Path(entry.path),
                        observation=observation,
                    )
                )

        def warm_sort_key(
            candidate: LaneReservationCandidate,
        ) -> tuple[float, str, str]:
            assert candidate.observation is not None
            return (
                -lane_last_used_mtime(candidate.path),
                candidate.name.casefold(),
                candidate.name,
            )

        warm.sort(key=warm_sort_key)
        candidates.extend(warm)
    observed_names = {candidate.name for candidate in candidates}
    for name in [base_lane, *(f"{base_lane}-{index}" for index in range(2, 66))]:
        if name not in observed_names:
            candidates.append(
                LaneReservationCandidate(
                    name=name,
                    path=lane_root / name,
                    observation=None,
                )
            )
            observed_names.add(name)
    return candidates


def _nonnegative_wait_seconds(value: str | float) -> float:
    seconds = float(value)
    if not math.isfinite(seconds) or seconds < 0:
        raise ValueError("wait must be a finite nonnegative number")
    return seconds


def _lanes_in_use(candidates: Sequence[LaneReservationCandidate]) -> int:
    """Existing lanes of one family that hold a reservation or a Cargo lock."""
    return len(_busy_lane_paths(candidates))


def _busy_lane_paths(candidates: Sequence[LaneReservationCandidate]) -> list[Path]:
    return [
        candidate.path
        for candidate in candidates
        if candidate.path.is_dir()
        and (
            lane_active_lock_is_held(candidate.path)
            or cargo_lock_is_busy(candidate.path)
        )
    ]


@contextmanager
def reserve_cargo_lane(
    *,
    repo_root: Path,
    requested_lane: str,
    command: Sequence[str],
    lane_root: Path | None = None,
    lock_timeout_seconds: float = LANE_COORDINATION_GRACE_SECONDS,
    warm_wait_seconds: float = 0.0,
    allow_cold_overflow: bool = False,
    build_context: Mapping[str, object] | None = None,
    max_active_lanes: int | None = None,
    source_identity: str | None = None,
    rank: Callable[[Path], int] | None = None,
) -> Iterator[tuple[str, Path]]:
    """Reserve a lane; `rank` orders warm candidates (lower first) before LRU."""
    warm_wait_seconds = _nonnegative_wait_seconds(warm_wait_seconds)
    lock_timeout_seconds = _nonnegative_wait_seconds(lock_timeout_seconds)
    if max_active_lanes is not None and max_active_lanes < 1:
        raise ValueError("max active lanes must be at least 1")
    explicit = requested_lane != "auto"
    root = initialize_cargo_lanes_root(
        repo_root,
        lane_root if lane_root is not None else cargo_lanes_root(repo_root),
    )
    # Relative dep-info cannot authenticate a legacy or another checkout's
    # backdated sources. All roots, including the default, need custody.
    base_lane = source_lane_name(
        requested_lane if explicit else _auto_lane_base(command),
        source_identity or source_root_identity(repo_root),
    )
    active_handle: BinaryIO | None = None
    target_dir: Path | None = None
    resolved_lane: str | None = None
    deadline = time.monotonic() + max(0.0, warm_wait_seconds)
    announced_wait = False
    retry_delay = 0.01
    while active_handle is None:
        busy_reusable = False
        with cargo_lane_coordination_lock(root, timeout_seconds=lock_timeout_seconds):
            candidates = _lane_reservation_candidates(root, base_lane, prefer_warm=True)
            warm_paths = {
                candidate.path for candidate in candidates if _is_warm_lane_dir(candidate.path)
            }
            compatible_paths = {
                path
                for path in warm_paths
                if build_context is None
                or _read_lane_build_context(path) == build_context
            }
            candidates.sort(
                key=lambda candidate: (
                    candidate.path not in compatible_paths,
                    candidate.path not in warm_paths,
                    rank(candidate.path) if rank is not None and candidate.path in warm_paths else 0,
                    # Cold placeholders keep deterministic base/overflow order.
                    0
                    if not explicit
                    or candidate.path in warm_paths
                    or candidate.name == base_lane
                    else int(candidate.name.removeprefix(base_lane + "-")),
                )
            )
            # Bound how many lanes of this family build at once, warm or cold:
            # at the bound, wait for one to finish.
            at_capacity = (
                max_active_lanes is not None
                and _lanes_in_use(candidates) >= max_active_lanes
            )
            for candidate in () if at_capacity else candidates:
                candidate_name = _safe_lane_name(candidate.name)
                candidate_dir = candidate.path
                observation = _lane_path_observation(candidate_dir)
                if observation is not None and _observation_is_indirect(observation):
                    raise CargoLanesRootValidationError(
                        f"refusing indirect Cargo lane path {candidate_dir}"
                    )
                if (candidate_dir / ".lane-cleanup-unconfirmed").exists():
                    continue
                warm = candidate_dir in warm_paths
                compatible = candidate_dir in compatible_paths
                if busy_reusable and not compatible and (compatible_paths or not warm):
                    if not allow_cold_overflow or time.monotonic() < deadline:
                        break
                candidate_dir.mkdir(exist_ok=True)
                if cargo_lock_is_busy(candidate_dir):
                    busy_reusable |= compatible or not compatible_paths
                    continue
                active_handle = _try_acquire_binary_file_lock(
                    candidate_dir / ".lane-active.lock"
                )
                if active_handle is not None:
                    target_dir = candidate_dir.resolve()
                    resolved_lane = candidate_name
                    break
                busy_reusable |= compatible or not compatible_paths
        if active_handle is not None:
            break
        if not announced_wait:
            print(
                f"waiting for a reusable Cargo lane for {base_lane!r}",
                file=sys.stderr,
            )
            announced_wait = True
        # Release the coordination lock while waiting so owners can finish and
        # other work can reserve unrelated lanes.
        time.sleep(retry_delay)
        retry_delay = min(0.05, retry_delay * 1.5)
    assert active_handle is not None and target_dir is not None and resolved_lane is not None
    stamp = target_dir / LANE_LAST_USED_STAMP
    try:
        stamp.write_text(f"{time.time()}\n", encoding="utf-8")
        yield resolved_lane, target_dir
    finally:
        try:
            stamp.write_text(f"{time.time()}\n", encoding="utf-8")
        finally:
            try:
                _release_binary_file_lock(active_handle)
            finally:
                active_handle.close()


def _normalized_target_dir(path: str | Path) -> str:
    if not str(path).strip():
        raise ValueError("Cargo --target-dir requires a non-empty path")
    try:
        resolved = Path(path).expanduser().resolve(strict=False)
    except (OSError, RuntimeError, ValueError) as exc:
        raise ValueError(f"Cargo --target-dir {str(path)!r} is invalid: {exc}") from exc
    return os.path.normcase(str(resolved))


def _require_reserved_target_dir(candidate: str, target_dir: Path) -> None:
    if _normalized_target_dir(candidate) != _normalized_target_dir(target_dir):
        raise ValueError(
            f"Cargo --target-dir {candidate!r} does not match reserved lane "
            f"target {str(target_dir)!r}"
        )


def _cargo_watch_exec_with_target_dir(
    command: str, target_dir: Path, *, working_dir: Path | None = None
) -> str:
    separator = re.search(r"\s--(?=\s|$)", command)
    separator_index = separator.start() if separator else -1
    cargo_command = command if separator_index < 0 else command[:separator_index]
    target_pattern = re.compile(
        r'(?:^|\s)--target-dir(?:=(?:"(?P<double>[^\"]*)"|'
        r"'(?P<single>[^']*)'|(?P<bare>[^\s]+))|\s+(?:"
        r'"(?P<double_space>[^\"]*)"|'
        r"'(?P<single_space>[^']*)'|(?P<bare_space>[^\s]+)))"
    )
    target_matches = list(target_pattern.finditer(cargo_command))
    if (
        re.search(r"(?:^|\s)--target-dir(?:=|\s|$)", cargo_command)
        and not target_matches
    ):
        raise ValueError("Cargo watch exec command has a malformed --target-dir option")
    for target_match in target_matches:
        candidate = next(
            value for value in target_match.groupdict().values() if value is not None
        )
        _require_reserved_target_dir(
            str((working_dir or Path.cwd()) / candidate), target_dir
        )
    if target_matches:
        # Freeze relative paths before cargo-watch changes its working directory.
        target_arg = str(target_dir)
        quoted_target = (
            f'"{target_arg}"' if re.search(r"\s", target_arg) else target_arg
        )
        for match in reversed(target_matches):
            command = (
                command[: match.start()]
                + f" --target-dir {quoted_target}"
                + command[match.end() :]
            )
        return command
    build_commands = {
        "b",
        "c",
        "t",
        "r",
        "d",
        "clean",
        "rustdoc",
        "package",
        "install",
        "publish",
        "bench",
        "build",
        "check",
        "clippy",
        "doc",
        "fix",
        "llvm-cov",
        "run",
        "rustc",
        "test",
    }
    stripped = command.strip()
    if not stripped or stripped.split(maxsplit=1)[0] not in build_commands:
        raise ValueError("Unsupported Cargo watch exec command in a reserved lane")
    target_arg = str(target_dir)
    escaped_target = target_arg.replace('"', '\\"')
    quoted_target = (
        f'"{escaped_target}"' if re.search(r"\s", target_arg) else target_arg
    )
    insertion = f" --target-dir {quoted_target}"
    if separator_index >= 0:
        return command[:separator_index] + insertion + command[separator_index:]
    return command + insertion


# Nextest's -P/--profile selects test execution policy; its Cargo build profile
# is --cargo-profile.
NEXTEST_GLOBAL_OPTIONS_WITH_VALUES = {
    "--color",
    "--manifest-path",
    "--config-file",
    "--user-config-file",
    "--tool-config-file",
    "-P",
    "--profile",
}


def _cargo_subcommand_index(
    command: Sequence[str],
    *,
    start_index: int = 1,
    global_options_with_values: set[str] | None = None,
) -> int | None:
    index = start_index
    if index == 1 and index < len(command) and command[index].startswith("+"):
        index += 1
    if global_options_with_values is None:
        global_options_with_values = {"--color", "--config", "-C", "-Z"}
    while index < len(command):
        argument = command[index]
        if argument == "--":
            return None
        if not argument.startswith("-"):
            return index
        if argument in global_options_with_values:
            if index + 1 >= len(command):
                raise ValueError(f"Cargo global option {argument} requires a value")
            index += 2
            continue
        if any(
            argument.startswith(f"{option}=") for option in global_options_with_values
        ) or any(
            argument.startswith(option) and argument != option
            for option in global_options_with_values
            if len(option) == 2
        ):
            index += 1
            continue
        index += 1
    return None


def _cargo_target_dir_is_present(
    command: Sequence[str],
    *,
    start_index: int,
    target_dir: Path,
) -> bool:
    present = False
    index = start_index
    while index < len(command):
        argument = command[index]
        if argument == "--":
            break
        if argument == "--target-dir":
            if index + 1 >= len(command) or command[index + 1] == "--":
                raise ValueError("Cargo --target-dir requires a path value")
            _require_reserved_target_dir(command[index + 1], target_dir)
            present = True
            index += 2
            continue
        if argument.startswith("--target-dir="):
            _require_reserved_target_dir(
                argument.removeprefix("--target-dir="),
                target_dir,
            )
            present = True
        index += 1
    return present


def _cargo_command_with_target_dir(
    command: Sequence[str],
    target_dir: Path,
) -> list[str]:
    result = list(command)
    if (
        len(result) >= 4
        and Path(result[0]).stem.lower() == "rustup"
        and result[1] == "run"
    ):
        cargo_index = 4 if result[2] == "--install" else 3
        if (
            cargo_index < len(result)
            and Path(result[cargo_index]).stem.lower() == "cargo"
        ):
            return result[:cargo_index] + _cargo_command_with_target_dir(
                result[cargo_index:], target_dir
            )
    if len(result) < 2 or Path(result[0]).stem.lower() != "cargo":
        return result
    subcommand_index = _cargo_subcommand_index(result)
    if subcommand_index is None:
        return result
    subcommand = result[subcommand_index]
    target_arg = str(target_dir)
    if subcommand == "nextest":
        nextest_index = _cargo_subcommand_index(
            result,
            start_index=subcommand_index + 1,
            global_options_with_values=NEXTEST_GLOBAL_OPTIONS_WITH_VALUES,
        )
        if nextest_index is None:
            return result
        if result[nextest_index] not in {"archive", "run", "r", "list", "bench", "b"}:
            if result[nextest_index] in {"help", "show-config", "self"}:
                return result
            raise ValueError("Unsupported Cargo nextest command in a reserved lane")
        if _cargo_target_dir_is_present(
            result,
            start_index=nextest_index + 1,
            target_dir=target_dir,
        ):
            return result
        return [
            *result[: nextest_index + 1],
            "--target-dir",
            target_arg,
            *result[nextest_index + 1 :],
        ]
    if subcommand == "watch":
        index = subcommand_index + 1
        has_exec = False
        exec_arguments: list[tuple[int, str, str]] = []
        working_dir = Path.cwd()
        value_options = {
            "-d",
            "--delay",
            "--env-file",
            "-E",
            "--env",
            "--features",
            "-i",
            "--ignore",
            "-B",
            "-L",
            "--use-shell",
            "-w",
            "--watch",
            "-C",
            "--workdir",
        }
        while index < len(result):
            argument = result[index]
            # Clap also accepts -qxcheck and -qscommand. Split flag prefixes
            # before inspecting the value-taking option, preserving the flags.
            if argument.startswith("-") and not argument.startswith("--"):
                short = argument[1:]
                flags = len(short) - len(short.lstrip("cqNhV"))
                if flags and short[flags : flags + 1] in {
                    "x",
                    "s",
                    "d",
                    "i",
                    "E",
                    "B",
                    "L",
                    "w",
                    "C",
                }:
                    result[index : index + 1] = [
                        "-" + short[:flags],
                        "-" + short[flags:],
                    ]
                    index += 1
                    argument = result[index]
            if argument == "--":
                if index + 1 < len(result):
                    raise ValueError(
                        "Cargo watch positional commands cannot enforce a reserved target; use --exec/-x"
                    )
                break
            if (
                argument.startswith("-s")
                or argument == "--shell"
                or argument.startswith("--shell=")
            ):
                raise ValueError(
                    "Cargo watch --shell/-s is not allowed inside a reserved lane; use --exec/-x so --target-dir can be enforced"
                )
            if argument in {"-x", "--exec"}:
                if index + 1 >= len(result):
                    raise ValueError("Cargo watch --exec/-x requires a command")
                index += 1
                exec_arguments.append((index, "", result[index]))
                has_exec = True
            elif argument.startswith("--exec="):
                exec_arguments.append(
                    (index, "--exec=", argument.removeprefix("--exec="))
                )
                has_exec = True
            elif argument.startswith("-x"):
                exec_arguments.append((index, "-x", argument[2:].removeprefix("=")))
                has_exec = True
            elif argument in value_options:
                if index + 1 >= len(result):
                    raise ValueError(f"Cargo watch {argument} requires a value")
                index += 1
                if argument in {"-C", "--workdir"}:
                    working_dir = Path(result[index])
            elif argument.startswith("--workdir="):
                working_dir = Path(argument.removeprefix("--workdir="))
            elif argument.startswith("-C"):
                working_dir = Path(argument[2:].removeprefix("="))
            elif not argument.startswith("-"):
                raise ValueError(
                    "Cargo watch positional commands cannot enforce a reserved target; use --exec/-x"
                )
            index += 1
        for exec_index, prefix, exec_command in exec_arguments:
            result[exec_index] = prefix + _cargo_watch_exec_with_target_dir(
                exec_command, target_dir, working_dir=working_dir
            )
        if not has_exec:
            result[index:index] = [
                "-x",
                _cargo_watch_exec_with_target_dir("check", target_dir),
            ]
        return result
    if subcommand not in {
        "b",
        "c",
        "t",
        "r",
        "d",
        "clean",
        "rustdoc",
        "package",
        "install",
        "publish",
        "bench",
        "build",
        "check",
        "clippy",
        "doc",
        "fix",
        "llvm-cov",
        "run",
        "rustc",
        "test",
    }:
        if subcommand in {
            "add",
            "remove",
            "rm",
            "fetch",
            "fmt",
            "generate-lockfile",
            "locate-project",
            "login",
            "logout",
            "metadata",
            "new",
            "init",
            "owner",
            "search",
            "tree",
            "update",
            "vendor",
            "verify-project",
            "version",
            "help",
            "report",
            "read-manifest",
            "uninstall",
            "info",
        }:
            return result
        raise ValueError(
            f"Unsupported Cargo command {subcommand!r} in a reserved lane; use an explicit build command"
        )
    if _cargo_target_dir_is_present(
        result,
        start_index=subcommand_index + 1,
        target_dir=target_dir,
    ):
        return result
    return [
        *result[: subcommand_index + 1],
        "--target-dir",
        target_arg,
        *result[subcommand_index + 1 :],
    ]


def _guard_cargo_checkout_cwd(command: Sequence[str], repo_root: Path) -> None:
    """Reject the common wrong-root invocation before waiting for a build lane.

    Do not relocate the command: relative arguments and Cargo configuration must
    keep their caller-selected meaning. Plugins and explicit path overrides stay
    under Cargo's control rather than emulating their argument parsers here.
    """
    if not command or Path(command[0]).stem.lower() != "cargo":
        return
    index = _cargo_subcommand_index(command)
    if index is None or command[index] not in {
        "build",
        "b",
        "check",
        "c",
        "test",
        "t",
        "bench",
        "run",
        "r",
        "clippy",
        "doc",
        "d",
        "rustc",
        "rustdoc",
        "fix",
    }:
        return
    options = command[: command.index("--")] if "--" in command else command
    if any(
        arg in {"--help", "-h", "--manifest-path"}
        or arg.startswith(("--manifest-path=", "-C"))
        for arg in options
    ):
        return
    cwd = Path.cwd()
    if cwd != repo_root.resolve() or not (cwd / "codex-rs" / "Cargo.toml").is_file():
        return
    if any((directory / "Cargo.toml").is_file() for directory in (cwd, *cwd.parents)):
        return
    raise ValueError(
        "Cargo working directory is the checkout root; run from codex-rs "
        "to use its pinned toolchain and .cargo configuration. "
        "--manifest-path alone does not select codex-rs's pinned toolchain "
        "or .cargo configuration. No build lane was reserved."
    )
















def maintain_cargo_lanes(repo_root: Path, lane_root: Path) -> None:
    """Apply age/warm-lane cleanup; recursive byte accounting is opt-in."""

    def setting(name: str, default: int, minimum: int = 0) -> int:
        try:
            value = int(os.environ.get(name, str(default)))
        except ValueError:
            return default
        return value if value >= minimum else default

    previous_root = os.environ.get("CODEX_CARGO_LANES_ROOT")
    gc_handle = None
    try:
        stamp = lane_root / ".gc-stamp"
        interval = setting("CODEX_CARGO_LANE_GC_INTERVAL_HOURS", 1) * 3600
        if stamp.exists() and time.time() - stamp.stat().st_mtime < interval:
            return
        gc_handle = _try_acquire_binary_file_lock(lane_root / ".lane-gc.lock")
        if gc_handle is None:
            return
        # Another command may have completed maintenance since the first check.
        if stamp.exists() and time.time() - stamp.stat().st_mtime < interval:
            return
        retry_stamp = lane_root / ".gc-retry"
        if retry_stamp.exists() and time.time() - retry_stamp.stat().st_mtime < 60:
            return
        os.environ["CODEX_CARGO_LANES_ROOT"] = str(lane_root)
        prune_stale_lanes(
            repo_root=repo_root,
            # Keep every lane that may build at once warm, so overflow
            # siblings are not rebuilt cold after each maintenance pass.
            keep_warm_per_base=max_active_lanes_per_base(),
            max_age_days=setting("CODEX_CARGO_LANE_MAX_AGE_DAYS", 7, 1),
            max_lane_bytes=setting("CODEX_CARGO_LANE_MAX_LANE_BYTES", 0) or None,
            max_total_lane_bytes=setting("CODEX_CARGO_LANE_MAX_TOTAL_BYTES", 0) or None,
            max_total_target_bytes=setting("CODEX_CARGO_TARGET_MAX_TOTAL_BYTES", 0)
            or None,
        )
        stamp.write_text(f"{time.time()}\n", encoding="utf-8")
        retry_stamp.unlink(missing_ok=True)
    except (OSError, RuntimeError, ValueError) as exc:
        if gc_handle is not None:
            try:
                (lane_root / ".gc-retry").touch()
            except OSError:
                pass
        print(f"warning: Cargo lane pruning failed: {exc}", file=sys.stderr)
    finally:
        if gc_handle is not None:
            try:
                _release_binary_file_lock(gc_handle)
            finally:
                gc_handle.close()
        if previous_root is None:
            os.environ.pop("CODEX_CARGO_LANES_ROOT", None)
        else:
            os.environ["CODEX_CARGO_LANES_ROOT"] = previous_root


def _maintenance_launch_lease(lane_root: Path, *, release: str | None = None) -> str | bool | None:
    """Nonblocking generation claim; orphaned startup leases expire after 60s."""
    guard = _try_acquire_binary_file_lock(lane_root / ".maintenance-launch.lock")
    if guard is None:
        return None
    try:
        path = lane_root / ".maintenance-launch.json"
        try:
            current = json.loads(path.read_text(encoding="utf-8"))
            if not (
                isinstance(current, dict)
                and isinstance(current.get("generation"), str)
                and current["generation"]
                and type(current.get("expires_at")) in (int, float)
                and math.isfinite(current["expires_at"])
            ):
                raise ValueError("invalid maintenance startup lease")
        except FileNotFoundError:
            current = None
        except (ValueError, OSError):
            # A launcher can die during its guarded write. Never retain a
            # malformed permanent marker; reclaim only after the startup bound.
            if time.time() - path.stat().st_mtime < 60:
                return None
            current = None
        if release is not None:
            if current is not None and current.get("generation") == release:
                path.unlink()
                return True
            return False
        if _binary_file_lock_is_busy(lane_root / ".cargo-lane-trash-cleanup.lock"):
            return None
        if current is not None and float(current.get("expires_at", 0)) > time.time():
            return None
        generation = str(uuid.uuid4())
        path.write_text(json.dumps({"generation": generation, "launcher_pid": os.getpid(), "expires_at": time.time() + 60}), encoding="utf-8")
        return generation
    finally:
        _release_binary_file_lock(guard)
        guard.close()


def request_cargo_lane_maintenance(repo_root: Path, lane_root: Path) -> None:
    """Request the shared worker; only explicit administration waits for pruning."""
    if os.environ.get("CODEX_CARGO_LANE_MAINTENANCE_SYNC") == "1":
        maintain_cargo_lanes(repo_root, lane_root)
        return
    if os.environ.get("CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE") == "1":
        return
    try:
        interval = (
            max(0, int(os.environ.get("CODEX_CARGO_LANE_GC_INTERVAL_HOURS", "1")))
            * 3600
        )
    except ValueError:
        interval = 3600
    stamp, retry = lane_root / ".gc-stamp", lane_root / ".gc-retry"
    due = not stamp.exists() or time.time() - stamp.stat().st_mtime >= interval
    due &= not retry.exists() or time.time() - retry.stat().st_mtime >= 60
    if not due and not any(lane_root.glob("*.trash-*")):
        return
    shell = shutil.which("powershell") or shutil.which("pwsh")
    if shell is None:
        print("warning: Cargo maintenance worker requires PowerShell", file=sys.stderr)
        return
    worker = repo_root / "scripts" / "build" / "cargo-lane-trash-cleanup.ps1"
    if not worker.is_file():
        print(
            f"warning: Cargo maintenance worker is missing: {worker}", file=sys.stderr
        )
        return
    command = [
        shell,
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        str(repo_root / "scripts" / "build" / "cargo-lane-trash-cleanup.ps1"),
        "-LanesRoot",
        str(lane_root),
    ]
    if due:
        command.append("-Prune")
    generation = _maintenance_launch_lease(lane_root)
    if not isinstance(generation, str):
        return
    command.extend(["-LaunchToken", generation])
    # The worker logs its own failures after acquiring ownership. Opening its
    # log here conflicts with PowerShell's append handle on Windows.
    try:
        child = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            close_fds=True,
        )
    except OSError:
        _maintenance_launch_lease(lane_root, release=generation)
        raise
    # Reap asynchronously if this launcher stays alive; never await pruning.
    import threading

    def reap() -> None:
        child.wait()
        _maintenance_launch_lease(lane_root, release=generation)
    threading.Thread(target=reap, daemon=True).start()


def _read_lane_build_context(target_dir: Path) -> object:
    try:
        return json.loads(
            (target_dir / ".lane-build-context.json").read_text(encoding="utf-8")
        )
    except (OSError, ValueError):
        return None


def cargo_build_context(
    repo_root: Path, command: Sequence[str], env: Mapping[str, str]
) -> dict[str, object]:
    """Rank recorded build settings, never replace Cargo's freshness checks."""
    settings = {
        key: value
        for key, value in env.items()
        if key
        in {
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTUP_TOOLCHAIN",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_INCREMENTAL",
            "CARGO_BUILD_TARGET",
            "CARGO_HOME",
            "RUSTUP_HOME",
        }
        or key.startswith("CARGO_PROFILE_")
        or (key.startswith("CARGO_TARGET_") and key.endswith(("_LINKER", "_RUSTFLAGS")))
    }
    tools = {}
    for name in ("cargo", "rustc"):
        path = shutil.which(env.get(name.upper(), name), path=env.get("PATH"))
        if path:
            resolved = Path(path).resolve()
            try:
                observation = resolved.stat()
                tools[name] = [
                    str(resolved),
                    observation.st_size,
                    observation.st_mtime_ns,
                ]
            except OSError:
                tools[name] = [str(resolved)]
    configs = {}
    for name in (
        "rust-toolchain",
        "rust-toolchain.toml",
        "codex-rs/rust-toolchain.toml",
        ".cargo/config.toml",
        "codex-rs/.cargo/config.toml",
    ):
        path = repo_root / name
        if path.is_file():
            configs[name] = hashlib.sha256(path.read_bytes()).hexdigest()
    options = []
    config_overrides = []
    working_dir = Path.cwd()
    cargo_index = (
        _cargo_subcommand_index(command)
        if command and Path(command[0]).stem.lower() == "cargo"
        else None
    )
    nextest = cargo_index is not None and command[cargo_index] == "nextest"
    options.extend(_cargo_profile_options(command))
    tokens = iter(command)
    for token in tokens:
        if token == "--":
            break
        if token == "-C":
            working_dir /= next(tokens, "")
        elif token.startswith("-C"):
            working_dir /= token[2:]
        elif token == "--config":
            config_overrides.append(next(tokens, ""))
        elif token.startswith("--config="):
            config_overrides.append(token.split("=", 1)[1])
        elif nextest and token in ("--profile", "-P"):
            next(tokens, "")
        elif nextest and token.startswith("--profile="):
            continue
        elif token in ("--cargo-profile", "--profile"):
            next(tokens, "")
        elif token.startswith(("--cargo-profile=", "--profile=")):
            continue
        elif token in ("--target", "--features", "-F"):
            options.extend(["--features" if token == "-F" else token, next(tokens, "")])
        elif token.startswith(("--target=", "--features=")):
            options.extend(token.split("=", 1))
        elif token in (
            "--all-features",
            "--no-default-features",
        ) or token.startswith("+"):
            options.append(token)
    for value in config_overrides:
        options.extend(["--config", value])
        if "=" not in value:
            path = (working_dir / value).resolve()
            # A missing/unreadable file still fails in Cargo; never claim it
            # matches a previously recorded readable configuration.
            try:
                configs[str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
            except OSError:
                configs[str(path)] = None
    return {
        "version": 2,
        "sourceRoot": os.path.normcase(os.path.abspath(repo_root)),
        "environment": settings,
        "tools": tools,
        "configs": configs,
        "packages": sorted(cargo_package_specs(command)),
        "options": options,
    }


# Sealed snapshots share one warm lane through a content-synced mirror. Only
# content-changed files are rewritten, after the lane's last build ended, so
# Cargo's mtime freshness stays sound across snapshots whatever their mtimes.
SOURCE_MIRROR_MODES = ("auto", "always", "never")
SOURCE_MIRRORS_ROOT_ENV = "CODEX_CARGO_LANE_SOURCE_MIRRORS_ROOT"
SOURCE_MIRROR_SUBCOMMANDS = frozenset({"build", "b", "check", "c", "rustc", "clippy", "doc", "d"})
# Compiled inputs: codex-rs and its vendored path dependencies, plus root-level
# toolchain/configuration that rustup and Cargo discover from the build cwd.
SOURCE_TREE_DIRS = ("codex-rs", "third_party", ".cargo")
SOURCE_TREE_FILES = ("rust-toolchain", "rust-toolchain.toml")
SOURCE_TREE_SKIP_DIRS = frozenset({".git", "node_modules", "__pycache__"})
BUNDLE_RECEIPT_NAME = "build-receipt.json"
BUNDLE_RECEIPT_VERSION = 1
FREEZE_MESSAGE_FORMAT = "--message-format=json-render-diagnostics"


def source_mirrors_root(env: Mapping[str, str] = os.environ) -> Path:
    raw = env.get(SOURCE_MIRRORS_ROOT_ENV, "").strip()
    if raw:
        return Path(raw).expanduser().absolute()
    base = env.get("LOCALAPPDATA", "").strip()
    return ((Path(base) if base else Path.home() / ".cache") / "codex-kd" / "lane-sources").absolute()


def source_tree_files(root: Path) -> dict[str, Path]:
    """Compiled-input files below a source root, keyed by POSIX relative path.

    Cargo target directories (CACHEDIR.TAG, codex-rs/target) and VCS/package
    caches are skipped. Links are refused: their content is not root-owned.
    """
    files: dict[str, Path] = {}
    pending: list[tuple[Path, str]] = []
    for name in (*SOURCE_TREE_DIRS, *SOURCE_TREE_FILES):
        path = root / name
        observation = _lane_path_observation(path)
        if observation is None:
            continue
        if _observation_is_indirect(observation):
            raise ValueError(f"source tree contains a link or junction: {path}")
        if name in SOURCE_TREE_FILES and stat.S_ISREG(observation.st_mode):
            files[name] = path
        elif name in SOURCE_TREE_DIRS and stat.S_ISDIR(observation.st_mode):
            pending.append((path, f"{name}/"))
    while pending:
        directory, prefix = pending.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                relative = f"{prefix}{entry.name}"
                observation = entry.stat(follow_symlinks=False)
                if _observation_is_indirect(observation):
                    raise ValueError(f"source tree contains a link or junction: {entry.path}")
                if stat.S_ISDIR(observation.st_mode):
                    if not (
                        entry.name in SOURCE_TREE_SKIP_DIRS
                        or relative.casefold() == "codex-rs/target"
                        or os.path.isfile(os.path.join(entry.path, "CACHEDIR.TAG"))
                    ):
                        pending.append((Path(entry.path), f"{relative}/"))
                elif stat.S_ISREG(observation.st_mode):
                    files[relative] = Path(entry.path)
    return files


class _SourceManifest:
    """Order-independent digest of (relative path, content SHA-256) pairs."""

    def __init__(self) -> None:
        self.entries: dict[str, str] = {}
        self.bytes = 0

    def add(self, relative: str, data: bytes) -> None:
        self.entries[relative] = hashlib.sha256(data).hexdigest()
        self.bytes += len(data)

    def summary(self) -> dict[str, object]:
        digest = hashlib.sha256()
        for relative in sorted(self.entries):
            digest.update(f"{relative}\0{self.entries[relative]}\n".encode("utf-8"))
        return {"sha256": digest.hexdigest(), "files": len(self.entries), "bytes": self.bytes}


def _source_manifest(root: Path) -> _SourceManifest:
    manifest = _SourceManifest()
    for relative, path in source_tree_files(root).items():
        manifest.add(relative, path.read_bytes())
    return manifest


def source_tree_manifest(root: Path) -> dict[str, object]:
    return _source_manifest(root).summary()


def _manifest_sha256(entries: Mapping[str, str]) -> str:
    manifest = _SourceManifest()
    manifest.entries = dict(entries)
    return str(manifest.summary()["sha256"])


def _posix_parents(relative: str) -> Iterator[str]:
    while "/" in relative:
        relative = relative.rsplit("/", 1)[0]
        yield relative


def sync_source_mirror(
    source_root: Path, mirror_root: Path, *, not_before_ns: int = 0
) -> dict[str, object]:
    """Make the mirror's compiled inputs equal the source's by content.

    Unchanged files keep their mirror mtimes; changed files are rewritten after
    `not_before_ns` (the lane's last build end), so Cargo sees them as newer
    than every artifact compiled from earlier mirror content.
    """
    started = time.perf_counter()
    source = source_tree_files(source_root)
    mirror_root.mkdir(parents=True, exist_ok=True)
    existing = source_tree_files(mirror_root)
    removed = 0
    for relative in sorted(existing.keys() - source.keys()):
        existing[relative].unlink()
        removed += 1
    source_dirs = {parent for relative in source for parent in _posix_parents(relative)}
    stale_dirs = {parent for relative in existing for parent in _posix_parents(relative)}
    for directory in sorted(stale_dirs - source_dirs, key=len, reverse=True):
        try:
            (mirror_root / directory).rmdir()
        except OSError:
            pass
    manifest = _SourceManifest()
    floor_ns = not_before_ns + 1_000_000_000 if not_before_ns else 0
    written = 0
    for relative in sorted(source):
        data = source[relative].read_bytes()
        manifest.add(relative, data)
        target = mirror_root / relative
        try:
            if target.stat().st_size == len(data) and target.read_bytes() == data:
                continue
        except FileNotFoundError:
            pass
        target.parent.mkdir(parents=True, exist_ok=True)
        temporary = target.with_name(f".{target.name}.mirror-{os.getpid()}")
        temporary.write_bytes(data)
        os.replace(temporary, target)
        if time.time_ns() < floor_ns:
            os.utime(target, ns=(floor_ns, floor_ns))
        written += 1
    return {
        "root": str(mirror_root),
        **manifest.summary(),
        "written": written,
        "removed": removed,
        "syncMs": round((time.perf_counter() - started) * 1000, 1),
        "entries": manifest.entries,
    }


def _cargo_configs_from(directory: Path) -> list[tuple[str, str]]:
    """Cargo/rustup discovery files in a directory and its ancestors, by content."""
    directory = Path(os.path.abspath(directory))
    found = []
    for current in (directory, *directory.parents):
        for name in (".cargo/config.toml", ".cargo/config", *SOURCE_TREE_FILES):
            path = current / name
            if path.is_file():
                found.append((name, hashlib.sha256(path.read_bytes()).hexdigest()))
    return found


def _source_mirror_ineligibility(command: Sequence[str], repo_root: Path) -> str | None:
    """Why a command cannot build from a relocated copy of its source root."""
    if not command or Path(command[0]).stem.lower() != "cargo":
        return "not a direct Cargo command"
    index = _cargo_subcommand_index(command)
    if index is None or command[index] not in SOURCE_MIRROR_SUBCOMMANDS:
        return "not a Cargo build/check command"
    root = source_root_identity(repo_root.resolve())
    tokens = iter(command[1:])
    for token in tokens:
        if token in ("--manifest-path", "-C", "-Z") or token.startswith(
            ("--manifest-path=", "-C", "-Z", "--target-dir")
        ):
            return f"explicit Cargo path option {token.split('=', 1)[0]}"
        if token == "--config" or token.startswith("--config="):
            value = token.split("=", 1)[1] if "=" in token else next(tokens, "")
            if "=" not in value:
                return "Cargo --config file"
        if root in os.path.normcase(token):
            return "argument names the source root"
    cwd = source_root_identity(Path.cwd().resolve())
    if cwd != root and not cwd.startswith(root.rstrip(os.sep) + os.sep):
        return "working directory is outside the source root"
    return None


def _is_warm_lane_dir(path: Path) -> bool:
    # Profile names are configurable. Inspect only immediate, direct profile
    # directories; their names alone do not prove reusable work.
    return (
        path.is_dir()
        and not is_indirect_directory(path)
        and any(
            profile.is_dir() and not is_indirect_directory(profile) and is_cargo_artifact_dir(profile)
            for profile in path.iterdir()
        )
    )


def _source_mirror_skip_reason(
    mode: str,
    *,
    command: Sequence[str],
    repo_root: Path,
    external: bool,
    mirrors_root: Path,
    source_lane: Path | None,
    freeze_requested: bool = False,
) -> str | None:
    """None when the build should use the shared mirror lanes, else why not."""
    if mode == "never":
        return "disabled by --source-mirror never"
    reason = _source_mirror_ineligibility(command, repo_root)
    # Explicit bundles need a stable build source even in a live checkout.
    # Reuse the existing mirror; ordinary in-place builds keep their warm lane.
    if reason is None and mode == "auto" and not freeze_requested:
        if not external:
            reason = "owning checkout builds in place"
        elif (repo_root / ".git").exists():
            reason = "checkout with .git builds in place"
        elif source_lane is not None and _is_warm_lane_dir(source_lane):
            reason = "existing warm lane for this source root"
    # Mirrors are created below mirrors_root; the source's own root is copied.
    if reason is None and _cargo_configs_from(repo_root.parent) != _cargo_configs_from(mirrors_root):
        reason = "Cargo/rustup files above the source root differ from the mirror's"
    if reason is not None and mode == "always":
        raise ValueError(f"--source-mirror always: {reason}")
    return reason


def _mirror_state_path(mirrors_root: Path, target_dir: Path) -> Path:
    key = hashlib.sha256(source_root_identity(target_dir).encode("utf-8")).hexdigest()[:16]
    return mirrors_root / f"{key}.json"


def _mirror_slot_distance(mirrors_root: Path, wanted: Mapping[str, str]) -> Callable[[Path], int]:
    """Files whose content differs between a mirror slot's last sync and `wanted`.

    Picking the nearest free slot keeps alternating arms in their own warm slot.
    Slots without recorded entries rank last.
    """
    cache: dict[str, tuple[int, int]] = {}

    def distance(lane_dir: Path) -> int:
        state_path = _mirror_state_path(mirrors_root, lane_dir.resolve())
        try:
            stamp = state_path.stat().st_mtime_ns
        except OSError:
            return len(wanted) + 1
        cached = cache.get(str(state_path))
        if cached is not None and cached[0] == stamp:
            return cached[1]
        try:
            entries = json.loads(state_path.read_text(encoding="utf-8")).get("entries")
        except (OSError, ValueError, AttributeError):
            entries = None
        value = (
            sum(1 for key in wanted.keys() | entries.keys() if wanted.get(key) != entries.get(key))
            if isinstance(entries, dict)
            else len(wanted) + 1
        )
        cache[str(state_path)] = (stamp, value)
        return value

    return distance


def _cargo_command_with_message_format(command: Sequence[str]) -> list[str]:
    result = list(command)
    options = result[: result.index("--")] if "--" in result else result
    if any(token == "--message-format" or token.startswith("--message-format=") for token in options):
        raise ValueError("--freeze-bundle sets Cargo's --message-format; remove it from the command")
    index = _cargo_subcommand_index(result) if result and Path(result[0]).stem.lower() == "cargo" else None
    if index is None:
        raise ValueError("--freeze-bundle requires a direct Cargo build command")
    return [*result[: index + 1], FREEZE_MESSAGE_FORMAT, *result[index + 1 :]]


def _cargo_executables(messages: Path) -> list[Path]:
    executables: dict[str, Path] = {}
    with messages.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            if not line.startswith("{"):
                continue
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("reason") == "compiler-artifact" and message.get("executable"):
                path = Path(message["executable"])
                executables.setdefault(source_root_identity(path), path)
    return list(executables.values())


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def freeze_bundle(bundle_dir: Path, executables: Sequence[Path]) -> list[dict[str, object]]:
    """Copy built executables into a new read-only bundle while the lane is held."""
    bundle_dir.mkdir(parents=True, exist_ok=False)
    frozen = []
    for source in executables:
        destination = bundle_dir / source.name
        if destination.exists():
            raise ValueError(f"bundle artifact name collision: {source.name}")
        shutil.copyfile(source, destination)
        frozen.append({
            "name": source.name,
            "sha256": _file_sha256(destination),
            "bytes": destination.stat().st_size,
            "cargoPath": str(source),
        })
        os.chmod(destination, stat.S_IREAD)
    return frozen


def bundle_store_root(env: Mapping[str, str] = os.environ) -> Path:
    """Default home of frozen bundles for builds without --freeze-bundle."""
    return source_mirrors_root(env).parent / "bundles"


def _prune_bundle_store(root: Path, *, max_age_days: float) -> None:
    """Drop store bundles older than the lane age limit (never explicit bundles)."""
    cutoff = time.time() - max_age_days * 86400
    try:
        entries = list(os.scandir(root))
    except OSError:
        return
    for entry in entries:
        try:
            if not entry.is_dir(follow_symlinks=False) or entry.stat().st_mtime >= cutoff:
                continue
            for child in Path(entry.path).iterdir():
                os.chmod(child, stat.S_IWRITE | stat.S_IREAD)
            shutil.rmtree(entry.path)
        except OSError:
            continue


def _write_receipt(bundle_dir: Path, receipt: Mapping[str, object]) -> None:
    receipt_path = bundle_dir / BUNDLE_RECEIPT_NAME
    receipt_path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    os.chmod(receipt_path, stat.S_IREAD)
    try:
        index_bundle(bundle_dir, _reuse_key(receipt))
    except OSError as error:
        print(f"warning: could not index frozen bundle: {error}", file=sys.stderr)


def _reuse_bundle(
    source: Path, receipt: Mapping[str, object], destination: Path | None
) -> tuple[Path, list[dict[str, object]]]:
    """Hand out a verified bundle; copy it when the caller named a new directory."""
    if destination is None:
        return source, list(receipt["binaries"])
    copies = freeze_bundle(destination, [source / entry["name"] for entry in receipt["binaries"]])
    if [(entry["name"], entry["sha256"]) for entry in copies] != [
        (entry["name"], entry["sha256"]) for entry in receipt["binaries"]
    ]:
        raise RuntimeError(f"copied bundle hashes differ from {source}")
    _write_receipt(destination, {
        **receipt,
        "createdUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "binaries": copies,
        "reusedFrom": str(source),
    })
    return destination, copies


def _rustc_identity(env: Mapping[str, str], cwd: Path) -> str | None:
    """`rustc -vV` as rustup resolves it from the build cwd (toolchain files)."""
    try:
        completed = subprocess.run(
            [env.get("RUSTC", "rustc"), "-vV"], cwd=cwd, env=dict(env),
            capture_output=True, text=True, timeout=60, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    return completed.stdout.strip() if completed.returncode == 0 else None


def bundle_index_path(env: Mapping[str, str] = os.environ) -> Path:
    return source_mirrors_root(env).parent / "bundle-index.jsonl"


def _reuse_key(receipt: Mapping[str, object]) -> str:
    context = dict(receipt.get("buildContext") or {})
    context.pop("sourceRoot", None)
    manifest = receipt.get("sourceManifest") or {}
    key = [
        receipt.get("command"),
        manifest.get("sha256") if isinstance(manifest, dict) else None,
        context,
        receipt.get("rustc"),
    ]
    return hashlib.sha256(json.dumps(key, sort_keys=True).encode("utf-8")).hexdigest()


def index_bundle(bundle_dir: Path, key: str) -> None:
    """Append one line; lookups verify the receipt and hashes, never trust this."""
    path = bundle_index_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps({"bundle": str(bundle_dir), "key": key}) + "\n")


def _verified_receipt(bundle: Path, key: str) -> Mapping[str, object] | None:
    try:
        receipt = json.loads((bundle / BUNDLE_RECEIPT_NAME).read_text(encoding="utf-8"))
        if (
            receipt.get("version") == BUNDLE_RECEIPT_VERSION
            and receipt.get("exitCode") == 0
            and receipt.get("binaries")
            and _reuse_key(receipt) == key
            and all(
                _file_sha256(bundle / entry["name"]) == entry["sha256"]
                for entry in receipt["binaries"]
            )
        ):
            return receipt
    except (OSError, ValueError, KeyError, TypeError, AttributeError):
        pass
    return None


def _newest_indexed_bundle(key: str) -> tuple[Path, Mapping[str, object]] | None:
    try:
        lines = bundle_index_path().read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return None
    candidates: dict[str, Path] = {}
    for line in lines:
        try:
            entry = json.loads(line)
            if entry.get("key") == key:
                candidates[source_root_identity(Path(entry["bundle"]))] = Path(entry["bundle"])
        except (ValueError, KeyError, TypeError, AttributeError):
            continue
    matches = [
        (str(receipt.get("createdUtc", "")), bundle, receipt)
        for bundle in candidates.values()
        if (receipt := _verified_receipt(bundle, key)) is not None
    ]
    if not matches:
        return None
    _, bundle, receipt = max(matches, key=lambda match: (match[0], str(match[1])))
    return bundle, receipt


def _cargo_tail(command: object) -> list[str] | None:
    """The Cargo invocation inside a recorded (possibly wrapped) argv list."""
    if not isinstance(command, list) or not all(isinstance(token, str) for token in command):
        return None
    for index, token in enumerate(command):
        if Path(token).stem.lower() == "cargo" and index + 1 < len(command):
            return command[index:]
    return None


def _legacy_entries(provenance: Mapping[str, object]) -> dict[str, str] | None:
    """Per-file SHA-256 map from pre-receipt provenance (inline or manifest file)."""
    manifest: object = provenance.get("source_snapshot")
    if not isinstance(manifest, dict):
        manifest = provenance.get("source_manifest")
    if isinstance(manifest, str) and manifest.lower().endswith(".json"):
        try:
            manifest = json.loads(Path(manifest).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return None
    if isinstance(manifest, dict) and isinstance(manifest.get("files"), list):
        manifest = {
            item.get("path"): item.get("sha256")
            for item in manifest["files"]
            if isinstance(item, dict)
        }
    if not isinstance(manifest, dict) or not manifest:
        return None
    if not all(
        isinstance(path, str) and isinstance(digest, str) and len(digest) == 64
        for path, digest in manifest.items()
    ):
        return None
    return dict(manifest)


def find_legacy_bundles(
    *, repo_root: Path, command: Sequence[str], search_roots: Sequence[Path]
) -> list[tuple[Path, Path, str]]:
    """Pre-receipt bundles whose recorded source files, Cargo command and binary
    hashes match. Build settings and Cargo's actual source consumption were not
    receipt-bound, so callers must treat these as candidates, not exact hits.
    Returns (bundle directory, provenance file, caveat), newest first.
    """
    entries = _source_manifest(repo_root).entries
    wanted_sha = _manifest_sha256(entries)
    env = os.environ.copy()
    env.update(local_rust_env(env, repo_root=repo_root, which=shutil.which))
    rustc = (_rustc_identity(env, Path.cwd()) or "").splitlines()[:1]
    found: list[tuple[float, Path, Path, str]] = []
    for search_root in search_roots:
        for directory, names, files in os.walk(search_root):
            names[:] = [
                name for name in names
                if name not in SOURCE_TREE_SKIP_DIRS and name not in SOURCE_TREE_DIRS
                and name != "target"
                and not os.path.isfile(os.path.join(directory, name, "CACHEDIR.TAG"))
            ]
            for name in files:
                if not name.lower().endswith(".json") or "provenance" not in name.lower():
                    continue
                path = Path(directory) / name
                try:
                    provenance = json.loads(path.read_text(encoding="utf-8"))
                    binaries = provenance["binaries"]
                    items = list(binaries.values()) if isinstance(binaries, dict) else binaries
                    paths = [Path(item["path"]) for item in items]
                    if (
                        not paths
                        or provenance.get("exit_code", 0) != 0
                        or _cargo_tail(provenance.get("command")) != list(command)
                        or (legacy := _legacy_entries(provenance)) is None
                        or _manifest_sha256(legacy) != wanted_sha
                    ):
                        continue
                    toolchain = provenance.get("toolchain")
                    recorded = (
                        str(toolchain.get("rustc", "")).splitlines()[:1]
                        if isinstance(toolchain, dict)
                        else []
                    )
                    if recorded and rustc and recorded != rustc:
                        continue
                    if any(_file_sha256(binary) != item["sha256"] for binary, item in zip(paths, items)):
                        continue
                except (OSError, ValueError, KeyError, TypeError, AttributeError):
                    continue
                bundle = paths[0].parent
                if any(binary.parent != bundle for binary in paths):
                    continue
                qualified = "-ws-" in json.dumps(provenance)
                caveat = (
                    "legacy bundle: recorded source files, command and binary hashes match; "
                    "build settings are not receipt-bound"
                    + ("" if recorded else " and rustc is unrecorded")
                    + (
                        "; built in a source-qualified lane"
                        if qualified
                        else "; no source-qualified lane recorded, so Cargo may have reused "
                        "another snapshot's artifacts (BC1)"
                    )
                )
                found.append(((qualified, path.stat().st_mtime), bundle, path, caveat))
    # Source-qualified lane builds first: older shared-lane builds carry BC1 risk.
    found.sort(key=lambda item: item[0], reverse=True)
    return [(bundle, path, caveat) for _, bundle, path, caveat in found]


def find_reusable_bundle(
    *, repo_root: Path, command: Sequence[str], search_roots: Sequence[Path] = ()
) -> tuple[Path | None, int]:
    """Newest frozen bundle built from identical compiled inputs and settings.

    Matches the command, source manifest, recorded build settings and resolved
    rustc; every listed binary must still exist with its recorded SHA-256.
    Candidates come from the bundle index; search roots add unindexed receipts.
    """
    env = os.environ.copy()
    env.update(local_rust_env(env, repo_root=repo_root, which=shutil.which))
    wanted = _reuse_key({
        "command": list(command),
        "sourceManifest": source_tree_manifest(repo_root),
        "buildContext": cargo_build_context(repo_root, command, env),
        "rustc": _rustc_identity(env, Path.cwd()),
    })
    indexed: set[str] = set()
    try:
        for line in bundle_index_path().read_text(encoding="utf-8").splitlines():
            try:
                indexed.add(source_root_identity(Path(json.loads(line)["bundle"])))
            except (ValueError, KeyError, TypeError):
                continue
    except FileNotFoundError:
        pass
    scanned = len(indexed)
    for search_root in search_roots:
        for directory, names, files in os.walk(search_root):
            names[:] = [
                name for name in names
                if name not in SOURCE_TREE_SKIP_DIRS and name not in SOURCE_TREE_DIRS
                and name != "target"
                and not os.path.isfile(os.path.join(directory, name, "CACHEDIR.TAG"))
            ]
            bundle = Path(directory)
            if BUNDLE_RECEIPT_NAME not in files or source_root_identity(bundle) in indexed:
                continue
            scanned += 1
            try:
                receipt = json.loads((bundle / BUNDLE_RECEIPT_NAME).read_text(encoding="utf-8"))
                index_bundle(bundle, _reuse_key(receipt))
            except (OSError, ValueError, TypeError, AttributeError):
                continue
            indexed.add(source_root_identity(bundle))
    newest = _newest_indexed_bundle(wanted)
    return (newest[0] if newest is not None else None), scanned


def run_in_cargo_lane(
    *,
    repo_root: Path,
    requested_lane: str,
    command: Sequence[str],
    lane_root: Path | None = None,
    lock_timeout_seconds: float = LANE_COORDINATION_GRACE_SECONDS,
    timing_path: Path | None = None,
    warm_wait_seconds: float = 0.0,
    allow_cold_overflow: bool = False,
    max_active_lanes: int | None = None,
    source_mirror: str = "auto",
    freeze_bundle_dir: Path | None = None,
    rebuild: bool = False,
) -> int:
    """Reuse a compatible warm build directory and run the command once.

    Sealed external snapshots (no .git) reuse an exact frozen bundle when one
    exists; otherwise they build in the nearest free content-synced mirror slot
    and freeze their executables (into --freeze-bundle or the bundle store).
    """
    if not command:
        raise ValueError("run-lane requires a command after --")
    if source_mirror not in SOURCE_MIRROR_MODES:
        raise ValueError(f"unknown source mirror mode {source_mirror!r}")
    if freeze_bundle_dir is not None:
        freeze_bundle_dir = freeze_bundle_dir.absolute()
        if freeze_bundle_dir.exists():
            raise ValueError(f"--freeze-bundle must name a new directory: {freeze_bundle_dir}")
    if timing_path is not None:
        timing_path.parent.mkdir(parents=True, exist_ok=True)
    stream = timing_path.open("x", encoding="utf-8") if timing_path else None
    started = time.perf_counter()
    record = {"command": list(command), "requestedLane": requested_lane,
              "exitCode": None, "cargoExitCode": None, "status": "running"}
    try:
        if not repo_root.is_dir():
            # The lane is named for this root; a mistyped root would silently
            # share one mislabeled lane between different sources.
            raise ValueError(f"--repo-root is not a directory: {repo_root}")
        _guard_cargo_checkout_cwd(command, repo_root)
        owner_root = repo_root
        external = source_root_identity(repo_root.resolve()) != source_root_identity(REPO_ROOT)
        if external:
            # Snapshot lanes and maintenance stay with the owning checkout:
            # short target paths and nothing written into the sealed tree.
            owner_root = REPO_ROOT
            if lane_root is None:
                lane_root = cargo_lanes_root(REPO_ROOT)
        child_env = os.environ.copy()
        child_env.update(local_rust_env(child_env, repo_root=repo_root, which=shutil.which))
        child_env.setdefault("RUST_MIN_STACK", RUST_MIN_STACK_BYTES)
        build_context = cargo_build_context(repo_root, command, child_env)
        mirrors_root = source_mirrors_root()
        skipped = _source_mirror_skip_reason(
            source_mirror, command=command, repo_root=repo_root, external=external,
            mirrors_root=mirrors_root,
            freeze_requested=freeze_bundle_dir is not None,
            source_lane=(
                Path(lane_root).absolute()
                / source_lane_name(
                    requested_lane if requested_lane != "auto" else _auto_lane_base(command),
                    source_root_identity(repo_root),
                )
                if lane_root is not None
                else None
            ),
        )
        # Snapshot lanes are shared or rebuilt later: only executables copied
        # while the lane is held stay bound to this source, so snapshots freeze.
        freeze = (
            freeze_bundle_dir is not None
            or skipped is None
            or (external and not (repo_root / ".git").exists())
        )
        source = _source_manifest(repo_root) if freeze else None
        rustc = _rustc_identity(child_env, Path.cwd()) if freeze else None
        if source is not None and not rebuild:
            reuse = _newest_indexed_bundle(_reuse_key({
                "command": list(command),
                "sourceManifest": source.summary(),
                "buildContext": build_context,
                "rustc": rustc,
            }))
            if reuse is not None:
                bundle_dir, binaries = _reuse_bundle(reuse[0], reuse[1], freeze_bundle_dir)
                record.update(
                    exitCode=0, status="complete", reusedFrom=str(reuse[0]),
                    bundle={"dir": str(bundle_dir), "binaries": binaries},
                )
                print(
                    f"Reused frozen bundle {bundle_dir}: exact source/settings match, "
                    "no build (--rebuild builds anyway)",
                    file=sys.stderr,
                )
                return 0
        source_identity = source_root_identity(repo_root)
        rank = None
        if skipped is None:
            source_identity = f"mirror:{source_root_identity(mirrors_root)}"
            build_context["sourceRoot"] = source_identity
            assert source is not None
            rank = _mirror_slot_distance(mirrors_root, source.entries)
            # A busy slot never blocks while the family has room: the next slot
            # starts cold once, then concurrent snapshot builds stay warm.
            allow_cold_overflow = True
            max_active_lanes = max_active_lanes or max_active_lanes_per_base()
        else:
            record["sourceMirrorSkipped"] = skipped
        with reserve_cargo_lane(
            repo_root=owner_root, requested_lane=requested_lane, command=command,
            lane_root=lane_root, lock_timeout_seconds=lock_timeout_seconds,
            warm_wait_seconds=warm_wait_seconds, allow_cold_overflow=allow_cold_overflow,
            build_context=build_context, max_active_lanes=max_active_lanes,
            source_identity=source_identity, rank=rank,
        ) as (resolved_lane, target_dir):
            record.update(resolvedLane=resolved_lane, targetDir=str(target_dir))
            record["sourceRoot"] = source_root_identity(repo_root)
            print(f"Cargo lane {resolved_lane}: {target_dir}", file=sys.stderr)
            build_root, child_cwd, state_path = repo_root, None, None
            if skipped is None:
                state_path = _mirror_state_path(mirrors_root, target_dir)
                build_root = state_path.with_suffix("")
                try:
                    state = json.loads(state_path.read_text(encoding="utf-8"))
                except (OSError, ValueError):
                    state = {}
                not_before_ns = state.get("lastBuildEndedNs", 0) if isinstance(state, dict) else 0
                mirror = sync_source_mirror(
                    repo_root, build_root,
                    not_before_ns=not_before_ns if isinstance(not_before_ns, int) else 0,
                )
                mirror_entries = mirror.pop("entries")
                record["sourceMirror"] = mirror
                child_cwd = build_root / os.path.relpath(Path.cwd().resolve(), repo_root.resolve())
                print(
                    f"Source mirror {build_root}: rewrote {mirror['written']} of "
                    f"{mirror['files']} files, removed {mirror['removed']} "
                    f"({mirror['syncMs']} ms)",
                    file=sys.stderr,
                )
            elif source_mirror != "never" and external:
                print(f"Source mirror skipped: {skipped}", file=sys.stderr)
            # An exported target directory fragments compiler-cache keys.
            child_env.pop("CARGO_TARGET_DIR", None)
            child_env.pop("CODEX_CARGO_LANE_TARGET_DIR", None)
            child_env.pop("CODEX_CARGO_LANE_OWNER_PID", None)
            child_env["CODEX_CARGO_LANE_OWNER_PID"] = str(os.getpid())
            child_env["CODEX_CARGO_LANE_TARGET_DIR"] = str(target_dir)
            child_command = _cargo_command_with_target_dir(command, target_dir)
            if not Path(child_command[0]).parent.name:
                program = shutil.which(child_command[0], path=child_env.get("PATH"))
                if program is not None:
                    child_command[0] = program
            try:
                request_cargo_lane_maintenance(owner_root, target_dir.parent)
            except OSError as error:
                print(f"warning: could not request Cargo maintenance: {error}", file=sys.stderr)
            messages_path = None
            if freeze:
                child_command = _cargo_command_with_message_format(child_command)
                messages_path = target_dir / f".cargo-messages-{uuid.uuid4().hex}.jsonl"
            try:
                with (
                    messages_path.open("x", encoding="utf-8")
                    if messages_path is not None
                    else open(os.devnull, "w", encoding="utf-8")
                ) as messages:
                    code = run_owned(
                        child_command, env=child_env, cwd=child_cwd,
                        creationflags=getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0),
                        prepare_sccache=True, check=False,
                        stdout=messages if messages_path is not None else sys.stdout,
                        stderr=sys.stderr,
                    ).returncode
                record["cargoExitCode"] = code
                record["exitCode"] = code
                if code == 0:
                    try:
                        (target_dir / ".lane-build-context.json").write_text(
                            json.dumps(build_context, indent=2) + "\n", encoding="utf-8"
                        )
                    except OSError as error:
                        print(f"warning: could not record lane build context: {error}", file=sys.stderr)
                if code == 0 and messages_path is not None:
                    assert source is not None
                    if skipped is None:
                        manifest = {key: mirror[key] for key in ("sha256", "files", "bytes")}
                    else:
                        manifest = source_tree_manifest(repo_root)
                        if manifest != source.summary():
                            raise RuntimeError(
                                "source changed during the build; no bundle was frozen. "
                                "Coordinate source writes, or use an eligible --source-mirror always build."
                            )
                    executables = _cargo_executables(messages_path)
                    bundle_dir = freeze_bundle_dir
                    if bundle_dir is None and executables:
                        store = bundle_store_root()
                        try:
                            max_age = float(os.environ.get("CODEX_CARGO_LANE_MAX_AGE_DAYS", "7"))
                        except ValueError:
                            max_age = 7.0
                        _prune_bundle_store(store, max_age_days=max(1.0, max_age))
                        bundle_dir = store / f"{manifest['sha256'][:16]}-{uuid.uuid4().hex[:8]}"
                    if bundle_dir is not None:
                        binaries = freeze_bundle(bundle_dir, executables)
                        _write_receipt(bundle_dir, {
                            "version": BUNDLE_RECEIPT_VERSION,
                            "createdUtc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                            "command": list(command),
                            "exitCode": code,
                            "sourceRoot": str(repo_root),
                            "buildRoot": str(build_root),
                            "sourceManifest": manifest,
                            "buildContext": build_context,
                            "rustc": rustc,
                            "resolvedLane": resolved_lane,
                            "targetDir": str(target_dir),
                            "binaries": binaries,
                        })
                        record["bundle"] = {"dir": str(bundle_dir), "binaries": binaries}
                        print(
                            f"Frozen bundle {bundle_dir}: {len(binaries)} executables"
                            + (f"; use it, not the shared {target_dir}" if skipped is None else ""),
                            file=sys.stderr,
                        )
            except CleanupFailed:
                (target_dir / ".lane-cleanup-unconfirmed").write_text(
                    "Process cleanup was not confirmed; inspect descendants before removing this quarantine.\n"
                )
                raise
            finally:
                if messages_path is not None:
                    messages_path.unlink(missing_ok=True)
                if state_path is not None:
                    # Later rewrites must postdate this build's compilations.
                    # Entries let the next snapshot pick its nearest slot.
                    state_path.write_text(json.dumps({
                        "targetDir": str(target_dir),
                        "lastBuildEndedNs": time.time_ns(),
                        "manifestSha256": mirror["sha256"],
                        "sourceRoot": str(repo_root),
                        "entries": mirror_entries,
                    }) + "\n", encoding="utf-8")
            record["status"] = "complete" if code == 0 else "failed"
            return code
    except (CargoLanesRootValidationError, OSError, RuntimeError, ValueError) as error:
        # Match main's runner-failure status, retaining the child result separately.
        # Cargo success alone does not prove freezing/provenance/cleanup succeeded.
        record.update(exitCode=2, status="failed", error=str(error))
        raise
    finally:
        if stream is not None:
            record["totalMs"] = (time.perf_counter() - started) * 1000
            try:
                with stream:
                    json.dump(record, stream, indent=2)
                    stream.write("\n")
            except OSError as error:
                print(f"warning: Cargo lane timing could not be saved: {error}", file=sys.stderr)


def is_protected_target_dir_name(name: str) -> bool:
    return name in PROTECTED_TARGET_DIR_NAMES or any(
        name.startswith(prefix) for prefix in PROTECTED_TARGET_DIR_PREFIXES
    )


def is_cargo_artifact_dir(path: Path) -> bool:
    return (
        (path / ".fingerprint").is_dir()
        and (path / "deps").is_dir()
        and ((path / "build").is_dir() or (path / "incremental").is_dir())
    )


def is_stray_cargo_target_dir(path: Path) -> bool:
    if is_protected_target_dir_name(path.name):
        return False
    if is_cargo_artifact_dir(path):
        return True
    return any(
        is_cargo_artifact_dir(path / profile)
        for profile in ("debug", "release", "dev-small")
    )


def stray_cargo_target_dirs(*, repo_root: Path = REPO_ROOT) -> list[Path]:
    target_root = repo_root / "codex-rs" / "target"
    if not target_root.is_dir():
        return []
    return sorted(
        path
        for path in target_root.iterdir()
        if path.is_dir()
        and not path.is_symlink()
        and not is_windows_junction(path)
        and is_stray_cargo_target_dir(path)
    )


def lane_last_used_mtime(path: Path) -> float:
    stamp = path / LANE_LAST_USED_STAMP
    try:
        if stamp.is_file():
            return stamp.stat().st_mtime
    except OSError:
        pass
    # Without the marker, the directory's own NTFS mtime only reflects
    # immediate-child churn, never rebuilds under debug/deps/... — so a
    # frequently-used markerless lane would look creation-era old. Take the
    # newest of the dir itself, cargo's per-build .rustc_info.json, and the
    # immediate children (profile dirs are recreated/renamed by builds).
    candidates = [path]
    candidates.extend(
        path / name for name in (".rustc_info.json", ".cargo-lock", "CACHEDIR.TAG")
    )
    try:
        candidates.extend(path.iterdir())
    except OSError:
        pass
    newest = 0.0
    for candidate in candidates:
        try:
            newest = max(newest, candidate.stat().st_mtime)
        except OSError:
            continue
    # A concurrent GC may remove a lane after the directory snapshot was
    # collected. Treat a vanished lane as oldest instead of failing diagnostics.
    return newest


def existing_lane_dirs(lane_root: Path) -> list[Path]:
    if not lane_root.exists():
        return []
    # Junctions are not symlinks to Path.is_symlink(); pruning through one
    # would delete its target (possibly another, active lane) or abort on the
    # containment check when it points outside the root.
    # Trash trees are already detached from their lane name; treating them as
    # lanes misreports them and makes each prune rename them again.
    return sorted(
        path
        for path in lane_root.iterdir()
        if path.is_dir()
        and LANE_TRASH_RE.search(path.name) is None
        and not is_indirect_directory(path)
    )


def lane_trash_dirs(lane_root: Path) -> list[Path]:
    try:
        return sorted(
            path
            for path in lane_root.iterdir()
            if LANE_TRASH_RE.search(path.name) is not None and path.is_dir()
        )
    except OSError:
        return []


def is_windows_junction(path: Path) -> bool:
    junction_probe = getattr(path, "is_junction", None)
    if callable(junction_probe):
        try:
            return bool(junction_probe())
        except OSError:
            return False
    try:
        return path.is_dir() and bool(
            os.lstat(path).st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT
        )
    except (OSError, AttributeError):
        return False


def is_indirect_directory(path: Path) -> bool:
    return path.is_symlink() or is_windows_junction(path)


def active_lane_names(processes: Sequence[RustProcess]) -> set[str]:
    return {
        lane
        for process in processes
        if (lane := lane_name_for_process(process)) is not None
    }


def env_active_lane_names(env: Mapping[str, str] = os.environ) -> set[str]:
    raw = env.get("CODEX_CARGO_LANE_ACTIVE_NAMES", "")
    active = set()
    for chunk in raw.replace(",", ";").split(";"):
        name = chunk.strip()
        if name:
            active.add(name)
    return active


def stale_lane_dirs(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
) -> list[Path]:
    return BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    ).stale_lanes


def is_timestamped_lane(name: str) -> bool:
    return TIMESTAMPED_LANE_RE.match(name) is not None


def warm_lane_base(name: str) -> str:
    if match := TIMESTAMPED_LANE_RE.match(name):
        return match.group("base")
    if match := LANE_SUFFIX_RE.match(name):
        return match.group("base")
    return name


def warm_lane_base_map(names: set[str]) -> dict[str, str]:
    return {name: warm_lane_base(name) for name in names}


def warm_lane_rank(name: str) -> int:
    if match := LANE_SUFFIX_RE.match(name):
        try:
            return int(match.group("suffix"))
        except ValueError:
            return 0
    return 0


def protected_warm_lane_names(
    lane_dirs: Sequence[Path],
    *,
    keep_warm_per_base: int,
    lane_mtime: Callable[[Path], float] | None = None,
) -> set[str]:
    if keep_warm_per_base <= 0:
        return set()

    lane_mtime = lane_last_used_mtime if lane_mtime is None else lane_mtime
    base_by_name = warm_lane_base_map({path.name for path in lane_dirs})
    grouped: dict[str, list[Path]] = {}
    for path in lane_dirs:
        if is_timestamped_lane(path.name):
            continue
        # Fold base keys so differently-cased invocations of the same lane
        # base group together on the case-insensitive filesystem.
        grouped.setdefault(base_by_name[path.name].casefold(), []).append(path)

    protected: set[str] = set()
    for lanes in grouped.values():
        ranked = sorted(
            lanes,
            key=lambda path: (
                -lane_mtime(path),
                warm_lane_rank(path.name),
                path.name.casefold(),
                path.name,
            ),
        )
        protected.update(path.name for path in ranked[:keep_warm_per_base])
    return protected


def prunable_lane_dirs(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> list[Path]:
    lane_root = validate_cargo_lanes_root(repo_root)
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
        lane_mtime=lane_mtime,
    )
    # Every prune, plan and report selects through here. Without process
    # evidence, "no activity seen" is a guess and must not authorize deletion.
    if snapshot.process_scan_error is not None:
        raise RustProcessScanError(
            "refusing to select Cargo lanes for pruning; lane activity is "
            f"unverified: {snapshot.process_scan_error}"
        )
    lane_dirs = snapshot.stale_lanes
    for path in lane_dirs:
        if path not in snapshot._lane_generations:
            snapshot._lane_generations[path] = _lane_prune_generation(path)
    protected = protected_warm_lane_names(
        lane_dirs,
        keep_warm_per_base=keep_warm_per_base,
        lane_mtime=snapshot.lane_mtime,
    )
    now = time.time() if now_timestamp is None else now_timestamp
    prunable: set[Path] = set()
    size_candidates: list[Path] = []
    effective_max_total_lane_bytes = max_total_lane_bytes
    if max_total_target_bytes is not None:
        non_lane_size_bytes, _errors = snapshot.non_lane_size(
            lane_root, size_workers=size_workers
        )
        target_lane_budget = max(0, max_total_target_bytes - non_lane_size_bytes)
        effective_max_total_lane_bytes = (
            target_lane_budget
            if effective_max_total_lane_bytes is None
            else min(effective_max_total_lane_bytes, target_lane_budget)
        )

    for path in lane_dirs:
        if is_timestamped_lane(path.name):
            prunable.add(path)
            continue
        if (
            max_age_days is not None
            and now - snapshot.lane_mtime(path) > max_age_days * 86400
        ):
            prunable.add(path)
            continue
        if keep_warm_per_base > 0 and path.name not in protected:
            prunable.add(path)
            continue
        if max_lane_bytes is not None:
            size_candidates.append(path)
            continue
        if (
            keep_warm_per_base <= 0
            and max_age_days is None
            and max_lane_bytes is None
            and effective_max_total_lane_bytes is None
        ):
            prunable.add(path)

    if max_lane_bytes is not None and size_candidates:
        for path, (size_bytes, _errors) in snapshot.lane_sizes(
            size_candidates,
            size_workers=size_workers,
            lane_size=lane_size,
        ).items():
            if size_bytes > max_lane_bytes:
                prunable.add(path)

    if effective_max_total_lane_bytes is not None and snapshot.lane_dirs:
        lane_sizes = snapshot.lane_sizes(
            snapshot.lane_dirs,
            size_workers=size_workers,
            lane_size=lane_size,
        )
        projected_total_bytes = sum(
            size_bytes for size_bytes, _errors in lane_sizes.values()
        )
        projected_total_bytes -= sum(
            lane_sizes[path][0] for path in prunable if path in lane_sizes
        )
        if projected_total_bytes > effective_max_total_lane_bytes:
            lru_candidates = sorted(
                (path for path in lane_dirs if path not in prunable),
                key=lambda path: (
                    snapshot.lane_mtime(path),
                    path.name.casefold(),
                    path.name,
                ),
            )
            for path in lru_candidates:
                prunable.add(path)
                projected_total_bytes -= lane_sizes[path][0]
                if projected_total_bytes <= effective_max_total_lane_bytes:
                    break

    return sorted(prunable)


def prune_stale_lanes(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    dry_run: bool = False,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> list[Path]:
    lane_root = validate_cargo_lanes_root(repo_root)
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
        lane_mtime=lane_mtime,
    )
    resolved_lane_root = lane_root.resolve()
    removed: list[Path] = []
    failures: list[Path] = []
    for path in prunable_lane_dirs(
        repo_root=repo_root,
        processes=snapshot.processes,
        snapshot=snapshot,
        keep_warm_per_base=keep_warm_per_base,
        max_age_days=max_age_days,
        max_lane_bytes=max_lane_bytes,
        max_total_lane_bytes=max_total_lane_bytes,
        max_total_target_bytes=max_total_target_bytes,
        now_timestamp=now_timestamp,
        lane_mtime=lane_mtime,
        lane_size=lane_size,
        size_workers=size_workers,
    ):
        if not path.exists():
            continue
        if is_indirect_directory(path):
            print(f"warning: skipping indirect lane path: {path}", file=sys.stderr)
            continue
        resolved_path = path.resolve()
        if not resolved_path.is_relative_to(resolved_lane_root):
            # A reparse point that escapes the lanes root should not brick
            # every future prune run; skip it loudly instead.
            print(
                f"warning: skipping lane outside {resolved_lane_root}: {resolved_path}",
                file=sys.stderr,
            )
            continue
        if cargo_lock_is_busy(path) or lane_active_lock_is_held(path):
            continue
        if not dry_run:
            trash_path: Path | None = None
            try:
                with cargo_lane_coordination_lock(lane_root):
                    if not path.exists():
                        continue
                    if is_indirect_directory(path):
                        print(
                            f"warning: lane became an indirect path before prune: {path}",
                            file=sys.stderr,
                        )
                        continue
                    if cargo_lock_is_busy(path) or lane_active_lock_is_held(path):
                        continue
                    generation = snapshot._lane_generations.get(path)
                    if generation is None or _lane_prune_generation(path) != generation:
                        # A completed reservation or same-name replacement invalidates
                        # the old plan, even when its locks have already been released.
                        continue
                    timestamp = time.strftime("%Y%m%d%H%M%S", time.gmtime())
                    centiseconds = int((time.time() % 1) * 100)
                    trash_path = path.with_name(
                        f"{path.name}.trash-{timestamp}{centiseconds:02d}0"
                    )
                    if trash_path.exists():
                        raise FileExistsError(trash_path)
                    if not _detach_expendable_lane_cache(path, trash_path):
                        continue
                # A new reservation may now recreate `path`; delete only the
                # uniquely renamed tree after releasing the coordination lock.
                remove_tree_allow_readonly(trash_path)
            except OSError as exc:
                if (
                    isinstance(exc, FileNotFoundError)
                    and not (trash_path if trash_path is not None else path).exists()
                ):
                    continue
                if trash_path is None:
                    if cargo_lock_is_busy(path) or lane_active_lock_is_held(path):
                        continue
                    failures.append(path)
                    print(
                        f"warning: failed to prune lane {path}: {exc}",
                        file=sys.stderr,
                    )
                    continue
                failures.append(trash_path)
                print(
                    f"warning: lane moved to deferred cleanup path {trash_path}: {exc}",
                    file=sys.stderr,
                )
                continue
        removed.append(path)
    # Retry trees an earlier prune renamed but could not delete. Their unique
    # names cannot be reserved again, so delete them in place instead of
    # renaming them once more.
    for trash_path in lane_trash_dirs(lane_root):
        if trash_path in failures or is_indirect_directory(trash_path):
            continue
        if cargo_lock_is_busy(trash_path) or lane_active_lock_is_held(trash_path):
            continue
        if (trash_path / "test-runner-logs").exists():
            # A legacy worker may have detached required evidence already.
            # Without a stable published-path handoff, absence of a build owner
            # does not authorize erasing it.
            failures.append(trash_path)
            print(
                f"warning: retained validation evidence in {trash_path}",
                file=sys.stderr,
            )
            continue
        if not dry_run:
            try:
                remove_tree_allow_readonly(trash_path)
            except OSError as exc:
                if isinstance(exc, FileNotFoundError) and not trash_path.exists():
                    continue
                failures.append(trash_path)
                print(
                    f"warning: deferred lane cleanup still pending for {trash_path}: {exc}",
                    file=sys.stderr,
                )
                continue
        removed.append(trash_path)
    if failures:
        raise OSError(f"Cargo lane cleanup incomplete; pending paths: {failures}")
    return removed


def prune_stale_lanes_plan(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> dict[str, object]:
    validate_cargo_lanes_root(repo_root)
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
        lane_mtime=lane_mtime,
    )
    lanes = prunable_lane_dirs(
        repo_root=repo_root,
        processes=snapshot.processes,
        snapshot=snapshot,
        keep_warm_per_base=keep_warm_per_base,
        max_age_days=max_age_days,
        max_lane_bytes=max_lane_bytes,
        max_total_lane_bytes=max_total_lane_bytes,
        max_total_target_bytes=max_total_target_bytes,
        now_timestamp=now_timestamp,
        lane_mtime=lane_mtime,
        lane_size=lane_size,
        size_workers=size_workers,
    )
    strays = prune_stray_cargo_target_dirs(repo_root=repo_root, dry_run=True)
    return {
        "type": "codexKdCargoLanePrunePlan",
        "repoRoot": str(repo_root),
        "keepWarmPerBase": keep_warm_per_base,
        "maxAgeDays": max_age_days,
        "maxLaneBytes": max_lane_bytes,
        "maxTotalLaneBytes": max_total_lane_bytes,
        "maxTotalTargetBytes": max_total_target_bytes,
        "lanes": [str(path) for path in lanes],
        "strayTargetDirs": [str(path) for path in strays],
    }


def remove_tree_allow_readonly(path: Path) -> None:
    def handle_remove_error(
        function: Callable[[str], None],
        name: str,
        _exc: object,
    ) -> None:
        os.chmod(name, 0o700)
        function(name)

    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=handle_remove_error)
    else:
        shutil.rmtree(path, onerror=handle_remove_error)


def prune_stray_cargo_target_dirs(
    *,
    repo_root: Path = REPO_ROOT,
    dry_run: bool = False,
) -> list[Path]:
    target_root = repo_root / "codex-rs" / "target"
    if not target_root.exists():
        return []
    resolved_target_root = target_root.resolve()
    detected: list[Path] = []
    for path in stray_cargo_target_dirs(repo_root=repo_root):
        if is_indirect_directory(path):
            print(f"warning: skipping indirect target path: {path}", file=sys.stderr)
            continue
        resolved_path = path.resolve()
        if resolved_path.parent != resolved_target_root:
            print(
                f"warning: skipping stray target outside {resolved_target_root}: {resolved_path}",
                file=sys.stderr,
            )
            continue
        # Raw Cargo commands do not participate in the lane reservation
        # protocol. A second liveness check therefore cannot close the race
        # between inspection and recursive deletion. Keep these paths visible
        # in plans and reports, but never auto-delete them.
        detected.append(path)
    return detected


def prune_stale_lanes_report(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    dry_run: bool = False,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    include_disk_report: bool = True,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> str:
    validate_cargo_lanes_root(repo_root)
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    if dry_run and include_disk_report and snapshot.process_scan_error is None:
        # Seed both budget selection and the final report from one observation.
        # Actual deletion must report a new post-prune size instead.
        snapshot.target_size()
    removed = prune_stale_lanes(
        repo_root=repo_root,
        processes=snapshot.processes,
        snapshot=snapshot,
        dry_run=dry_run,
        keep_warm_per_base=keep_warm_per_base,
        max_age_days=max_age_days,
        max_lane_bytes=max_lane_bytes,
        max_total_lane_bytes=max_total_lane_bytes,
        max_total_target_bytes=max_total_target_bytes,
        size_workers=size_workers,
    )
    detected_strays = prune_stray_cargo_target_dirs(
        repo_root=repo_root,
        dry_run=dry_run,
    )
    action = "would prune" if dry_run else "pruned"
    lines = ["target prune report"]
    if keep_warm_per_base > 0:
        lines.append(f"warm lanes kept per base: {keep_warm_per_base}")
    if max_age_days is not None:
        lines.append(f"max lane age: {max_age_days:g} days")
    if max_lane_bytes is not None:
        lines.append(f"max lane size: {format_bytes(max_lane_bytes)}")
    if max_total_lane_bytes is not None:
        lines.append(f"max aggregate lane size: {format_bytes(max_total_lane_bytes)}")
    if max_total_target_bytes is not None:
        lines.append(
            f"max aggregate target size: {format_bytes(max_total_target_bytes)}"
        )
    if removed:
        for path in removed:
            lines.append(f"{action}: {path}")
    else:
        lines.append("no stale lanes to prune")
    if detected_strays:
        for path in detected_strays:
            lines.append(f"detected stray target (not auto-pruned): {path}")
        lines.append("stray targets are diagnostic-only and were preserved")
    else:
        lines.append("no stray cargo target dirs detected")
    if include_disk_report:
        lines.extend(
            target_disk_report_lines(
                repo_root=repo_root,
                warn_bytes=warn_bytes,
                snapshot=snapshot if dry_run else None,
            )
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Inspect local Rust build health.")
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser(
        "lane-affinity", help="Compute Cargo lane affinity from a JSON argument array on stdin."
    )
    subparsers.add_parser("lanes", help="Show active/stale Cargo target lanes.")
    disk_parser = subparsers.add_parser(
        "disk", help="Show codex-rs/target disk usage and warnings."
    )
    disk_parser.add_argument("--warn-gib", type=positive_float, default=250.0)
    prune_parser = subparsers.add_parser(
        "prune", help="Remove inactive target/lanes directories."
    )
    add_prune_arguments(prune_parser)
    prune_parser.add_argument("--skip-disk-report", action="store_true")
    prune_parser.add_argument(
        "--json-plan",
        action="store_true",
        help="Emit the destructive prune plan as JSON without deleting anything.",
    )
    optimize_parser = subparsers.add_parser(
        "optimize", help="Prune inactive lanes while retaining warm build directories."
    )
    add_prune_arguments(optimize_parser)
    optimize_parser.add_argument("--include-prune-disk-report", action="store_true")
    run_lane_parser = subparsers.add_parser(
        "run-lane",
        help="Reserve a Cargo target lane for the lifetime of a child command.",
        description="Reserve a build lane without changing the working directory. "
        "Run Cargo from codex-rs to use its pinned toolchain and .cargo configuration. "
        "Explicit manifest overrides remain allowed, but "
        "--manifest-path alone does not select codex-rs's pinned toolchain "
        "or .cargo configuration.",
    )
    run_lane_parser.add_argument("--lane", required=True)
    run_lane_parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    run_lane_parser.add_argument("--lanes-root", type=Path)
    run_lane_parser.add_argument(
        "--warm-wait-seconds",
        type=_nonnegative_wait_seconds,
        default=0.0,
        help="Wait this long before optional cold overflow; without overflow, wait until a lane is free.",
    )
    run_lane_parser.add_argument(
        "--allow-cold-overflow",
        action="store_true",
        help="Allow a cold sibling build after the warm-lane wait; may duplicate compilation. "
        "Lanes of the family that build at once stay within --max-active-lanes.",
    )
    run_lane_parser.add_argument(
        "--max-active-lanes",
        type=positive_int,
        help="Lanes of this family that may build at once (default with "
        "--allow-cold-overflow: CODEX_CARGO_LANE_MAX_ACTIVE_PER_BASE, or "
        f"{DEFAULT_MAX_ACTIVE_LANES_PER_BASE}).",
    )
    run_lane_parser.add_argument(
        "--timing-json",
        type=Path,
        help="Write total wall-clock duration to a new JSON file (never overwrite).",
    )
    run_lane_parser.add_argument(
        "--lock-timeout-seconds",
        type=positive_float,
        default=LANE_COORDINATION_GRACE_SECONDS,
        help="Short creation/prune lock grace (default: 1 second). Does not wait for busy builds.",
    )
    run_lane_parser.add_argument(
        "--source-mirror",
        choices=SOURCE_MIRROR_MODES,
        default=os.environ.get("CODEX_CARGO_LANE_SOURCE_MIRROR", "auto"),
        help="auto (default): eligible --freeze-bundle builds and sealed external "
        "snapshots (no .git, no warm lane of their own) build incrementally in the "
        "nearest free shared slot through a content-synced copy; always: require "
        "that; never: build in place in the "
        "source-root lane.",
    )
    run_lane_parser.add_argument(
        "--freeze-bundle",
        type=Path,
        help="New directory: copy the built (or reused) executables there "
        f"(read-only) with {BUNDLE_RECEIPT_NAME} before the lane is released. "
        "Sealed snapshots without it freeze into the local bundle store.",
    )
    run_lane_parser.add_argument(
        "--rebuild",
        action="store_true",
        help="Build even when an exact frozen bundle already exists.",
    )
    run_lane_parser.add_argument("command_args", nargs=argparse.REMAINDER)
    find_parser = subparsers.add_parser(
        "find-bundle",
        help="Print a frozen bundle built from identical compiled inputs and settings.",
        description="Run from the source's codex-rs with the exact run-lane command. "
        "Candidates come from the local bundle index (written by every freeze). "
        "Exit 0 prints the newest matching bundle directory; exit 1 means build it; "
        "exit 3 (--include-legacy only) prints a pre-receipt bundle candidate.",
    )
    find_parser.add_argument(
        "--include-legacy",
        action="store_true",
        help="On a miss, also match pre-receipt *provenance*.json files below the "
        "search roots by recorded per-file source hashes, command and binary hashes.",
    )
    find_parser.add_argument("--repo-root", type=Path, required=True)
    find_parser.add_argument(
        "--search-root",
        type=Path,
        action="append",
        default=[],
        help="Also scan (and index) receipts below this directory; repeatable.",
    )
    find_parser.add_argument("command_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    keep_warm_per_base = getattr(args, "keep_warm_per_base", None)
    max_age_days = getattr(args, "max_age_days", None)
    if getattr(args, "all", False):
        keep_warm_per_base = 0
        max_age_days = None

    try:
        if args.command == "lane-affinity":
            command = json.load(sys.stdin)
            if not isinstance(command, list) or not all(isinstance(value, str) for value in command):
                raise ValueError("lane-affinity requires a JSON array of strings")
            print(_auto_lane_base(command))
        elif args.command == "lanes":
            print(lane_report())
        elif args.command == "disk":
            print(target_disk_report(warn_bytes=warn_bytes_from_gib(args.warn_gib)))
        elif args.command == "prune":
            if args.json_plan:
                print(
                    json.dumps(
                        prune_stale_lanes_plan(
                            keep_warm_per_base=keep_warm_per_base,
                            max_age_days=max_age_days,
                            max_lane_bytes=max_lane_bytes_from_args(args),
                            max_total_lane_bytes=max_total_lane_bytes_from_args(args),
                            max_total_target_bytes=max_total_target_bytes_from_args(
                                args
                            ),
                            size_workers=args.size_workers,
                        ),
                        separators=(",", ":"),
                    )
                )
            else:
                print(
                    prune_stale_lanes_report(
                        dry_run=args.dry_run,
                        warn_bytes=warn_bytes_from_gib(args.warn_gib),
                        keep_warm_per_base=keep_warm_per_base,
                        max_age_days=max_age_days,
                        max_lane_bytes=max_lane_bytes_from_args(args),
                        max_total_lane_bytes=max_total_lane_bytes_from_args(args),
                        max_total_target_bytes=max_total_target_bytes_from_args(args),
                        include_disk_report=not args.skip_disk_report,
                        size_workers=args.size_workers,
                    )
                )
        elif args.command == "optimize":
            print(
                target_optimize_report(
                    dry_run=args.dry_run,
                    warn_bytes=warn_bytes_from_gib(args.warn_gib),
                    keep_warm_per_base=keep_warm_per_base,
                    max_age_days=max_age_days,
                    max_lane_bytes=max_lane_bytes_from_args(args),
                    max_total_lane_bytes=max_total_lane_bytes_from_args(args),
                    max_total_target_bytes=max_total_target_bytes_from_args(args),
                    include_prune_disk_report=args.include_prune_disk_report,
                    size_workers=args.size_workers,
                )
            )
        elif args.command == "run-lane":
            command_args = list(args.command_args)
            if command_args[:1] == ["--"]:
                command_args = command_args[1:]
            return run_in_cargo_lane(
                repo_root=args.repo_root.absolute(),
                requested_lane=args.lane,
                command=command_args,
                lane_root=args.lanes_root,
                lock_timeout_seconds=args.lock_timeout_seconds,
                timing_path=args.timing_json,
                warm_wait_seconds=args.warm_wait_seconds,
                allow_cold_overflow=args.allow_cold_overflow,
                max_active_lanes=args.max_active_lanes
                or (max_active_lanes_per_base() if args.allow_cold_overflow else None),
                source_mirror=args.source_mirror,
                freeze_bundle_dir=args.freeze_bundle,
                rebuild=args.rebuild,
            )
        elif args.command == "find-bundle":
            command_args = list(args.command_args)
            if command_args[:1] == ["--"]:
                command_args = command_args[1:]
            if not command_args:
                raise ValueError("find-bundle requires the build command after --")
            bundle, scanned = find_reusable_bundle(
                repo_root=args.repo_root.absolute(),
                command=command_args,
                search_roots=args.search_root,
            )
            if bundle is None and args.include_legacy:
                legacy = find_legacy_bundles(
                    repo_root=args.repo_root.absolute(),
                    command=command_args,
                    search_roots=args.search_root,
                )
                if legacy:
                    bundle, provenance, caveat = legacy[0]
                    print(f"{caveat} ({provenance})", file=sys.stderr)
                    print(bundle)
                    return 3
            if bundle is None:
                print(f"no matching bundle ({scanned} receipts scanned)", file=sys.stderr)
                return 1
            print(bundle)
        else:
            parser.error(f"unknown command {args.command}")
    except (CargoLanesRootValidationError, OSError, RuntimeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
