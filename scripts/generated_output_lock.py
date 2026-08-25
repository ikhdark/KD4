#!/usr/bin/env python3
"""Cross-process serialization for repository-owned generated outputs."""

from __future__ import annotations

import contextlib
import json
import os
from pathlib import Path
from typing import TextIO
from typing import Iterator


class GenerationLockError(RuntimeError):
    """Raised when another generation owner already holds the lock."""


def _acquire_nonblocking(handle: TextIO) -> None:
    import msvcrt

    handle.seek(0, os.SEEK_END)
    if handle.tell() == 0:
        handle.seek(0)
        handle.write("\0")
        handle.flush()
    handle.seek(0)
    try:
        msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
    except OSError as error:
        raise BlockingIOError from error


def _release(handle: TextIO) -> None:
    import msvcrt

    handle.seek(0)
    msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)


@contextlib.contextmanager
def repository_lock(
    lock_path: Path, owner: str, resource: str = "repository resource"
) -> Iterator[Path]:
    owner = owner.strip()
    if not owner:
        raise GenerationLockError(f"{resource} owner cannot be empty")

    lock_path.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(
        {"owner": owner, "pid": os.getpid()},
        sort_keys=True,
        separators=(",", ":"),
    )
    handle = lock_path.open("a+", encoding="utf-8")
    try:
        _acquire_nonblocking(handle)
    except BlockingIOError as error:
        try:
            handle.seek(0)
            holder = lock_path.read_text(encoding="utf-8").strip()
        except OSError:
            holder = "unreadable lock metadata"
        handle.close()
        raise GenerationLockError(
            f"{resource} is already locked at {lock_path}: {holder}"
        ) from error
    except BaseException:
        handle.close()
        raise

    try:
        handle.seek(0)
        handle.truncate()
        handle.write(payload)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
        yield lock_path
    finally:
        try:
            _release(handle)
        finally:
            handle.close()


@contextlib.contextmanager
def generated_output_lock(root: Path, owner: str) -> Iterator[Path]:
    lock_path = root / ".codex" / "locks" / "generated-output.lock"
    with repository_lock(lock_path, owner, "generated outputs") as acquired:
        yield acquired


@contextlib.contextmanager
def source_map_lock(root: Path, owner: str) -> Iterator[Path]:
    lock_path = root / ".codex" / "locks" / "source-map.lock"
    with repository_lock(lock_path, owner, "source map outputs") as acquired:
        yield acquired
