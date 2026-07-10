#!/usr/bin/env python3

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from dataclasses import field
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import time
import tomllib
from typing import Callable
from typing import Mapping
from typing import Sequence
from typing import TextIO

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.tool_versions import JUST_LANE_PATTERN  # noqa: E402
from scripts.tool_versions import JUST_FIXED_LANE_NAMES  # noqa: E402
from scripts.tool_versions import JUST_FIXED_LANE_PATTERN  # noqa: E402
from scripts.tool_versions import LANE_PATH_PATTERN  # noqa: E402
from scripts.tool_versions import SCRIPT_LANE_PATTERN  # noqa: E402


LANE_RE = re.compile(LANE_PATH_PATTERN, re.IGNORECASE)
SCRIPT_LANE_RE = re.compile(SCRIPT_LANE_PATTERN)
JUST_LANE_RE = re.compile(JUST_LANE_PATTERN)
JUST_FIXED_LANE_RE = re.compile(JUST_FIXED_LANE_PATTERN)
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
DEFAULT_TARGET_WARN_BYTES = 50 * BYTES_PER_GIB
TIMESTAMPED_LANE_RE = re.compile(r"^(?P<base>.+)-\d{14}$")
LANE_SUFFIX_RE = re.compile(r"^(?P<base>.+)-(?P<suffix>\d+)$")
WINDOWS_RUST_PROCESS_FILTER = " OR ".join(
    f"Name = '{name}.exe'"
    for name in (*RUST_PROCESS_NAMES, *RUST_WRAPPER_PROCESS_NAMES)
)
DEFAULT_LANE_SIZE_WORKERS = 2
MAX_LANE_SIZE_WORKERS = 4
DEFAULT_PRUNE_KEEP_WARM_PER_BASE = 1
DEFAULT_PRUNE_MAX_AGE_DAYS = 7.0
LANE_LAST_USED_STAMP = ".lane-last-used"
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


def cargo_lanes_root(
    repo_root: Path = REPO_ROOT,
    env: Mapping[str, str] = os.environ,
) -> Path:
    raw = env.get("CODEX_CARGO_LANES_ROOT", "").strip()
    if not raw:
        return repo_root / "codex-rs" / "target" / "lanes"
    path = Path(raw).expanduser()
    if not path.is_absolute():
        path = repo_root / path
    return path.resolve()


@dataclass(frozen=True)
class RustProcess:
    pid: int
    name: str
    command_line: str


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
        process_list = active_rust_processes() if processes is None else list(processes)
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
    if os.name == "nt":
        return active_rust_processes_windows()
    return active_rust_processes_posix()


def active_rust_processes_windows() -> list[RustProcess]:
    shell = shutil.which("pwsh") or shutil.which("powershell") or "powershell"
    command = (
        f'Get-CimInstance Win32_Process -Filter "{WINDOWS_RUST_PROCESS_FILTER}" | '
        "Select-Object Name,ProcessId,CommandLine | ConvertTo-Json -Compress"
    )
    try:
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", command],
            check=True,
            capture_output=True,
            text=True,
            timeout=5,
            creationflags=(subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0),
        )
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return []

    if not result.stdout.strip():
        return []
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
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
        if is_rust_process(process):
            processes.append(process)
    return processes


