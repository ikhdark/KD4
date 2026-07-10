#!/usr/bin/env python3

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import shutil
import tomllib
from typing import Callable
from typing import Mapping
from typing import Sequence
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from scripts.rust_build_status import BuildStatusSnapshot
    from scripts.rust_build_status import RustProcess


REPO_ROOT = Path(__file__).resolve().parent.parent
BYTES_PER_KIB = 1024
BYTES_PER_MIB = BYTES_PER_KIB * 1024
BYTES_PER_GIB = BYTES_PER_MIB * 1024
DEFAULT_TARGET_WARN_BYTES = 50 * BYTES_PER_GIB
DEFAULT_LANE_SIZE_WORKERS = 2
MAX_LANE_SIZE_WORKERS = 4
DEFAULT_PRUNE_KEEP_WARM_PER_BASE = 1
DEFAULT_PRUNE_MAX_AGE_DAYS = 7.0
WINDOWS_MSVC_TARGETS = (
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)


def _runtime():
    from scripts import rust_build_status

    return rust_build_status


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
    strays = _runtime().stray_cargo_target_dirs(repo_root=repo_root)
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
    snapshot = snapshot or _runtime().BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    processes = snapshot.processes
    shared = _runtime().shared_target_rust_processes(
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
    snapshot = _runtime().BuildStatusSnapshot.collect(
        repo_root=repo_root, processes=processes
    )
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
    snapshot = snapshot or _runtime().BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    lane_root = _runtime().cargo_lanes_root(repo_root)
    existing_names = {path.name for path in snapshot.lane_dirs}
    active_lanes = snapshot.active_lanes
    active_existing = sorted(active_lanes & existing_names)
    active_external = sorted(active_lanes - existing_names)
    stale = snapshot.stale_lanes
    protected = _runtime().protected_warm_lane_names(
        stale,
        keep_warm_per_base=DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
        lane_mtime=snapshot.lane_mtime,
    )
    prunable = set(
        _runtime().prunable_lane_dirs(
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
    snapshot = _runtime().BuildStatusSnapshot.collect(repo_root=repo_root)
    return "\n".join(
        [
            build_doctor_report(repo_root=repo_root, snapshot=snapshot),
            _runtime().prune_stale_lanes_report(
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
