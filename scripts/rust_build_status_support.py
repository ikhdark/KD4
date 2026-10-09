#!/usr/bin/env python3

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import math
import os
from pathlib import Path
import shutil
import stat
import tomllib
from typing import Callable
from typing import Mapping
from typing import Sequence
from typing import TYPE_CHECKING

from scripts.rust_tool_env import local_rust_env

if TYPE_CHECKING:
    from scripts.rust_build_status import BuildStatusSnapshot
    from scripts.rust_build_status import RustProcess


REPO_ROOT = Path(__file__).resolve().parent.parent
BYTES_PER_KIB = 1024
BYTES_PER_MIB = BYTES_PER_KIB * 1024
BYTES_PER_GIB = BYTES_PER_MIB * 1024
DEFAULT_TARGET_WARN_BYTES = 250 * BYTES_PER_GIB
DEFAULT_LANE_SIZE_WORKERS = 2
MAX_LANE_SIZE_WORKERS = 4
DEFAULT_PRUNE_KEEP_WARM_PER_BASE = 1
DEFAULT_PRUNE_MAX_AGE_DAYS = 7.0
# Cargo hardlinks uplifted binaries, PDBs and build scripts. Checking link
# identity only for files of at least 1 MiB removed 94% of the double-counted
# bytes in a 43 GiB target for about 0.4 s instead of 11 s for every file.
HARDLINK_CHECK_MIN_BYTES = BYTES_PER_MIB
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


def directory_size_bytes(
    path: Path,
    *,
    exclude: Path | None = None,
    subtree_sizes: dict[Path, tuple[int, int]] | None = None,
    partition_sizes: dict[Path, tuple[int, int]] | None = None,
    partition_exclude: Path | None = None,
) -> tuple[int, int]:
    """Measure a tree, requested subtrees, and optional non-lane child partitions.

    Partitions match target_non_lane_size_bytes: hardlinks are deduplicated per
    immediate child, while root-level files are counted individually.
    """
    if not path.exists():
        return 0, 0

    subtrees = {
        os.path.normcase(os.fspath(child.resolve())): child
        for child in (subtree_sizes or {})
    }
    totals = {scope: [0, 0] for scope in (None, *subtrees.values())}
    linked = {scope: set() for scope in totals}
    root = os.fspath(path.resolve())
    excluded_partition = (
        os.path.normcase(os.fspath(partition_exclude.resolve()))
        if partition_exclude is not None
        else None
    )
    stack = [(root, None, None)]
    while stack:
        current, subtree, partition = stack.pop()
        if partition_sizes is not None:
            if os.path.normcase(current) == excluded_partition:
                partition = False
            elif partition is None:
                partition = ("partition", current)
                totals[partition] = [0, 0]
                linked[partition] = set()
        if subtrees:
            subtree = subtrees.get(os.path.normcase(current), subtree)
        scopes = (None,) if subtree is None else (None, subtree)
        if partition:
            scopes += (partition,)
        try:
            with os.scandir(current) as entries:
                for entry in entries:
                    try:
                        if exclude is not None and Path(entry.path) == exclude:
                            continue
                        if _is_reparse_point(entry):
                            continue
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(
                                (
                                    entry.path,
                                    subtree,
                                    None
                                    if current == root and partition is not False
                                    else partition,
                                )
                            )
                        elif entry.is_file(follow_symlinks=False):
                            size = entry.stat(follow_symlinks=False).st_size
                            key = None
                            if size >= HARDLINK_CHECK_MIN_BYTES:
                                # scandir leaves st_ino/st_nlink zero on Windows.
                                try:
                                    identity = os.stat(
                                        entry.path, follow_symlinks=False
                                    )
                                except OSError:
                                    identity = None
                                if identity is not None and identity.st_nlink > 1:
                                    key = (identity.st_dev, identity.st_ino)
                            # A hardlink shared across lanes counts once globally
                            # but once in each lane's independent size budget.
                            for scope in scopes:
                                if key is not None and not (
                                    partition and scope == partition and current == root
                                ):
                                    if key in linked[scope]:
                                        continue
                                    linked[scope].add(key)
                                totals[scope][0] += size
                    except OSError:
                        for scope in scopes:
                            totals[scope][1] += 1
        except OSError:
            for scope in scopes:
                totals[scope][1] += 1
    if subtree_sizes is not None:
        subtree_sizes.update(
            (child, (totals[child][0], totals[child][1])) for child in subtrees.values()
        )
    if partition_sizes is not None:
        partition_sizes.update(
            (Path(scope[1]), (values[0], values[1]))
            for scope, values in totals.items()
            if isinstance(scope, tuple)
        )
    return totals[None][0], totals[None][1]


