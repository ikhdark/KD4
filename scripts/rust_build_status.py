#!/usr/bin/env python3

from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
from dataclasses import field
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time
from typing import Callable
from typing import BinaryIO
from typing import Iterator
from typing import Mapping
from typing import Sequence
from typing import TextIO

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.process_owner import run_owned, CleanupFailed  # noqa: E402

from scripts.tool_versions import cargo_lane_patterns  # noqa: E402

from scripts.rust_build_status_support import (  # noqa: E402
    add_prune_arguments,
    bounded_size_workers,
    build_doctor_report,
    bytes_from_gib,
    directory_size_bytes,
    directory_sizes_bytes,
    format_bytes,
    lane_report,
    lane_report_lines,
    max_lane_bytes_from_args,
    max_total_lane_bytes_from_args,
    max_total_target_bytes_from_args,
    msvc_linkers_from_cargo_config,
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
    "build_doctor_report",
    "bytes_from_gib",
    "directory_size_bytes",
    "directory_sizes_bytes",
    "format_bytes",
    "lane_report",
    "lane_report_lines",
    "max_lane_bytes_from_args",
    "max_total_lane_bytes_from_args",
    "max_total_target_bytes_from_args",
    "msvc_linkers_from_cargo_config",
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
WINDOWS_MSVC_TARGETS = (
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)
BYTES_PER_KIB = 1024
BYTES_PER_MIB = BYTES_PER_KIB * 1024
BYTES_PER_GIB = BYTES_PER_MIB * 1024
DEFAULT_TARGET_WARN_BYTES = 250 * BYTES_PER_GIB
TIMESTAMPED_LANE_RE = re.compile(r"^(?P<base>.+)-\d{14}$")
LANE_SUFFIX_RE = re.compile(r"^(?P<base>.+)-(?P<suffix>\d+)$")
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


class CargoLanesRootValidationError(ValueError):
    pass


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


@dataclass(frozen=True)
class RustProcess:
    pid: int
    name: str
    command_line: str
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
    _lane_mtimes: dict[Path, float] = field(default_factory=dict, repr=False)
    _lane_sizes: dict[Path, tuple[int, int]] = field(default_factory=dict, repr=False)

    @classmethod
    def collect(
        cls,
        *,
        repo_root: Path = REPO_ROOT,
        processes: Sequence[RustProcess] | None = None,
        lane_mtime: Callable[[Path], float] | None = None,
    ) -> "BuildStatusSnapshot":
        discovered = active_rust_processes() if processes is None else processes
        process_list = list(discovered)
        lane_root = cargo_lanes_root(repo_root)
        lane_dirs = existing_lane_dirs(lane_root)
        lane_names_by_process: dict[tuple[int, str, str], str] = {}
        for process in process_list:
            lane_name = lane_name_for_process(process)
            if lane_name is not None:
                lane_names_by_process[rust_process_key(process)] = lane_name
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
        )

    def lane_name_for(self, process: RustProcess) -> str | None:
        return self.lane_names_by_process.get(rust_process_key(process))

    def lane_mtime(self, path: Path) -> float:
        if path not in self._lane_mtimes:
            self._lane_mtimes[path] = self._lane_mtime(path)
        return self._lane_mtimes[path]

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
    return active_rust_processes_windows()


def active_rust_processes_windows() -> list[RustProcess]:
    shell = shutil.which("pwsh") or shutil.which("powershell") or "powershell"
    command = (
        "$selfPid = $PID; "
        f'Get-CimInstance Win32_Process -Filter "({WINDOWS_RUST_PROCESS_FILTER}) '
        'AND ProcessId != $selfPid" | '
        "Select-Object Name,ProcessId,CommandLine | ConvertTo-Json -Compress"
    )
    try:
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", command],
            check=True,
            capture_output=True,
            text=True,
            timeout=WINDOWS_PROCESS_SCAN_TIMEOUT_SECONDS,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
        print(f"warning: Windows Rust process scan failed: {exc}", file=sys.stderr)
        return []

    if not result.stdout.strip():
        return []
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        print(
            f"warning: Windows Rust process scan returned invalid JSON: {exc}",
            file=sys.stderr,
        )
        return []
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


