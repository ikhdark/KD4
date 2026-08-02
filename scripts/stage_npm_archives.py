#!/usr/bin/env python3
"""Archive extraction and cache helpers for stage_npm_packages."""

from __future__ import annotations

from concurrent.futures import as_completed
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import errno
import hashlib
import os
from pathlib import Path
import shutil
import stat
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
DEFAULT_LOCK_TIMEOUT_SECONDS = 60 * 60
DEFAULT_GHA_DOWNLOAD_WORKERS = 8


def _runtime():
    from scripts import stage_npm_packages

    return stage_npm_packages


def _gha_enabled() -> bool:
    return os.environ.get("GITHUB_ACTIONS") == "true"


@contextmanager
def exclusive_file_lock(
    lock_path: Path,
    *,
    timeout_seconds: float = DEFAULT_LOCK_TIMEOUT_SECONDS,
    poll_seconds: float = LOCK_POLL_SECONDS,
):
    if timeout_seconds < 0:
        raise ValueError("lock timeout must be non-negative")
    if poll_seconds <= 0:
        raise ValueError("lock poll interval must be positive")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(lock_path), os.O_CREAT | os.O_RDWR, 0o600)
    acquired = False
    deadline = time.monotonic() + timeout_seconds
    try:
        if os.name == "nt" and os.fstat(fd).st_size == 0:
            os.write(fd, b"\0")
            os.fsync(fd)
        while not acquired:
            os.lseek(fd, 0, os.SEEK_SET)
            try:
                _acquire_file_lock(fd)
                acquired = True
            except OSError as error:
                if not _lock_error_is_contention(error):
                    raise
                if time.monotonic() >= deadline:
                    owner = lock_owner_pid(lock_path)
                    owner_detail = f" (reported owner pid {owner})" if owner else ""
                    raise TimeoutError(
                        f"timed out acquiring lock {lock_path}{owner_detail}"
                    ) from error
                time.sleep(poll_seconds)

        payload = f"pid={os.getpid()} thread={threading.get_ident()}\n".encode("utf-8")
        os.ftruncate(fd, 0)
        os.lseek(fd, 0, os.SEEK_SET)
        if os.write(fd, payload) != len(payload):
            raise OSError(f"short write while initializing lock {lock_path}")
        os.fsync(fd)
        yield
    finally:
        if acquired:
            os.lseek(fd, 0, os.SEEK_SET)
            try:
                _release_file_lock(fd)
            except OSError:
                pass
        os.close(fd)


def _acquire_file_lock(fd: int) -> None:
    if os.name == "nt":
        import msvcrt

        msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
    else:
        import fcntl

        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)


def _release_file_lock(fd: int) -> None:
    if os.name == "nt":
        import msvcrt

        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
    else:
        import fcntl

        fcntl.flock(fd, fcntl.LOCK_UN)


def _lock_error_is_contention(error: OSError) -> bool:
    if getattr(error, "winerror", None) in {33, 36}:
        return True
    return error.errno in {errno.EACCES, errno.EAGAIN, errno.EWOULDBLOCK}


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
                "version=2\n"
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
    except OSError:
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
        "version=2\n" not in marker
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
    digest.update(b"codex-extracted-archive-cache-v2\0")
    for path in sorted(
        root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()
    ):
        if path.name == COMPLETE_MARKER:
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
            with path.open("rb") as handle:
                while chunk := handle.read(1024 * 1024):
                    digest.update(chunk)
    return digest.hexdigest()


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
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
        try:
            subprocess.check_call(
                ["zstd", "-f", "-d", str(archive_path), "-o", str(temp_path)]
            )
        except FileNotFoundError as exc:
            raise RuntimeError(
                "zstd is required to extract native npm artifacts; install it and retry"
            ) from exc
        temp_path.replace(dest)
    finally:
        temp_path.unlink(missing_ok=True)
