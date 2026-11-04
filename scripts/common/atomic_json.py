"""Atomic JSON file output shared by repository maintenance scripts."""

from __future__ import annotations

import json
import os
import tempfile
import time
from pathlib import Path
from typing import Any


def write_json_atomic(path: Path, payload: Any, *, exclusive: bool = False) -> None:
    write_bytes_atomic(
        path, (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode("utf-8"),
        exclusive=exclusive,
    )


def _replace_with_retry(source: Path, destination: Path) -> None:
    # Windows readers may briefly deny replacement. Retry only publication,
    # retaining the already-written payload, with a total sleep budget of 250ms.
    delays = (0.01, 0.02, 0.04, 0.08, 0.1)
    for attempt in range(len(delays) + 1):
        try:
            os.replace(source, destination)
            return
        except OSError as error:
            if getattr(error, "winerror", None) not in (5, 32, 33) or attempt == len(
                delays
            ):
                raise
            time.sleep(delays[attempt])


def write_bytes_atomic(path: Path, payload: bytes, *, exclusive: bool = False) -> None:
    # Replace the directory entry, never truncate a symlink/hardlink's referent.
    path = path.absolute()
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as temporary:
            temporary_path = Path(temporary.name)
            temporary.write(payload)
            temporary.flush()
            os.fsync(temporary.fileno())
        if exclusive:
            # Publish only complete bytes, atomically refusing an occupied path.
            os.link(temporary_path, path)
        else:
            _replace_with_retry(temporary_path, path)
            temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
