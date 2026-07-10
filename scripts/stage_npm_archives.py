#!/usr/bin/env python3
"""Archive extraction and cache helpers for stage_npm_packages."""

from __future__ import annotations

from concurrent.futures import as_completed
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import threading
import time
from typing import Sequence
from typing import TYPE_CHECKING
import uuid

from scripts.codex_package.targets import BINARY_TARGETS

if TYPE_CHECKING:
    from scripts.stage_npm_packages import BinaryComponent


COMPLETE_MARKER = ".complete"
LOCK_POLL_SECONDS = 0.1
LOCK_STALE_SECONDS = 60 * 60
DEFAULT_GHA_DOWNLOAD_WORKERS = 8


def _runtime():
    from scripts import stage_npm_packages

    return stage_npm_packages


def _gha_enabled() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


@contextmanager
def exclusive_file_lock(lock_path: Path):
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd: int | None = None
    lock_identity: tuple[int, int] | None = None
    while fd is None:
        try:
            fd = os.open(str(lock_path), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(
                fd,
                f"pid={os.getpid()} thread={threading.get_ident()}\n".encode("utf-8"),
            )
            stat_result = os.fstat(fd)
            lock_identity = (stat_result.st_dev, stat_result.st_ino)
        except FileExistsError:
            try:
                lock_stat = lock_path.stat()
                owner_pid = lock_owner_pid(lock_path)
                owner_is_live = owner_pid is not None and _runtime().process_is_running(
                    owner_pid
                )
                if (
                    time.time() - lock_stat.st_mtime > LOCK_STALE_SECONDS
                    and not owner_is_live
                ):
                    lock_path.unlink()
            except (FileNotFoundError, PermissionError):
                # On Windows, unlinking a lock whose holder still has the fd
                # open raises PermissionError — keep waiting, don't crash.
                pass
            time.sleep(LOCK_POLL_SECONDS)

    try:
        yield
    finally:
        if fd is not None:
            os.close(fd)
        try:
            lock_stat = lock_path.stat()
            if lock_identity == (lock_stat.st_dev, lock_stat.st_ino):
                lock_path.unlink()
        except FileNotFoundError:
            pass


def lock_owner_pid(lock_path: Path) -> int | None:
    try:
        fields = lock_path.read_text(encoding="utf-8").split()
    except (OSError, UnicodeError):
        return None
    for field in fields:
        if field.startswith("pid="):
            try:
                return int(field.removeprefix("pid="))
            except ValueError:
                return None
    return None


def process_is_running(pid: int) -> bool:
    if pid <= 0:
        return False
    if os.name == "nt":
        # Python's Windows os.kill implementation does not offer a harmless
        # signal-0 probe. The open lock handle denies deletion on Windows, so
        # let the unlink attempt itself distinguish a live owner.
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return True
    return True


def worker_count_for(item_count: int, requested: int | None = None) -> int:
    item_count = max(1, item_count)
    if requested is not None:
        return max(1, min(item_count, requested))
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
    artifact_subdir = artifact_dir_for_target(artifacts_dir, target)
    archive_path = artifact_subdir / f"codex-package-{target}.tar.gz"
    if not archive_path.exists():
        raise FileNotFoundError(f"Expected package archive not found: {archive_path}")

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
        if backup_dir.exists():
            shutil.rmtree(backup_dir)
    except Exception:
        if not dest_dir.exists() and backup_dir.exists():
            backup_dir.replace(dest_dir)
        raise
    finally:
        if temp_dir.exists():
            shutil.rmtree(temp_dir, ignore_errors=True)
        if backup_dir.exists():
            shutil.rmtree(backup_dir, ignore_errors=True)

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
        ignore=shutil.ignore_patterns(COMPLETE_MARKER),
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
            hardlink_tree(src, dest, ignored_names=ignored_names)
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
    stat = archive_path.stat()
    cache_dir = cache_root / f"{target}-{stat.st_size}-{stat.st_mtime_ns}"
    marker_path = cache_dir / COMPLETE_MARKER
    if marker_path.is_file():
        return cache_dir

    lock_path = cache_root / f".{cache_dir.name}.lock"
    with exclusive_file_lock(lock_path):
        if marker_path.is_file():
            return cache_dir

        temp_dir = (
            cache_root / f".{cache_dir.name}.tmp-{os.getpid()}-{threading.get_ident()}"
        )
        shutil.rmtree(temp_dir, ignore_errors=True)
        temp_dir.mkdir(parents=True, exist_ok=True)
        try:
            extract_tar_data(archive_path, temp_dir)
            (temp_dir / COMPLETE_MARKER).write_text(
                f"source={archive_path}\n"
                f"size={stat.st_size}\n"
                f"mtime_ns={stat.st_mtime_ns}\n",
                encoding="utf-8",
            )
            if cache_dir.exists():
                shutil.rmtree(cache_dir)
            temp_dir.rename(cache_dir)
        except Exception:
            shutil.rmtree(temp_dir, ignore_errors=True)
            raise

    return cache_dir


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
        if not is_relative_to(member_path, dest_root):
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


def is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def install_binary_components(
    artifacts_dir: Path,
    vendor_dir: Path,
    selected_components: Sequence[BinaryComponent],
    targets: Sequence[str] = BINARY_TARGETS,
) -> None:
    for component in selected_components:
        component_targets = list(targets)

        print(
            f"Installing {component.binary_basename} binaries for targets: "
            + ", ".join(component_targets),
            flush=True,
        )
        max_workers = min(len(component_targets), max(1, (os.cpu_count() or 1)))
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = {
                executor.submit(
                    install_single_binary,
                    artifacts_dir,
                    vendor_dir,
                    target,
                    component,
                ): target
                for target in component_targets
            }
            for future in as_completed(futures):
                installed_path = future.result()
                print(f"  installed {installed_path}", flush=True)


def install_single_binary(
    artifacts_dir: Path,
    vendor_dir: Path,
    target: str,
    component: BinaryComponent,
) -> Path:
    artifact_subdir = artifact_dir_for_target(artifacts_dir, target)
    archive_path = _runtime().binary_archive_path(
        artifact_subdir, component.artifact_prefix, target
    )

    dest_dir = vendor_dir / target / component.dest_dir
    dest_dir.mkdir(parents=True, exist_ok=True)

    binary_name = (
        f"{component.binary_basename}.exe"
        if "windows" in target
        else component.binary_basename
    )
    dest = dest_dir / binary_name
    _runtime().extract_zstd_archive(archive_path, dest)
    if "windows" not in target:
        dest.chmod(0o755)
    return dest


def binary_archive_path(artifact_dir: Path, artifact_prefix: str, target: str) -> Path:
    archive_names = [archive_name_for_target(artifact_prefix, target)]
    if artifact_dir.name == f"{target}-unsigned":
        archive_names.append(
            archive_name_for_target(artifact_prefix, f"{target}-unsigned")
        )

    for archive_name in archive_names:
        archive_path = artifact_dir / archive_name
        if archive_path.exists():
            return archive_path

    raise FileNotFoundError(
        f"Expected artifact not found: {artifact_dir / archive_names[0]}"
    )


def archive_name_for_target(artifact_prefix: str, target: str) -> str:
    if "windows" in target:
        return f"{artifact_prefix}-{target}.exe.zst"
    return f"{artifact_prefix}-{target}.zst"


def artifact_dir_for_target(artifacts_dir: Path, target: str) -> Path:
    for artifact_name in [target, f"{target}-unsigned"]:
        artifact_dir = artifacts_dir / artifact_name
        if artifact_dir.is_dir():
            return artifact_dir

    return artifacts_dir / target


def extract_zstd_archive(archive_path: Path, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)

    temp_path = dest.parent / f".{dest.name}.{uuid.uuid4().hex}.tmp"
    try:
        subprocess.check_call(
            ["zstd", "-f", "-d", str(archive_path), "-o", str(temp_path)]
        )
        temp_path.replace(dest)
    finally:
        temp_path.unlink(missing_ok=True)
