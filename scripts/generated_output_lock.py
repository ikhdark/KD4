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

    handle.seek(0, os.SEEK_END)
    if handle.tell() == 0:
        handle.seek(0)
        handle.write("\0")
        handle.flush()
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
    handle = lock_path.open("a+", encoding="utf-8")
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
def generated_output_lock(
    root: Path, owner: str, *, timeout: float = 0
) -> Iterator[Path]:
    lock_path = root / ".codex" / "locks" / "generated-output.lock"
    with repository_lock(
        lock_path, owner, "generated outputs", timeout=timeout
    ) as acquired:
        yield acquired