def _classify_rust_process(process: RustProcess) -> RustProcessClassification:
    executable = process.name.lower().removesuffix(".exe")
    lane_name = _lane_name_from_command_line(process.command_line)
    contains_rust_command = bool(RUST_PROCESS_TOKEN_RE.search(process.command_line))
    if executable in RUST_PROCESS_NAMES:
        is_rust = True
    elif executable in RUST_WRAPPER_PROCESS_NAMES:
        is_rust = lane_name is not None or contains_rust_command
    else:
        is_rust = contains_rust_command
    return RustProcessClassification(is_rust=is_rust, lane_name=lane_name)


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


def cargo_lock_is_busy(target_dir: Path) -> bool:
    # Cargo locks each profile directory, optionally below a target triple.
    # Inspect only those two levels; never follow junctions into other trees.
    try:
        for child in target_dir.iterdir():
            if not child.is_dir():
                continue
            if is_indirect_directory(child):
                return True
            if _cargo_lock_file_is_busy(child / ".cargo-lock"):
                return True
            for profile in child.iterdir():
                if profile.is_dir():
                    if is_indirect_directory(profile):
                        return True
                    if _cargo_lock_file_is_busy(profile / ".cargo-lock"):
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
        import msvcrt

        try:
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
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
    if (lane_dir / ".lane-cleanup-unconfirmed").exists():
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
        import msvcrt

        handle.seek(0)
        try:
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError:
            return True
        msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
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
            time.sleep(0.05)
    acquired = False
    try:
        handle.seek(0, os.SEEK_END)
        if handle.tell() == 0:
            handle.write(b"\0")
            handle.flush()
        handle.seek(0)
        import msvcrt

        while True:
            try:
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                acquired = True
                break
            except OSError as exc:
                if not _is_file_lock_contention(exc):
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError(
                        f"timed out waiting for lane coordination lock {lock_path}"
                    )
                time.sleep(0.05)
        yield
    finally:
        try:
            if acquired:
                handle.seek(0)
                import msvcrt

                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        finally:
            handle.close()


def _release_binary_file_lock(handle: BinaryIO) -> None:
    handle.seek(0)
    import msvcrt

    msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)


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
        import msvcrt

        try:
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        except OSError as exc:
            if _is_file_lock_contention(exc):
                return None
            raise
        acquired = True
        return handle
    finally:
        if not acquired:
            handle.close()


def _safe_lane_name(value: str) -> str:
    if (
        not value
        or re.fullmatch(r"[A-Za-z0-9_.-]+", value) is None
        or re.fullmatch(r"\.+", value) is not None
        or re.search(r"\.trash-\d{17}$", value, re.IGNORECASE) is not None
    ):
        raise ValueError(f"invalid Cargo lane name {value!r}")
    return value


def _auto_lane_base(command: Sequence[str]) -> str:
    signature = " ".join(command).strip()
    release = re.search(
        r"(?:^|\s)(?:--release|-r|--profile(?:=|\s+)release)(?:\s|$)",
        signature,
    )
    from scripts.rust_tool_env import cargo_package_specs

    # Affinity also covers Cargo watch's embedded --exec/-x command strings.
    packages = cargo_package_specs(signature.split())
    if packages:
        base = packages[0]
    elif command:
        program = Path(command[0]).stem
        digest = hashlib.sha1(signature.encode("utf-8")).hexdigest()[:8]
        base = f"{program}-{digest}"
    else:
        base = "auto"
    safe = re.sub(r"[^A-Za-z0-9_.-]", "-", base).strip("-") or "auto"
    return f"{safe}-release" if release is not None else safe


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


