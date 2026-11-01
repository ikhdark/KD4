#!/usr/bin/env python3
"""Archive extraction and cache helpers for stage_npm_packages."""

from __future__ import annotations

# Support direct execution (including python -I) from any working directory.
if __package__ in (None, ""):
    import sys as _sys
    from pathlib import Path as _Path

    _sys.path.insert(0, str(_Path(__file__).resolve().parents[2]))


from concurrent.futures import as_completed
from scripts.common.process_owner import (
    OwnedThreadPoolExecutor as ThreadPoolExecutor,
    check_operation,
)
from scripts.common.generated_output_lock import exclusive_file_lock
import hashlib
import os
from pathlib import Path
import shutil
import stat
import sys
import tarfile
import threading
from typing import Sequence
import uuid

from scripts.codex_package.layout import sha256_file as file_sha256

COMPLETE_MARKER = ".complete"
DEFAULT_GHA_DOWNLOAD_WORKERS = 8


def _gha_enabled() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


def worker_count_for(item_count: int, requested: int | None = None) -> int:
    item_count = max(1, item_count)
    if requested is not None:
        if requested <= 0:
            raise ValueError("requested worker count must be > 0")
        return min(item_count, requested)
    return min(item_count, max(1, (os.cpu_count() or 1)))


def download_worker_count_for(item_count: int, requested: int | None = None) -> int:
    if requested is not None:
        return worker_count_for(item_count, requested)
    if _gha_enabled():
        return min(item_count, DEFAULT_GHA_DOWNLOAD_WORKERS)
    return worker_count_for(item_count)


def install_codex_package_archives(
    artifacts_dir: Path,
    vendor_dir: Path,
    targets: Sequence[str],
    extracted_cache_dir: Path | None = None,
    *,
    vendor_copy_mode: str = "auto",
) -> None:
    if not targets:
        return

    print(
        "Installing Codex package archives for targets: " + ", ".join(targets),
        flush=True,
    )
    max_workers = min(len(targets), max(1, (os.cpu_count() or 1)))
    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        futures = {
            executor.submit(
                install_single_codex_package_archive,
                artifacts_dir,
                vendor_dir,
                target,
                extracted_cache_dir,
                vendor_copy_mode=vendor_copy_mode,
            ): target
            for target in targets
        }
        for future in as_completed(futures):
            installed_path = future.result()
            print(f"  installed {installed_path}", flush=True)


def install_single_codex_package_archive(
    artifacts_dir: Path,
    vendor_dir: Path,
    target: str,
    extracted_cache_dir: Path | None = None,
    *,
    vendor_copy_mode: str = "auto",
) -> Path:
    archive_name = f"codex-package-{target}.tar.gz"
    artifact_subdir = artifact_dir_for_target(
        artifacts_dir, target, expected_names=(archive_name,)
    )
    archive_path = artifact_subdir / archive_name

    dest_dir = vendor_dir / target
    vendor_dir.mkdir(parents=True, exist_ok=True)
    temp_dir = vendor_dir / f".{target}.{uuid.uuid4().hex}.tmp"
    backup_dir = vendor_dir / f".{target}.{uuid.uuid4().hex}.old"

    try:
        temp_dir.mkdir(parents=True)
        if extracted_cache_dir is None:
            extract_tar_data(archive_path, temp_dir)
        else:
            cached_dir = cached_codex_package_archive(
                archive_path,
                target,
                extracted_cache_dir,
            )
            materialize_cached_tree(cached_dir, temp_dir, vendor_copy_mode)

        if dest_dir.exists():
            dest_dir.replace(backup_dir)
        temp_dir.replace(dest_dir)
    except Exception:
        if not dest_dir.exists() and backup_dir.exists():
            backup_dir.replace(dest_dir)
        raise
    finally:
        if temp_dir.exists():
            shutil.rmtree(temp_dir, ignore_errors=True)

    if backup_dir.exists():
        try:
            shutil.rmtree(backup_dir)
        except OSError as error:
            print(
                f"warning: installed {dest_dir}, but could not remove backup "
                f"{backup_dir}: {error}",
                file=sys.stderr,
            )

    return dest_dir


def materialize_cached_tree(
    cached_dir: Path,
    dest_dir: Path,
    vendor_copy_mode: str,
) -> None:
    if vendor_copy_mode in {"auto", "hardlink"}:
        try:
            hardlink_tree(cached_dir, dest_dir, ignored_names={COMPLETE_MARKER})
            return
        except OSError:
            if vendor_copy_mode == "hardlink":
                raise
            shutil.rmtree(dest_dir, ignore_errors=True)
            dest_dir.mkdir(parents=True, exist_ok=True)

    shutil.copytree(
        cached_dir,
        dest_dir,
        dirs_exist_ok=True,
        ignore=lambda directory, names: (
            {COMPLETE_MARKER} if Path(directory) == cached_dir else set()
        ),
    )


def hardlink_tree(
    src_dir: Path,
    dest_dir: Path,
    *,
    ignored_names: set[str],
) -> None:
    dest_dir.mkdir(parents=True, exist_ok=True)
    for src in src_dir.iterdir():
        if src.name in ignored_names:
            continue

        dest = dest_dir / src.name
        if src.is_dir():
            hardlink_tree(src, dest, ignored_names=set())
        elif src.is_file():
            os.link(src, dest)
        else:
            shutil.copy2(src, dest)


