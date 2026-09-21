"""Atomic JSON file output shared by repository maintenance scripts."""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any


def write_json_atomic(path: Path, payload: Any) -> None:
    write_bytes_atomic(
        path, (json.dumps(payload, indent=2, sort_keys=True) + "\n").encode("utf-8")
    )


def write_bytes_atomic(path: Path, payload: bytes, *, immutable: bool = False) -> None:
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
        if immutable:
            try:
                os.link(temporary_path, path)
            except FileExistsError:
                if path.is_symlink() or path.read_bytes() != payload:
                    raise ValueError(
                        f"immutable delivery artifact was modified: {path}"
                    )
        else:
            os.replace(temporary_path, path)
            temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
