#!/usr/bin/env python3
"""Read a live Codex rollout through a stable, checksummed byte snapshot."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import tempfile
import json
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO, Iterator, Sequence

try:
    from scripts.atomic_json import write_stream_atomic
except ImportError:
    from atomic_json import write_stream_atomic


_READ_CHUNK_BYTES = 1024 * 1024


@dataclass(frozen=True)
class RolloutSnapshot:
    path: Path
    stream: BinaryIO
    sha256: str
    byte_length: int

    @property
    def data(self) -> bytes:
        self.stream.seek(0)
        return self.stream.read()

    @contextlib.contextmanager
    def open_lines(self) -> Iterator[BinaryIO]:
        """Decode captured bytes without changing their on-disk identity."""
        self.stream.seek(0)
        with contextlib.nullcontext(self.stream) as raw:
            if self.path.name.endswith(".jsonl.zst"):
                try:
                    from compression import zstd
                except ImportError:
                    try:
                        from backports import zstd
                    except ImportError as error:
                        raise ValueError(
                            "Compressed rollouts require Python 3.14+ or backports.zstd; "
                            "run through `uv run --project scripts` to install dependencies"
                        ) from error
                try:
                    with zstd.open(raw, "rb") as decoded:
                        yield decoded
                except (zstd.ZstdError, EOFError) as error:
                    raise ValueError(
                        f"cannot decompress rollout {self.path}: {error}"
                    ) from error
            else:
                yield raw

    def text_lines(self) -> list[str]:
        with self.open_lines() as handle:
            return handle.read().decode("utf-8").splitlines()

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


def existing_rollout_path(path: Path) -> Path:
    """Resolve a canonical path after the rollout compression worker moves it."""
    if path.exists():
        return path
    if path.name.endswith(".jsonl"):
        compressed = path.with_name(path.name + ".zst")
        if compressed.is_file():
            return compressed
    return path


def discover_rollouts(root: Path, pattern: str = "*.jsonl") -> list[Path]:
    """Include cold rollouts, preferring plain siblings as the runtime does."""
    paths = {path for path in root.rglob(pattern) if path.is_file()}
    for compressed in root.rglob(pattern + ".zst"):
        if compressed.is_file() and compressed.with_suffix("") not in paths:
            paths.add(compressed)
    return sorted(paths)


def read_rollout_snapshot(path: Path) -> RolloutSnapshot:
    candidate = existing_rollout_path(path)
    for attempt in range(2):
        try:
            resolved = candidate.resolve(strict=True)
            handle = (
                _open_shared_binary(resolved)
                if os.name == "nt"
                else resolved.open("rb")
            )
            break
        except FileNotFoundError:
            # Compression can retire the plain name after lookup but before
            # open. Recover that one transition without rescanning or polling.
            compressed = candidate.with_name(candidate.name + ".zst")
            if (
                attempt
                or not candidate.name.endswith(".jsonl")
                or not compressed.is_file()
            ):
                raise
            candidate = compressed
    with handle:
        byte_length = os.fstat(handle.fileno()).st_size
        captured = tempfile.SpooledTemporaryFile(max_size=4 * _READ_CHUNK_BYTES)
        digest = hashlib.sha256()
        remaining = byte_length
        while remaining:
            chunk = handle.read(min(remaining, _READ_CHUNK_BYTES))
            if not chunk:
                raise OSError(
                    f"rollout shrank while reading {resolved}: "
                    f"expected {byte_length} bytes"
                )
            captured.write(chunk)
            digest.update(chunk)
            remaining -= len(chunk)

    return RolloutSnapshot(
        path=resolved,
        stream=captured,
        sha256=digest.hexdigest(),
        byte_length=byte_length,
    )


def read_rollout_records(path: Path) -> list[tuple[dict[str, Any], int]]:
    """Return records and their JSONL byte lengths from one fixed snapshot.

    A live writer can leave an unterminated, unparseable final record. Keep the
    complete prefix with a warning, but reject corrupt complete lines and
    non-object records rather than silently presenting an incomplete report.
    """
    snapshot = read_rollout_snapshot(path)
    records = []
    with contextlib.closing(snapshot.stream), snapshot.open_lines() as lines:
        for number, line in enumerate(lines, 1):
            try:
                record = json.loads(line.decode("utf-8"))
            except (json.JSONDecodeError, UnicodeDecodeError) as error:
                if not line.endswith(b"\n"):
                    print(
                        f"warning: {snapshot.path}:{number}: ignoring unparseable "
                        "unterminated final record; report covers the complete prefix only",
                        file=sys.stderr,
                    )
                    break
                raise ValueError(
                    f"invalid rollout record {snapshot.path}:{number}: {error}"
                ) from error
            if not isinstance(record, dict):
                raise ValueError(
                    f"rollout record {snapshot.path}:{number} is not an object"
                )
            records.append((record, len(line)))
    return records


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Read the fixed byte length observed when a live rollout is opened "
            "and report its SHA-256 identity."
        )
    )
    parser.add_argument("path", type=Path, help="Rollout JSONL or JSONL.zst path")
    parser.add_argument(
        "--output",
        type=Path,
        help="Optional path where the captured bytes are written",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    snapshot = read_rollout_snapshot(args.path)
    with contextlib.closing(snapshot.stream):
        metadata = snapshot.metadata()
        if args.output is not None:
            output = args.output.resolve()
            if output == snapshot.path or (
                output.exists() and output.samefile(snapshot.path)
            ):
                raise ValueError("snapshot output must not overwrite the live rollout")
            if output.name.endswith(".jsonl.zst") != snapshot.path.name.endswith(
                ".jsonl.zst"
            ):
                raise ValueError(
                    "snapshot output suffix must match the captured rollout format "
                    "(.jsonl.zst for compressed bytes)"
                )
            snapshot.stream.seek(0)
            write_stream_atomic(output, snapshot.stream)
            metadata["output"] = str(output)
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