@contextmanager
def reserve_cargo_lane(
    *,
    repo_root: Path,
    requested_lane: str,
    command: Sequence[str],
    lane_root: Path | None = None,
    lock_timeout_seconds: float = 30.0,
    warm_wait_seconds: float = 30.0,
) -> Iterator[tuple[str, Path]]:
    explicit = requested_lane != "auto"
    base_lane = _safe_lane_name(
        requested_lane if explicit else _auto_lane_base(command)
    )
    root = initialize_cargo_lanes_root(
        repo_root,
        lane_root if lane_root is not None else cargo_lanes_root(repo_root),
    )
    active_handle: BinaryIO | None = None
    target_dir: Path | None = None
    resolved_lane: str | None = None
    deadline = time.monotonic() + max(0.0, warm_wait_seconds)
    announced_wait = False
    while active_handle is None:
        busy_warm = False
        deferred_cold = False
        with cargo_lane_coordination_lock(root, timeout_seconds=lock_timeout_seconds):
            candidates = _lane_reservation_candidates(
                root, base_lane, prefer_warm=not explicit
            )
            # Existing directories alone are not evidence of reusable build work.
            warm_paths = {
                candidate.path
                for candidate in candidates
                if any(
                    is_cargo_artifact_dir(candidate.path / profile)
                    for profile in ("debug", "release", "dev-small")
                )
            }
            candidates.sort(key=lambda candidate: candidate.path not in warm_paths)
            for candidate in candidates:
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
                if not warm and busy_warm and time.monotonic() < deadline:
                    deferred_cold = True
                    break
                candidate_dir.mkdir(exist_ok=True)
                if cargo_lock_is_busy(candidate_dir):
                    busy_warm |= warm
                    continue
                active_handle = _try_acquire_binary_file_lock(
                    candidate_dir / ".lane-active.lock"
                )
                if active_handle is not None:
                    target_dir = candidate_dir.resolve()
                    resolved_lane = candidate_name
                    break
                busy_warm |= warm
        if active_handle is not None or not deferred_cold:
            break
        if not announced_wait:
            print(
                f"waiting up to {warm_wait_seconds:g}s for a warm Cargo lane for {base_lane!r}",
                file=sys.stderr,
            )
            announced_wait = True
        # Release the coordination lock while waiting so owners can finish and
        # other work can reserve unrelated lanes.
        time.sleep(min(0.25, max(0.0, deadline - time.monotonic())))
    if active_handle is None or target_dir is None or resolved_lane is None:
        raise RuntimeError(f"unable to reserve an idle Cargo lane for {base_lane!r}")
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


def _cargo_watch_exec_with_target_dir(command: str, target_dir: Path) -> str:
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
        _require_reserved_target_dir(candidate, target_dir)
    if target_matches:
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


def _cargo_subcommand_index(command: Sequence[str]) -> int | None:
    index = 1
    if index < len(command) and command[index].startswith("+"):
        index += 1
    global_options_with_values = {"--color", "--config", "-C", "-Z"}
    global_long_options_with_values = {"--color", "--config"}
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
            argument.startswith(f"{option}=")
            for option in global_long_options_with_values
        ) or any(
            argument.startswith(option) and argument != option
            for option in {"-C", "-Z"}
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
    tail = result[subcommand_index + 1 :]
    if subcommand == "nextest":
        if not tail or tail[0] not in {"archive", "run", "list"}:
            return result
        if _cargo_target_dir_is_present(
            result,
            start_index=subcommand_index + 2,
            target_dir=target_dir,
        ):
            return result
        return [
            *result[: subcommand_index + 2],
            "--target-dir",
            target_arg,
            *result[subcommand_index + 2 :],
        ]
    if subcommand == "watch":
        index = subcommand_index + 1
        has_exec = False
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
                result[index] = _cargo_watch_exec_with_target_dir(
                    result[index], target_dir
                )
                has_exec = True
            elif argument.startswith("--exec="):
                result[index] = "--exec=" + _cargo_watch_exec_with_target_dir(
                    argument.removeprefix("--exec="), target_dir
                )
                has_exec = True
            elif argument.startswith("-x"):
                result[index] = "-x" + _cargo_watch_exec_with_target_dir(
                    argument[2:].removeprefix("="), target_dir
                )
                has_exec = True
            elif argument in value_options:
                if index + 1 >= len(result):
                    raise ValueError(f"Cargo watch {argument} requires a value")
                index += 1
            elif not argument.startswith("-"):
                raise ValueError(
                    "Cargo watch positional commands cannot enforce a reserved target; use --exec/-x"
                )
            index += 1
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


def _direct_reserved_lane_command(
    command: Sequence[str],
    child_env: dict[str, str],
    *,
    repo_root: Path,
    target_dir: Path,
) -> list[str] | None:
    if len(command) < 2 or Path(command[0]).stem.lower() != "just":
        return None

    recipe = command[1]
    arguments = list(command[2:])
    runner_arguments: list[str] | None = None
    if recipe == "_core-test-reserved" and len(arguments) >= 2:
        profile, target, *forwarded = arguments
        runner_arguments = ["run-target", "--profile", profile, target, *forwarded]
    elif recipe == "_core-gate-reserved" and arguments:
        profile = "fast"
        runner_arguments = ["run-gate", "--profile", profile, *arguments]
    elif recipe == "_core-parity-reserved" and len(arguments) >= 2:
        profile = "fast"
        runner_arguments = ["parity", "--profile", profile, *arguments]
    if runner_arguments is not None:
        child_env["RUST_MIN_STACK"] = RUST_MIN_STACK_BYTES
        child_env["NEXTEST_PROFILE"] = profile
        return [
            sys.executable,
            str(repo_root / "scripts" / "rust_test_runner.py"),
            "--target-dir",
            str(target_dir),
            *runner_arguments,
        ]

    profile: str | None = None
    cargo_arguments: list[str] | None = None
    if recipe == "_test-lane-local-reserved":
        profile = "local"
        cargo_arguments = ["--no-fail-fast", *arguments]
    elif recipe == "_test-lane-fast-reserved":
        profile = "fast"
        cargo_arguments = arguments
    elif recipe == "_test-lane-package-reserved" and arguments:
        package, *forwarded = arguments
        profile = "fast"
        cargo_arguments = ["-p", package, *forwarded]
    else:
        return None

    from scripts.rust_test_runner import RunnerError, guard_generic_recipe_args

    try:
        guard_generic_recipe_args(cargo_arguments, recipe=recipe)
    except RunnerError as exc:
        raise ValueError(str(exc)) from exc
    child_env["RUST_MIN_STACK"] = RUST_MIN_STACK_BYTES
    child_env["NEXTEST_PROFILE"] = profile
    return ["cargo", "nextest", "run", *cargo_arguments]


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
            keep_warm_per_base=1,
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
    worker = repo_root / "scripts" / "cargo-lane-trash-cleanup.ps1"
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
        str(repo_root / "scripts" / "cargo-lane-trash-cleanup.ps1"),
        "-LanesRoot",
        str(lane_root),
    ]
    if due:
        command.append("-Prune")
    # The worker logs its own failures after acquiring ownership. Opening its
    # log here conflicts with PowerShell's append handle on Windows.
    child = subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        close_fds=True,
    )
    # Reap asynchronously if this launcher stays alive; never await pruning.
    import threading

    threading.Thread(target=child.wait, daemon=True).start()


