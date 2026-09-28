#!/usr/bin/env python3
"""Cross-process serialization for repository-owned generated outputs."""

from __future__ import annotations

import contextlib
import errno
import json
import os
import sys
import time
from collections.abc import Iterator
from pathlib import Path
from typing import TextIO


class GenerationLockError(RuntimeError):
    """Raised when another generation owner already holds the lock."""


def _acquire_nonblocking(handle: TextIO) -> None:
    import msvcrt

    # Windows can lock beyond EOF. Never write before acquiring ownership:
    # an older owner may have truncated its metadata while retaining the lock.
    handle.seek(0)
    try:
        msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
    except OSError as error:
        if error.errno in {errno.EACCES, errno.EAGAIN, errno.EDEADLK}:
            raise BlockingIOError from error
        raise


def _release(handle: TextIO) -> None:
    import msvcrt

    handle.seek(0)
    msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)


@contextlib.contextmanager
def repository_lock(
    lock_path: Path,
    owner: str,
    resource: str = "repository resource",
    *,
    timeout: float = 0,
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
    handle = os.fdopen(
        os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o666), "r+", encoding="utf-8"
    )
    deadline = time.monotonic() + timeout
    waiting = False
    try:
        while True:
            try:
                _acquire_nonblocking(handle)
                break
            except BlockingIOError as error:
                try:
                    holder = lock_path.read_text(encoding="utf-8").strip()
                except OSError:
                    holder = "unreadable lock metadata"
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    detail = "acquisition timed out" if timeout else "is already locked"
                    raise GenerationLockError(
                        f"{resource} {detail} at {lock_path}: {holder}"
                    ) from error
                if not waiting:
                    print(
                        f"Waiting for {resource} at {lock_path}: {holder}",
                        file=sys.stderr,
                    )
                    waiting = True
                time.sleep(min(0.05, remaining))
    except BaseException:
        handle.close()
        raise

    try:
        handle.seek(0)
        handle.write(payload)
        handle.write("\n")
        handle.flush()
        handle.truncate()
        os.fsync(handle.fileno())
        yield lock_path
    finally:
        try:
            _release(handle)
        finally:
            handle.close()


@contextlib.contextmanager
def generated_output_lock(
    root: Path, owner: str, *, timeout: float = 0, resource: str = "generated-output"
) -> Iterator[Path]:
    if resource not in {"generated-output", "app-server-schema", "config-schema"}:
        raise ValueError(f"unknown generated output resource: {resource}")
    lock_path = root / ".codex" / "locks" / f"{resource}.lock"
    with repository_lock(
        lock_path, owner, f"generated outputs ({resource})", timeout=timeout
    ) as acquired:
        yield acquired