def _is_reparse_point(entry: os.DirEntry[str]) -> bool:
    junction_probe = getattr(entry, "is_junction", None)
    if callable(junction_probe) and junction_probe():
        return True
    attributes = getattr(entry.stat(follow_symlinks=False), "st_file_attributes", 0)
    return bool(attributes & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0))


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


def target_non_lane_size_bytes(
    *,
    repo_root: Path = REPO_ROOT,
    lane_root: Path | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> tuple[int, int]:
    target_root = repo_root / "codex-rs" / "target"
    if not target_root.exists():
        return 0, 0

    resolved_lane_root = (
        (target_root / "lanes").resolve() if lane_root is None else lane_root.resolve()
    )
    total = 0
    errors = 0
    directories: list[Path] = []
    try:
        with os.scandir(target_root) as entries:
            for entry in entries:
                try:
                    if _is_reparse_point(entry):
                        continue
                    path = Path(entry.path)
                    if path.resolve() == resolved_lane_root:
                        continue
                    if entry.is_dir(follow_symlinks=False):
                        directories.append(path)
                    elif entry.is_file(follow_symlinks=False):
                        total += entry.stat(follow_symlinks=False).st_size
                except OSError:
                    errors += 1
    except OSError:
        return 0, 1

    for size_bytes, child_errors in directory_sizes_bytes(
        directories,
        size_workers=size_workers,
        size_func=lambda path: directory_size_bytes(path, exclude=resolved_lane_root),
    ).values():
        total += size_bytes
        errors += child_errors
    return total, errors


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
    snapshot: BuildStatusSnapshot | None = None,
) -> list[str]:
    target_root = repo_root / "codex-rs" / "target"
    lines = [f"target root: {target_root}"]
    if not target_root.exists():
        lines.append("target disk: missing")
        return lines

    size_bytes, errors = (
        snapshot.target_size()
        if snapshot is not None
        else directory_size_bytes(target_root)
    )
    lines.append(f"target disk: {format_bytes(size_bytes)}")
    lines.append(f"target warning threshold: {format_bytes(warn_bytes)}")
    if errors:
        lines.append(f"target disk scan errors: {errors}")
    if size_bytes > warn_bytes:
        lines.append(
            "target disk warning: codex-rs/target is above the warning threshold. "
            "Automatic lane GC enforces only lane age and warm-lane limits unless "
            "CODEX_CARGO_LANE_MAX_TOTAL_BYTES or CODEX_CARGO_TARGET_MAX_TOTAL_BYTES "
            "is set. Preview a size-bounded cleanup with `just target-prune --dry-run "
            "--max-total-target-gib <GiB>`; it removes only lanes proven idle by "
            "their locks and never shared profiles or stray targets. This report "
            "cannot prove that nothing is using codex-rs/target, so it is not a "
            "basis for deleting that directory."
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
    include_disk: bool = True,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
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
        env_name = f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"
        lines.append(
            f"MSVC linker config {target}: {msvc_linkers.get(target) or '(unset)'}"
        )
        lines.append(f"MSVC linker env {env_name}: {env.get(env_name) or '(unset)'}")
    # Just recipes and run-lane add these to the values above before Cargo runs.
    additions = local_rust_env(env, repo_root=repo_root, which=tool_lookup)
    lines.append(
        "just/run-lane env additions: "
        + (
            "; ".join(f"{name}={value}" for name, value in sorted(additions.items()))
            or "(none)"
        )
    )

    if snapshot.process_scan_error is not None:
        lines.append(f"active Rust processes: unknown ({snapshot.process_scan_error})")
    else:
        lines.append(
            f"active Rust processes: {len(processes)} total, {len(lane_processes)} lane, "
            f"{len(shared)} without lane"
        )
    if shared:
        lines.append(
            "Rust jobs without a lane are running; their target dir is not verified "
            "(codex-rs/target or another workspace). For codex-rs builds prefer "
            "`just test-lane-fast <lane> ...`"
        )
    active_lanes = sorted(snapshot.active_lanes - snapshot.quarantined_lanes)
    if active_lanes:
        lines.append(
            "active lanes: " + ", ".join(lane for lane in active_lanes if lane)
        )

    if include_disk:
        lines.extend(
            target_disk_report_lines(
                repo_root=repo_root, warn_bytes=warn_bytes, snapshot=snapshot
            )
        )
    else:
        lines.append("target disk: not scanned (use `doctor --include-disk` or `disk`)")
    lines.extend(
        lane_report_lines(
            repo_root=repo_root,
            processes=processes,
            snapshot=snapshot,
            keep_warm_per_base=keep_warm_per_base,
            max_age_days=max_age_days,
            max_lane_bytes=max_lane_bytes,
            max_total_lane_bytes=max_total_lane_bytes,
            max_total_target_bytes=max_total_target_bytes,
            size_workers=size_workers,
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
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> list[str]:
    snapshot = snapshot or _runtime().BuildStatusSnapshot.collect(
        repo_root=repo_root,
        processes=processes,
    )
    lane_root = _runtime().cargo_lanes_root(repo_root)
    # Match on-disk names case-insensitively, as stale detection does.
    existing_by_folded = {
        path.name.casefold(): path.name for path in snapshot.lane_dirs
    }
    active_existing = sorted(
        {
            existing_by_folded[name.casefold()]
            for name in snapshot.active_lanes
            if name.casefold() in existing_by_folded
        }
        - snapshot.quarantined_lanes
    )
    active_external = sorted(
        name
        for name in snapshot.active_lanes
        if name.casefold() not in existing_by_folded
    )
    stale = snapshot.stale_lanes
    protected = _runtime().protected_warm_lane_names(
        stale,
        keep_warm_per_base=keep_warm_per_base,
        lane_mtime=snapshot.lane_mtime,
    )
    prune_refusal = None
    try:
        prunable = set(
            _runtime().prunable_lane_dirs(
                repo_root=repo_root,
                processes=snapshot.processes,
                snapshot=snapshot,
                keep_warm_per_base=keep_warm_per_base,
                max_age_days=max_age_days,
                max_lane_bytes=max_lane_bytes,
                max_total_lane_bytes=max_total_lane_bytes,
                max_total_target_bytes=max_total_target_bytes,
                size_workers=size_workers,
            )
        )
    except (
        _runtime().RustProcessScanError,
        _runtime().CargoLanesRootValidationError,
    ) as exc:
        prunable, prune_refusal = set(), str(exc)

    lines = ["lane report", f"lane root: {lane_root}"]
    lines.append(
        "active: " + (", ".join(active_existing) if active_existing else "(none)")
    )
    if snapshot.quarantined_lanes:
        lines.append(
            "quarantined (process cleanup unconfirmed; not pruned or reused until "
            ".lane-cleanup-unconfirmed is removed after checking for leftover "
            "processes): " + ", ".join(sorted(snapshot.quarantined_lanes))
        )
    if active_external:
        lines.append("active without directory: " + ", ".join(active_external))
    lines.append(
        "stale: " + (", ".join(path.name for path in stale) if stale else "(none)")
    )
    warm_protected = sorted(
        path.name for path in stale if path.name in protected and path not in prunable
    )
    if warm_protected:
        lines.append("warm-protected: " + ", ".join(warm_protected))
    trash = _runtime().lane_trash_dirs(lane_root)
    if trash:
        lines.append(
            "pending deletion (renamed by an earlier prune): "
            + ", ".join(path.name for path in trash)
        )
    if prune_refusal is not None:
        lines.append(f"pruning refused: {prune_refusal}")
    elif prunable:
        lines.append("prunable:")
        for path in sorted(prunable):
            lines.append(f"  {path.name}")
        if (
            keep_warm_per_base == DEFAULT_PRUNE_KEEP_WARM_PER_BASE
            and max_age_days == DEFAULT_PRUNE_MAX_AGE_DAYS
            and max_lane_bytes is None
            and max_total_lane_bytes is None
            and max_total_target_bytes is None
        ):
            lines.append("safe prune suggestions:")
            lines.append("  just target-prune")
    return lines


def target_optimize_report(
    *,
    repo_root: Path = REPO_ROOT,
    dry_run: bool = False,
    warn_bytes: int = DEFAULT_TARGET_WARN_BYTES,
    keep_warm_per_base: int = DEFAULT_PRUNE_KEEP_WARM_PER_BASE,
    max_age_days: float | None = DEFAULT_PRUNE_MAX_AGE_DAYS,
    max_lane_bytes: int | None = None,
    max_total_lane_bytes: int | None = None,
    max_total_target_bytes: int | None = None,
    include_prune_disk_report: bool = False,
    size_workers: int = DEFAULT_LANE_SIZE_WORKERS,
) -> str:
    snapshot = _runtime().BuildStatusSnapshot.collect(repo_root=repo_root)
    return "\n".join(
        [
            build_doctor_report(
                repo_root=repo_root,
                snapshot=snapshot,
                warn_bytes=warn_bytes,
                keep_warm_per_base=keep_warm_per_base,
                max_age_days=max_age_days,
                max_lane_bytes=max_lane_bytes,
                max_total_lane_bytes=max_total_lane_bytes,
                max_total_target_bytes=max_total_target_bytes,
                size_workers=size_workers,
            ),
            _runtime().prune_stale_lanes_report(
                repo_root=repo_root,
                snapshot=snapshot,
                dry_run=dry_run,
                warn_bytes=warn_bytes,
                keep_warm_per_base=keep_warm_per_base,
                max_age_days=max_age_days,
                max_lane_bytes=max_lane_bytes,
                max_total_lane_bytes=max_total_lane_bytes,
                max_total_target_bytes=max_total_target_bytes,
                include_disk_report=include_prune_disk_report,
                size_workers=size_workers,
            ),
        ]
    )


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


def max_total_lane_bytes_from_args(args: argparse.Namespace) -> int | None:
    if args.max_total_lane_bytes is not None:
        return args.max_total_lane_bytes
    return bytes_from_gib(args.max_total_lane_gib)


def max_total_target_bytes_from_args(args: argparse.Namespace) -> int | None:
    if args.max_total_target_bytes is not None:
        return args.max_total_target_bytes
    return bytes_from_gib(args.max_total_target_gib)


def positive_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed):
        raise argparse.ArgumentTypeError("must be finite")
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
    parser.add_argument("--warn-gib", type=positive_float, default=250.0)
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
        "--max-total-lane-gib",
        type=positive_float,
        help="Evict least-recently-used inactive lanes until all lanes fit this aggregate GiB ceiling.",
    )
    parser.add_argument("--max-total-lane-bytes", type=positive_int)
    parser.add_argument(
        "--max-total-target-gib",
        type=positive_float,
        help=(
            "Evict least-recently-used inactive lanes until the lane pool plus "
            "other codex-rs/target content fits this aggregate GiB ceiling."
        ),
    )
    parser.add_argument("--max-total-target-bytes", type=positive_int)
    parser.add_argument(
        "--size-workers", type=positive_int, default=DEFAULT_LANE_SIZE_WORKERS
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Prune all idle lanes instead of keeping warm lanes or applying the default age window.",
    )