def run_in_cargo_lane(
    *,
    repo_root: Path,
    requested_lane: str,
    command: Sequence[str],
    lane_root: Path | None = None,
    lock_timeout_seconds: float = 30.0,
    timing_path: Path | None = None,
) -> int:
    if not command:
        raise ValueError("run-lane requires a command after --")
    # Open exclusively before any child work: a bad path must not launch a build
    # or overwrite an earlier measurement. Reporting never changes the child exit.
    stream = timing_path.open("x", encoding="utf-8") if timing_path else None
    phases = dict.fromkeys(
        ("reservation", "setup", "maintenance", "command", "release")
    )
    record = {
        "schemaVersion": 1,
        "startedUnixMs": time.time_ns() // 1_000_000,
        "requestedLane": requested_lane,
        "resolvedLane": None,
        "targetDir": None,
        "command": list(command),
        "exitCode": None,
        "status": "error",
        "phaseDurationsMs": phases,
    }
    phase = "reservation"
    started = previous = time.perf_counter_ns()

    def next_phase(name: str | None) -> None:
        nonlocal phase, previous
        now = time.perf_counter_ns()
        if phase is not None:
            phases[phase] = (now - previous) / 1_000_000
        phase, previous = name, now

    try:
        with reserve_cargo_lane(
            repo_root=repo_root,
            requested_lane=requested_lane,
            command=command,
            lane_root=lane_root,
            lock_timeout_seconds=lock_timeout_seconds,
        ) as (resolved_lane, target_dir):
            next_phase("setup")
            record.update(resolvedLane=resolved_lane, targetDir=str(target_dir))
            if requested_lane != "auto" and resolved_lane != requested_lane:
                print(
                    f"warning: requested Cargo lane {requested_lane!r} is busy; "
                    f"using {resolved_lane!r}",
                    file=sys.stderr,
                )
            child_env = os.environ.copy()
            # Keep the lane out of Cargo's environment so its absolute path does
            # not fragment compiler-cache keys. Nested recipes consume CODEX's value.
            child_env.pop("CARGO_TARGET_DIR", None)
            child_env.pop("CODEX_CARGO_LANE_TARGET_DIR", None)
            child_env.pop("CODEX_CARGO_LANE_OWNER_PID", None)
            direct_command = _direct_reserved_lane_command(
                command, child_env, repo_root=repo_root, target_dir=target_dir
            )
            if direct_command is None:
                child_env["CODEX_CARGO_LANE_OWNER_PID"] = str(os.getpid())
                child_env["CODEX_CARGO_LANE_TARGET_DIR"] = str(target_dir)
            child_command = _cargo_command_with_target_dir(
                direct_command if direct_command is not None else command,
                target_dir,
            )
            if not Path(child_command[0]).parent.name:
                resolved_program = shutil.which(
                    child_command[0], path=child_env.get("PATH")
                )
                if resolved_program is not None:
                    child_command[0] = resolved_program
            next_phase("maintenance")
            try:
                request_cargo_lane_maintenance(repo_root, target_dir.parent)
            except OSError as error:
                print(
                    f"warning: could not request Cargo maintenance: {error}",
                    file=sys.stderr,
                )
            next_phase("command")
            try:
                try:
                    exit_code = run_owned(
                        child_command,
                        env=child_env,
                        check=False,
                        stdout=sys.stdout,
                        stderr=sys.stderr,
                    ).returncode
                except CleanupFailed:
                    (target_dir / ".lane-cleanup-unconfirmed").write_text(
                        "Process cleanup was not confirmed; inspect descendants before removing this quarantine.\n"
                    )
                    raise
                record.update(
                    exitCode=exit_code,
                    status="completed" if exit_code == 0 else "failed",
                )
                return exit_code
            finally:
                next_phase("release")
    except BaseException as error:
        record.update(
            status="interrupted" if isinstance(error, KeyboardInterrupt) else "error",
            errorType=type(error).__name__,
        )
        raise
    finally:
        next_phase(None)
        record["totalMs"] = (previous - started) / 1_000_000
        if stream is not None:
            try:
                with stream:
                    json.dump(record, stream, indent=2)
                    stream.write("\n")
            except OSError as error:
                print(
                    f"warning: Cargo lane timing could not be saved: {error}",
                    file=sys.stderr,
                )


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
    return sorted(
        path
        for path in lane_root.iterdir()
        if path.is_dir() and not is_indirect_directory(path)
    )


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
    lane_dirs = snapshot.stale_lanes
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
        non_lane_size_bytes, _errors = target_non_lane_size_bytes(
            repo_root=repo_root,
            lane_root=lane_root,
            size_workers=size_workers,
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
                    timestamp = time.strftime("%Y%m%d%H%M%S", time.gmtime())
                    centiseconds = int((time.time() % 1) * 100)
                    trash_path = path.with_name(
                        f"{path.name}.trash-{timestamp}{centiseconds:02d}0"
                    )
                    if trash_path.exists():
                        raise FileExistsError(trash_path)
                    path.replace(trash_path)
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
            target_disk_report_lines(repo_root=repo_root, warn_bytes=warn_bytes)
        )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Inspect local Rust build health.")
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser(
        "doctor", help="Show local Rust build environment and contention."
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
        "optimize", help="Show doctor output, then prune inactive target lanes."
    )
    add_prune_arguments(optimize_parser)
    optimize_parser.add_argument("--include-prune-disk-report", action="store_true")
    run_lane_parser = subparsers.add_parser(
        "run-lane",
        help="Reserve a Cargo target lane for the lifetime of a child command.",
    )
    run_lane_parser.add_argument("--lane", required=True)
    run_lane_parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    run_lane_parser.add_argument("--lanes-root", type=Path)
    run_lane_parser.add_argument(
        "--timing-json",
        type=Path,
        help="Write phase durations to a new JSON file (never overwrite).",
    )
    run_lane_parser.add_argument(
        "--lock-timeout-seconds",
        type=positive_float,
        default=30.0,
    )
    run_lane_parser.add_argument("command_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    keep_warm_per_base = getattr(args, "keep_warm_per_base", None)
    max_age_days = getattr(args, "max_age_days", None)
    if getattr(args, "all", False):
        keep_warm_per_base = 0
        max_age_days = None

    try:
        if args.command == "doctor":
            print(build_doctor_report())
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
            )
        else:
            parser.error(f"unknown command {args.command}")
    except (CargoLanesRootValidationError, OSError, RuntimeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