def active_rust_processes_posix() -> list[RustProcess]:
    try:
        result = subprocess.run(
            ["ps", "-eo", "pid=,comm=,args="],
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return []

    processes = []
    for line in result.stdout.splitlines():
        parts = line.strip().split(maxsplit=2)
        if len(parts) < 2:
            continue
        try:
            pid = int(parts[0])
        except ValueError:
            continue
        name = parts[1]
        command_line = parts[2] if len(parts) > 2 else name
        process = RustProcess(pid=pid, name=name, command_line=command_line)
        if is_rust_process(process):
            processes.append(process)
    return processes


def is_rust_process(process: RustProcess) -> bool:
    name = process.name.lower().removesuffix(".exe")
    if name in RUST_PROCESS_NAMES:
        return True
    if name in RUST_WRAPPER_PROCESS_NAMES:
        return lane_name_for_process(process) is not None or bool(
            RUST_PROCESS_TOKEN_RE.search(process.command_line)
        )
    return bool(RUST_PROCESS_TOKEN_RE.search(process.command_line))


def lane_name_for_process(process: RustProcess) -> str | None:
    command_line = process.command_line
    if match := LANE_RE.search(command_line):
        return match.group(1)
    if match := SCRIPT_LANE_RE.search(command_line):
        return match.group(1)
    if match := JUST_LANE_RE.search(command_line):
        return match.group(1)
    if match := JUST_FIXED_LANE_RE.search(command_line):
        return JUST_FIXED_LANE_NAMES[match.group(1)]
    return None


def shared_target_rust_processes(
    processes: Sequence[RustProcess],
    lane_names_by_process: Mapping[tuple[int, str, str], str] | None = None,
) -> list[RustProcess]:
    shared = []
    for process in processes:
        lane_name = (
            lane_names_by_process.get(rust_process_key(process))
            if lane_names_by_process is not None
            else lane_name_for_process(process)
        )
        if (
            is_rust_process(process)
            and lane_name is None
            and "nextest show-config" not in process.command_line
        ):
            shared.append(process)
    return shared


def has_shared_target_rust_jobs(processes: Sequence[RustProcess] | None = None) -> bool:
    processes = active_rust_processes() if processes is None else processes
    return bool(shared_target_rust_processes(processes))


def cargo_lock_is_busy(target_dir: Path) -> bool:
    lock_path = target_dir / ".cargo-lock"
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
        if os.name == "nt":
            import msvcrt

            try:
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
                return False
            except OSError:
                return True
        else:
            import fcntl

            try:
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
        if os.name == "nt":
            import msvcrt

            handle.seek(0)
            try:
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
            except OSError:
                return True
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
            return False

        import fcntl

        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return True
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        return False
    except OSError:
        return True
    finally:
        if handle is not None:
            handle.close()


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
    if stamp.is_file():
        return stamp.stat().st_mtime
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
    return newest or path.stat().st_mtime


def format_bytes(size_bytes: int) -> str:
    if size_bytes >= BYTES_PER_GIB:
        return f"{size_bytes / BYTES_PER_GIB:.2f} GiB"
    if size_bytes >= BYTES_PER_MIB:
        return f"{size_bytes / BYTES_PER_MIB:.2f} MiB"
    if size_bytes >= BYTES_PER_KIB:
        return f"{size_bytes / BYTES_PER_KIB:.2f} KiB"
    return f"{size_bytes} B"


def directory_size_bytes(path: Path) -> tuple[int, int]:
    if not path.exists():
        return 0, 0

    total = 0
    errors = 0
    stack = [os.fspath(path)]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as entries:
                for entry in entries:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            total += entry.stat(follow_symlinks=False).st_size
                    except OSError:
                        errors += 1
        except OSError:
            errors += 1
    return total, errors


def bounded_size_workers(size_workers: int, path_count: int) -> int:
    if path_count <= 0:
        return 0
    return min(max(1, size_workers), MAX_LANE_SIZE_WORKERS, path_count)


def directory_sizes_bytes(
    paths: Sequence[Path],
    *,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
    size_func: Callable[[Path], tuple[int, int]] = directory_size_bytes,
) -> dict[Path, tuple[int, int]]:
    workers = bounded_size_workers(size_workers, len(paths))
    if workers <= 1:
        return {path: size_func(path) for path in paths}
    with ThreadPoolExecutor(max_workers=workers) as executor:
        return dict(zip(paths, executor.map(size_func, paths)))


def target_disk_report(
    *,
    repo_root: Path = REPO_ROOT,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
) -> str:
    return "\n".join(
        [
            "target disk report",
            *target_disk_report_lines(repo_root=repo_root, warn_bytes=warn_bytes),
        ]
    )


def target_disk_report_lines(
    *,
    repo_root: Path,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
) -> list[str]:
    target_root = repo_root / "codex-rs" / "target"
    lines = [f"target root: {target_root}"]
    if not target_root.exists():
        lines.append("target disk: missing")
        return lines

    size_bytes, errors = directory_size_bytes(target_root)
    lines.append(f"target disk: {format_bytes(size_bytes)}")
    lines.append(f"target warning threshold: {format_bytes(warn_bytes)}")
    if errors:
        lines.append(f"target disk scan errors: {errors}")
    if size_bytes > warn_bytes:
        lines.append(
            "target disk warning: codex-rs/target is above the local budget; "
            "run `just target-prune` for stale lanes, or remove `codex-rs/target` "
            "only after `just rust-build-doctor` shows no active Rust jobs."
        )
    strays = stray_cargo_target_dirs(repo_root=repo_root)
    if strays:
        names = ", ".join(path.name for path in strays)
        lines.append(
            "stray cargo target dirs: "
            f"{names}; prefer `just cargo-lane <lane> ...` or `just test-lane <lane> ...`"
        )
    return lines


def build_doctor_report(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    tool_lookup: Callable[[str], str | None] = shutil.which,
    env: Mapping[str, str] | None = None,
) -> str:
    env = os.environ if env is None else env
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    processes = snapshot.processes
    shared = shared_target_rust_processes(
        processes,
        snapshot.lane_names_by_process,
    )
    lane_processes = [
        process for process in processes if snapshot.lane_name_for(process)
    ]
    sccache = tool_lookup("sccache")
    msvc_linkers = msvc_linkers_from_cargo_config(repo_root)

    lines = [
        "Rust build doctor",
        f"repo: {repo_root}",
        f"sccache: {sccache or 'not found'}",
        f"RUSTC_WRAPPER: {env.get('RUSTC_WRAPPER') or '(unset)'}",
    ]
    for target in WINDOWS_MSVC_TARGETS:
        lines.append(
            f"MSVC linker config {target}: {msvc_linkers.get(target) or '(unset)'}"
        )

    lines.append(
        f"active Rust jobs: {len(processes)} total, {len(shared)} shared-target, {len(lane_processes)} lane"
    )
    if shared:
        lines.append(
            "shared-target jobs are active; prefer `just test-lane-fast <lane> ...`"
        )
    if lane_processes:
        active_lanes = sorted(snapshot.active_lanes)
        lines.append(
            "active lanes: " + ", ".join(lane for lane in active_lanes if lane)
        )

    lines.extend(target_disk_report_lines(repo_root=repo_root))
    lines.extend(
        lane_report_lines(
            repo_root=repo_root,
            processes=processes,
            snapshot=snapshot,
        )
    )
    return "\n".join(lines)


def msvc_linkers_from_cargo_config(repo_root: Path) -> dict[str, str]:
    config_path = repo_root / "codex-rs" / ".cargo" / "config.toml"
    try:
        config = tomllib.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return {}
    target_config = config.get("target", {})
    if not isinstance(target_config, dict):
        return {}

    linkers: dict[str, str] = {}
    for target in WINDOWS_MSVC_TARGETS:
        target_table = target_config.get(target, {})
        if isinstance(target_table, dict):
            linker = target_table.get("linker")
            if isinstance(linker, str):
                linkers[target] = linker
    return linkers


def lane_report(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
) -> str:
    snapshot = BuildStatusSnapshot.collect(repo_root=repo_root, processes=processes)
    return "\n".join(
        lane_report_lines(
            repo_root=repo_root,
            processes=snapshot.processes,
            snapshot=snapshot,
        )
    )


def lane_report_lines(
    *,
    repo_root: Path,
    processes: Sequence[RustProcess],
    snapshot: BuildStatusSnapshot | None = None,
) -> list[str]:
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    lane_root = cargo_lanes_root(repo_root)
    existing_names = {path.name for path in snapshot.lane_dirs}
    active_lanes = snapshot.active_lanes
    active_existing = sorted(active_lanes & existing_names)
    active_external = sorted(active_lanes - existing_names)
    stale = snapshot.stale_lanes
    protected = protected_warm_lane_names(
        stale,
        keep_warm_per_base=DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
        lane_mtime=snapshot.lane_mtime,
    )
    prunable = set(
        prunable_lane_dirs(
            repo_root=repo_root,
            processes=snapshot.processes,
            snapshot=snapshot,
        )
    )

    lines = ["lane report", f"lane root: {lane_root}"]
    lines.append(
        "active: " + (", ".join(active_existing) if active_existing else "(none)")
    )
    if active_external:
        lines.append("active without directory: " + ", ".join(active_external))
    lines.append(
        "stale: " + (", ".join(path.name for path in stale) if stale else "(none)")
    )
    warm_protected = sorted(path.name for path in stale if path.name in protected)
    if warm_protected:
        lines.append("warm-protected: " + ", ".join(warm_protected))
    if prunable:
        lines.append("prunable:")
        for path in sorted(prunable):
            lines.append(f"  {path.name}")
        lines.append("safe prune suggestions:")
        for path in sorted(prunable):
            lines.append(f"  {powershell_remove_item_command(path)}")
    return lines


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
                warm_lane_rank(path.name),
                -lane_mtime(path),
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
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> list[Path]:
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
    prunable: list[Path] = []
    size_candidates: list[Path] = []

    for path in lane_dirs:
        if is_timestamped_lane(path.name):
            prunable.append(path)
            continue
        if (
            max_age_days is not None
            and now - snapshot.lane_mtime(path) > max_age_days * 86400
        ):
            prunable.append(path)
            continue
        if keep_warm_per_base > 0 and path.name not in protected:
            prunable.append(path)
            continue
        if max_lane_bytes is not None:
            size_candidates.append(path)
            continue
        if keep_warm_per_base <= 0 and max_age_days is None and max_lane_bytes is None:
            prunable.append(path)

    if max_lane_bytes is not None and size_candidates:
        for path, (size_bytes, _errors) in snapshot.lane_sizes(
            size_candidates,
            size_workers=size_workers,
            lane_size=lane_size,
        ).items():
            if size_bytes > max_lane_bytes:
                prunable.append(path)

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
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> list[Path]:
    snapshot = snapshot or BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
        lane_mtime=lane_mtime,
    )
    lane_root = cargo_lanes_root(repo_root)
    resolved_lane_root = lane_root.resolve()
    removed: list[Path] = []
    for path in prunable_lane_dirs(
        repo_root=repo_root,
        processes=snapshot.processes,
        snapshot=snapshot,
        keep_warm_per_base=keep_warm_per_base,
        max_age_days=max_age_days,
        max_lane_bytes=max_lane_bytes,
        now_timestamp=now_timestamp,
        lane_mtime=lane_mtime,
        lane_size=lane_size,
        size_workers=size_workers,
    ):
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
            try:
                if is_indirect_directory(path):
                    print(
                        f"warning: lane became an indirect path before prune: {path}",
                        file=sys.stderr,
                    )
                    continue
                if cargo_lock_is_busy(path) or lane_active_lock_is_held(path):
                    continue
                # Lanes routinely contain read-only files (registry-cache
                # copies in build OUT_DIRs); bare rmtree would abort partway.
                remove_tree_allow_readonly(path)
            except OSError as exc:
                if not cargo_lock_is_busy(path) and not lane_active_lock_is_held(path):
                    print(
                        f"warning: failed to prune lane {path}: {exc}",
                        file=sys.stderr,
                    )
                continue
        removed.append(path)
    return removed


