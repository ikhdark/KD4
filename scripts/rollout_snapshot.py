#!/usr/bin/env python3
"""Read a live Codex rollout through a stable, checksummed byte snapshot."""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import tempfile
import json
import os
import re
import stat
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, BinaryIO, Iterator, Sequence

try:
    from scripts.atomic_json import write_stream_atomic
except ImportError:
    from atomic_json import write_stream_atomic


_READ_CHUNK_BYTES = 1024 * 1024
_MAX_PAYLOAD_BYTES = 256 * 1024 * 1024
_PAYLOAD_KIND = "rollout_payload_artifact"


def rollout_payload_root(path: Path) -> Path:
    for ancestor in path.parents:
        if ancestor.name in {"sessions", "archived_sessions"}:
            return ancestor.parent / "rollout-payloads"
    return path.parent / "rollout-payloads"


def _codex_home_payload_roots() -> list[Path]:
    homes = [os.environ.get("CODEX_HOME"), os.path.expanduser("~/.codex")]
    return [Path(home) / "rollout-payloads" for home in homes if home]


def load_rollout_payload(path: Path, sha256: str, expected_bytes: int | None = None) -> bytes:
    """Load immutable undo/timing data, rejecting missing or substituted blobs."""
    if not isinstance(sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", sha256):
        raise ValueError("invalid rollout payload hash")
    # A rollout copied out of its sessions tree leaves its blobs behind. They
    # are content-addressed and checksum-verified below, so the Codex home's
    # store may stand in for a missing colocated blob, but never a corrupt one.
    roots = list(dict.fromkeys([rollout_payload_root(path), *_codex_home_payload_roots()]))
    for root in roots:
        artifact = root / f"{sha256}.json"
        try:
            metadata = artifact.lstat()
            break
        except FileNotFoundError:
            continue
    else:
        raise FileNotFoundError(
            f"rollout payload {sha256}.json not found in: "
            + ", ".join(str(root) for root in roots)
        )
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > _MAX_PAYLOAD_BYTES:
        raise ValueError(f"invalid rollout payload file: {artifact}")
    if expected_bytes is not None and metadata.st_size != expected_bytes:
        raise ValueError(f"rollout payload size mismatch: {artifact}")
    with artifact.open("rb") as handle:
        data = handle.read(metadata.st_size + 1)
    if len(data) != metadata.st_size or hashlib.sha256(data).hexdigest() != sha256:
        raise ValueError(f"rollout payload checksum mismatch: {artifact}")
    return data


def hydrate_rollout_record(record: Any, path: Path) -> Any:
    """Keep the public record shape identical for inline and external payloads."""
    if not isinstance(record, dict) or record.get("type") != _PAYLOAD_KIND:
        return record
    reference = record.get("payload")
    if not isinstance(reference, dict) or type(reference.get("bytes")) is not int:
        raise ValueError("invalid rollout payload reference")
    data = load_rollout_payload(path, reference.get("sha256"), reference["bytes"])
    return _hydrate_verified_payload(record, data)


def _hydrate_verified_payload(record: dict[str, Any], data: bytes) -> dict[str, Any]:
    """Decode bytes already authenticated by load_rollout_payload in this operation."""
    reference = record["payload"]
    item = json.loads(data)
    if (not isinstance(item, dict) or item.get("type") == _PAYLOAD_KIND
            or item.get("type") != reference.get("item_type")):
        raise ValueError("rollout payload type mismatch")
    item["timestamp"] = record.get("timestamp")
    item["format_version"] = record.get("format_version")
    return item


def copy_rollout_payloads(snapshot: RolloutSnapshot, output: Path) -> None:
    """Copy dependencies before publishing a byte-identical portable snapshot."""
    if rollout_payload_root(snapshot.path) == rollout_payload_root(output):
        return
    seen = set()
    with snapshot.open_lines() as lines:
        for line in lines:
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue  # Preserve raw snapshots, including an incomplete tail.
            if not isinstance(record, dict) or record.get("type") != _PAYLOAD_KIND:
                continue
            reference = record["payload"]
            if not isinstance(reference, dict) or type(reference.get("bytes")) is not int:
                raise ValueError("invalid rollout payload reference")
            data = load_rollout_payload(snapshot.path, reference.get("sha256"), reference["bytes"])
            _hydrate_verified_payload(record, data)
            digest = reference["sha256"]
            if digest in seen:
                continue
            seen.add(digest)
            directory = rollout_payload_root(output)
            directory.mkdir(parents=True, exist_ok=True)
            destination = directory / f"{digest}.json"
            # Publish atomically without replacing any existing immutable blob.
            temporary_path = None
            try:
                with tempfile.NamedTemporaryFile(dir=directory, delete=False) as temporary:
                    temporary_path = Path(temporary.name)
                    temporary.write(data)
                    temporary.flush()
                    os.fsync(temporary.fileno())
                try:
                    os.link(temporary_path, destination)
                except FileExistsError:
                    load_rollout_payload(output, digest, reference["bytes"])
            finally:
                if temporary_path is not None:
                    temporary_path.unlink()


@dataclass(frozen=True)
class RolloutSnapshot:
    """Owned immutable bytes; use as a context manager or explicitly close it.

    Reading lines does not transfer ownership. Copying only ``data`` does not
    export referenced payloads; use the CLI's ``--output`` for a portable copy.
    """

    path: Path
    stream: BinaryIO
    sha256: str
    byte_length: int

    def close(self) -> None:
        """Release the captured stream; repeated calls are harmless."""
        self.stream.close()

    def __enter__(self) -> RolloutSnapshot:
        if self.stream.closed:
            raise ValueError("rollout snapshot is already closed")
        return self

    def __exit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> None:
        self.close()

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

    def decoded_lines(self) -> Iterator[tuple[int, Any, str | None, int]]:
        """Decode this captured stream once, preserving line/error coverage.

        Consumers may retain these operation-local values, not treat them as
        current disk evidence. A new snapshot must decode its own bytes. The
        final field is expanded line bytes, for bounded compressed-input reuse.
        """
        with self.open_lines() as lines:
            for number, line in enumerate(lines, 1):
                try:
                    item = json.loads(line)
                    if not isinstance(item, dict):
                        raise TypeError("rollout record must be an object")
                except (ValueError, TypeError) as error:
                    yield number, None, str(error), len(line)
                else:
                    yield number, item, None, len(line)

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
    """Capture once; use `with read_rollout_snapshot(path) as snapshot` to close it.

    `open_lines()` borrows the snapshot; exiting it does not close an uncompressed
    snapshot. Existing callers may still explicitly close `snapshot.stream`.
    For decoded records plus wire lengths, prefer `read_rollout_records(path)`.
    """
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
        try:
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
        except BaseException:
            captured.close()
            raise

    return RolloutSnapshot(
        path=resolved,
        stream=captured,
        sha256=digest.hexdigest(),
        byte_length=byte_length,
    )


def iter_rollout_records(path: Path) -> Iterator[tuple[dict[str, Any], int]]:
    """Yield records and their JSONL byte lengths from one fixed snapshot.

    A live writer can leave an unterminated, unparseable final record. Keep the
    complete prefix with a warning, but reject corrupt complete lines and
    non-object records rather than silently presenting an incomplete report.

    Exhaust the iterator before publishing results. Consumers that can stop
    early must close it (for example, with contextlib.closing).
    """
    snapshot = read_rollout_snapshot(path)
    with snapshot, snapshot.open_lines() as lines:
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
            yield hydrate_rollout_record(record, snapshot.path), len(line)


def read_rollout_records(path: Path) -> list[tuple[dict[str, Any], int]]:
    """Return all verified records, closing the snapshot before returning."""
    return list(iter_rollout_records(path))


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
        help="Write a portable snapshot with checksum-verified referenced payloads",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    with read_rollout_snapshot(args.path) as snapshot:
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
            copy_rollout_payloads(snapshot, output)
            snapshot.stream.seek(0)
            write_stream_atomic(output, snapshot.stream)
            metadata["output"] = str(output)
    print(json.dumps(metadata, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