def cached_codex_package_archive(
    archive_path: Path,
    target: str,
    cache_root: Path,
) -> Path:
    cache_root.mkdir(parents=True, exist_ok=True)
    archive_sha256 = file_sha256(archive_path)
    cache_dir = cache_root / f"{target}-{archive_sha256}"
    marker_path = cache_dir / COMPLETE_MARKER
    if extracted_cache_is_complete(cache_dir, marker_path, archive_sha256):
        return cache_dir

    lock_path = cache_root / f".{cache_dir.name}.lock"
    with exclusive_file_lock(lock_path):
        if extracted_cache_is_complete(cache_dir, marker_path, archive_sha256):
            return cache_dir

        temp_dir = (
            cache_root / f".{cache_dir.name}.tmp-{os.getpid()}-{threading.get_ident()}"
        )
        shutil.rmtree(temp_dir, ignore_errors=True)
        temp_dir.mkdir(parents=True, exist_ok=True)
        try:
            extract_tar_data(archive_path, temp_dir)
            tree_sha256 = cache_tree_digest(temp_dir)
            (temp_dir / COMPLETE_MARKER).write_text(
                "version=3\n"
                f"source={archive_path}\n"
                f"archive_sha256={archive_sha256}\n"
                f"tree_sha256={tree_sha256}\n",
                encoding="utf-8",
            )
            if cache_dir.exists():
                shutil.rmtree(cache_dir)
            temp_dir.rename(cache_dir)
        except Exception:
            shutil.rmtree(temp_dir, ignore_errors=True)
            raise

    return cache_dir


def extracted_cache_is_complete(
    cache_dir: Path, marker_path: Path, archive_sha256: str
) -> bool:
    try:
        marker = marker_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return False
    expected_tree = next(
        (
            line.removeprefix("tree_sha256=")
            for line in marker.splitlines()
            if line.startswith("tree_sha256=")
        ),
        None,
    )
    expected_archive = next(
        (
            line.removeprefix("archive_sha256=")
            for line in marker.splitlines()
            if line.startswith("archive_sha256=")
        ),
        None,
    )
    if (
        "version=3\n" not in marker
        or expected_tree is None
        or expected_archive != archive_sha256
    ):
        return False
    try:
        return cache_tree_digest(cache_dir) == expected_tree
    except (OSError, RuntimeError):
        return False


def cache_tree_digest(root: Path) -> str:
    digest = hashlib.sha256()
    digest.update(b"codex-extracted-archive-cache-v3\0")
    for path in sorted(
        root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()
    ):
        if path == root / COMPLETE_MARKER:
            continue
        relative = path.relative_to(root).as_posix().encode("utf-8")
        if path.is_symlink():
            raise RuntimeError(f"cache contains a symlink: {path}")
        kind = b"d" if path.is_dir() else b"f" if path.is_file() else None
        if kind is None:
            raise RuntimeError(f"cache contains a special entry: {path}")
        digest.update(kind)
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        mode = stat.S_IMODE(path.lstat().st_mode) & 0o777
        digest.update(mode.to_bytes(4, "big"))
        if kind == b"f":
            digest.update(bytes.fromhex(file_sha256(path)))
    return digest.hexdigest()


def extract_tar_data(archive_path: Path, dest_dir: Path) -> None:
    with tarfile.open(archive_path, "r:gz") as archive:
        try:
            archive.extractall(dest_dir, filter="data")
        except TypeError:
            validate_tar_members_for_legacy_python(archive, dest_dir)
            archive.extractall(dest_dir)


def validate_tar_members_for_legacy_python(
    archive: tarfile.TarFile, dest_dir: Path
) -> None:
    dest_root = dest_dir.resolve()
    for member in archive.getmembers():
        member_path = (dest_dir / member.name).resolve()
        if not member_path.is_relative_to(dest_root):
            raise RuntimeError(f"unsafe archive member path: {member.name}")
        if member.issym() or member.islnk():
            raise RuntimeError(
                f"archive links require Python tarfile data_filter support: {member.name}"
            )
        if not (member.isfile() or member.isdir()):
            raise RuntimeError(
                "archive special files require Python tarfile data_filter "
                f"support: {member.name}"
            )


def artifact_dir_for_target(
    artifacts_dir: Path,
    target: str,
    *,
    expected_names: Sequence[str],
) -> Path:
    candidates = [
        (artifacts_dir / target, expected_names),
        (artifacts_dir / f"{target}-unsigned", expected_names),
    ]
    checked = [directory / name for directory, names in candidates for name in names]
    matches = [
        directory
        for directory, names in candidates
        if any((directory / name).is_file() for name in names)
    ]
    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        raise RuntimeError(
            f"Ambiguous artifact directories for {target}: "
            + ", ".join(str(path) for path in matches)
        )
    raise FileNotFoundError(
        f"Expected artifact for {target} not found; checked: "
        + ", ".join(str(path) for path in checked)
    )