def prune_stale_lanes_plan(
    *,
    repo_root: Path = REPO_ROOT,
    processes: Sequence[RustProcess] | None = None,
    snapshot: BuildStatusSnapshot | None = None,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    now_timestamp: float | None = None,
    lane_mtime: Callable[[Path], float] | None = None,
    lane_size: Callable[[Path], tuple[int, int]] | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> dict[str, object]:
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
    removed: list[Path] = []
    for path in stray_cargo_target_dirs(repo_root=repo_root):
        if is_indirect_directory(path):
            print(f"warning: skipping indirect target path: {path}", file=sys.stderr)
            continue
        resolved_path = path.resolve()
        if resolved_path.parent != resolved_target_root:
            raise ValueError(
                f"refusing to prune stray target outside {resolved_target_root}: {resolved_path}"
            )
        if cargo_lock_is_busy(resolved_path):
            continue
        if not dry_run:
            try:
                if is_indirect_directory(path) or cargo_lock_is_busy(path):
                    continue
                remove_tree_allow_readonly(path)
            except OSError:
                if cargo_lock_is_busy(path):
                    continue
                continue
        removed.append(path)
    return removed


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
    include_disk_report: bool = True,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> str:
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
        size_workers=size_workers,
    )
    removed_strays = prune_stray_cargo_target_dirs(
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
    if removed:
        for path in removed:
            lines.append(f"{action}: {path}")
    else:
        lines.append("no stale lanes to prune")
    if removed_strays:
        for path in removed_strays:
            lines.append(f"{action} stray target: {path}")
    else:
        lines.append("no stray cargo target dirs to prune")
    if include_disk_report:
        lines.extend(
            target_disk_report_lines(repo_root=repo_root, warn_bytes=warn_bytes)
        )
    return "\n".join(lines)


def target_optimize_report(
    *,
    repo_root: Path = REPO_ROOT,
    dry_run: bool = False,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    include_prune_disk_report: bool = False,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> str:
    snapshot = BuildStatusSnapshot.collect(repo_root=repo_root)
    return "\n".join(
        [
            build_doctor_report(repo_root=repo_root, snapshot=snapshot),
            prune_stale_lanes_report(
                repo_root=repo_root,
                snapshot=snapshot,
                dry_run=dry_run,
                warn_bytes=warn_bytes,
                keep_warm_per_base=keep_warm_per_base,
                max_age_days=max_age_days,
                max_lane_bytes=max_lane_bytes,
                include_disk_report=include_prune_disk_report,
                size_workers=size_workers,
            ),
        ]
    )


def powershell_remove_item_command(path: Path) -> str:
    escaped = str(path).replace("'", "''")
    return f"Remove-Item -Recurse -LiteralPath '{escaped}'"


def warn_bytes_from_gib(warn_gib: float) -> int:
    return int(warn_gib * BYTES_PER_GIB)


def bytes_from_gib(gib: float | None) -> int | None:
    if gib is None:
        return None
    return int(gib * BYTES_PER_GIB)


def max_lane_bytes_from_args(args: argparse.Namespace) -> int | None:
    if args.max_lane_bytes is not None:
        return args.max_lane_bytes
    return bytes_from_gib(args.max_lane_gib)


def positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be > 0")
    return parsed


def non_negative_int(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be >= 0")
    return parsed


def add_prune_arguments(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--warn-gib", type=positive_float, default=50.0)
    parser.add_argument(
        "--keep-warm-per-base",
        type=non_negative_int,
        default=DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    )
    parser.add_argument(
        "--max-age-days", type=positive_float, default=DEFAULT_PRUNE_MAX_AGE_DAYS
    )
    parser.add_argument("--max-lane-gib", type=positive_float)
    parser.add_argument("--max-lane-bytes", type=positive_int)
    parser.add_argument(
        "--size-workers", type=positive_int, default=DEFAULT_LANE_SIZE_WORKERS
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Prune all idle lanes instead of keeping warm lanes or applying the default age window.",
    )


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
    disk_parser.add_argument("--warn-gib", type=positive_float, default=50.0)
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
    args = parser.parse_args(argv)
    keep_warm_per_base = getattr(args, "keep_warm_per_base", None)
    max_age_days = getattr(args, "max_age_days", None)
    if getattr(args, "all", False):
        keep_warm_per_base = 0
        max_age_days = None

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
                include_prune_disk_report=args.include_prune_disk_report,
                size_workers=args.size_workers,
            )
        )
    else:
        parser.error(f"unknown command {args.command}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
