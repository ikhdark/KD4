#!/usr/bin/env python3
"""Read a live Codex rollout through a stable, checksummed byte snapshot."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Sequence


_READ_CHUNK_BYTES = 1024 * 1024


@dataclass(frozen=True)
class RolloutSnapshot:
    path: Path
    data: bytes
    sha256: str
    byte_length: int

    def text_lines(self) -> list[str]:
        return self.data.decode("utf-8").splitlines()

    def metadata(self) -> dict[str, str | int]:
        return {
            "path": str(self.path),
            "byteLength": self.byte_length,
            "sha256": self.sha256,
        }


def _open_shared_binary(path: Path) -> BinaryIO:
    import ctypes
    import msvcrt
    from ctypes import wintypes

    create_file = ctypes.WinDLL("kernel32", use_last_error=True).CreateFileW
    create_file.argtypes = (
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.LPVOID,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    )
    create_file.restype = wintypes.HANDLE

    generic_read = 0x80000000
    share_read_write_delete = 0x00000001 | 0x00000002 | 0x00000004
    open_existing = 3
    normal_attributes = 0x00000080
    handle = create_file(
        str(path),
        generic_read,
        share_read_write_delete,
        None,
        open_existing,
        normal_attributes,
        None,
    )
    if handle == wintypes.HANDLE(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())

    try:
        fd = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
    except BaseException:
        ctypes.WinDLL("kernel32", use_last_error=True).CloseHandle(handle)
        raise
    return os.fdopen(fd, "rb")


def read_rollout_snapshot(path: Path) -> RolloutSnapshot:
    resolved = path.resolve(strict=True)
    with _open_shared_binary(resolved) as handle:
        byte_length = os.fstat(handle.fileno()).st_size
        chunks: list[bytes] = []
        remaining = byte_length
        while remaining:
            chunk = handle.read(min(remaining, _READ_CHUNK_BYTES))
            if not chunk:
                raise OSError(
                    f"rollout shrank while reading {resolved}: "
                    f"expected {byte_length} bytes"
                )
            chunks.append(chunk)
            remaining -= len(chunk)

    data = b"".join(chunks)
    return RolloutSnapshot(
        path=resolved,
        data=data,
        sha256=hashlib.sha256(data).hexdigest(),
        byte_length=byte_length,
    )


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Read the fixed byte length observed when a live rollout is opened "
            "and report its SHA-256 identity."
        )
    )
    parser.add_argument("path", type=Path, help="Exact rollout JSONL path")
    parser.add_argument(
        "--output",
        type=Path,
        help="Optional path where the captured bytes are written",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    snapshot = read_rollout_snapshot(args.path)
    metadata = snapshot.metadata()
    if args.output is not None:
        output = args.output.resolve()
        if output == snapshot.path:
            raise ValueError("snapshot output must not overwrite the live rollout")
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(snapshot.data)
        metadata["output"] = str(output)
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
