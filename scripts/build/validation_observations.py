"""Bounded content observations, never a hermetic build or replay contract."""

from __future__ import annotations

import hashlib
import json
import os
import stat
from pathlib import Path


MAX_SOURCE_ENTRIES = 8192
MAX_SOURCE_BYTES = 16 * 1024 * 1024
MAX_ARTIFACT_BYTES = 128 * 1024 * 1024
EXCLUDED_OUTPUT_DIRECTORIES = frozenset(
    {".git", "target", "node_modules", ".venv", "__pycache__"}
)


def file_observation(path: Path, budget: list[int]) -> dict:
    """Hash bytes, not size/mtime; concurrent replacement stays unavailable."""
    try:
        before = path.lstat()
        if (
            not stat.S_ISREG(before.st_mode)
            or getattr(before, "st_file_attributes", 0) & 0x400
        ):
            return {"state": "indirect_or_nonregular"}
        if before.st_size > budget[0]:
            return {"state": "budget_exceeded"}
        digest = hashlib.sha256()
        count = 0
        with path.open("rb") as source:
            opened = os.fstat(source.fileno())
            while chunk := source.read(min(65536, budget[0] + 1)):
                if len(chunk) > budget[0]:
                    return {"state": "changed_or_budget_exceeded"}
                budget[0] -= len(chunk)
                count += len(chunk)
                digest.update(chunk)
            closed = os.fstat(source.fileno())
        after = path.lstat()
        identity = lambda value: (
            value.st_dev,
            value.st_ino,
            value.st_size,
            value.st_mtime_ns,
            value.st_ctime_ns,
        )
        if any(
            identity(value) != identity(before) for value in (opened, closed, after)
        ):
            return {"state": "changed_during_read"}
        return {"state": "observed", "sha256": digest.hexdigest(), "bytes": count}
    except (OSError, ValueError):
        return {"state": "unavailable"}


def source_observations(
    roots, *, max_entries=MAX_SOURCE_ENTRIES, max_bytes=MAX_SOURCE_BYTES
):
    """Observe declared package trees with a single shared byte/entry budget.

    Ignored/ordinary untracked source is included. Symlinks, build outputs,
    external/generated inputs and inputs outside these roots are not a complete
    dependency closure. Even matching observations never authorize replay.
    """
    budget = [max_bytes]
    remaining = max_entries
    results = []
    # Invocation-local only: sharing observations never authorizes later replay.
    directories = {}
    observations = {}
    for root in sorted({Path(root).absolute() for root in roots}, key=str):
        stack, records, incomplete = [root], [], False
        while stack:
            directory = stack.pop()
            try:
                attributes = directory.lstat()
                if (
                    directory.is_symlink()
                    or getattr(attributes, "st_file_attributes", 0) & 0x400
                ):
                    incomplete = True
                    continue
                if directory not in directories:
                    bounded, truncated = [], False
                    with os.scandir(directory) as entries:
                        for entry in entries:
                            if remaining <= 0:
                                truncated = True
                                break
                            remaining -= 1
                            bounded.append(entry)
                    directories[directory] = (bounded, truncated)
                bounded, truncated = directories[directory]
                incomplete |= truncated
                for entry in sorted(bounded, key=lambda entry: entry.name):
                    path = Path(entry.path)
                    if entry.is_dir(follow_symlinks=False):
                        if entry.name not in EXCLUDED_OUTPUT_DIRECTORIES:
                            stack.append(path)
                    else:
                        if path not in observations:
                            observations[path] = file_observation(path, budget)
                        observed = observations[path]
                        records.append((path.relative_to(root).as_posix(), observed))
                        incomplete |= observed["state"] != "observed"
            except (OSError, ValueError):
                incomplete = True
        digest = hashlib.sha256(
            json.dumps(sorted(records), sort_keys=True).encode()
        ).hexdigest()
        results.append(
            {
                "root": str(root),
                "state": "incomplete" if incomplete else "observed",
                "sha256": None if incomplete else digest,
                "files": len(records),
            }
        )
    return results


def artifact_observations(artifacts):
    budget = [MAX_ARTIFACT_BYTES]
    return {
        name: {"path": str(path), **file_observation(Path(path), budget)}
        for name, path in sorted(artifacts.items())
    }


def comparison(before, after):
    if before != after:
        return "changed"
    rows = list(before.values()) if isinstance(before, dict) else before
    return (
        "observed_unchanged"
        if rows and all(row.get("state") == "observed" for row in rows)
        else "unknown"
    )
