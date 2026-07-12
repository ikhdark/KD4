#!/usr/bin/env python3
"""Audit KD4/upstream divergence and forecast a merge without changing the worktree."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import tempfile
from collections import Counter
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_UPSTREAM_REF = "upstream/main"
CONFLICT_STATUSES = frozenset({"DD", "AU", "UD", "UA", "DU", "AA", "UU"})


@dataclass(frozen=True)
class WorktreeState:
    changed_paths: int
    staged_paths: int
    unstaged_paths: int
    untracked_paths: int
    conflicted_paths: int
    status_counts: dict[str, int]

    @property
    def dirty(self) -> bool:
        return self.changed_paths > 0


@dataclass(frozen=True)
class MergeForecast:
    status: str
    result_tree: str | None
    conflict_paths: tuple[str, ...]
    messages: tuple[str, ...]
    exit_code: int


@dataclass(frozen=True)
class SyncAudit:
    schema_version: int
    captured_at: str
    repository: str
    branch: str
    head: str
    upstream_ref: str
    upstream: str
    merge_base: str
    ahead: int
    behind: int
    worktree: WorktreeState
    merge_forecast: MergeForecast
    safe_for_in_place_sync: bool
    recommended_strategy: str
    reasons: tuple[str, ...]

    def to_json(self) -> dict[str, Any]:
        return asdict(self)


def _run_git(
    repo_root: Path,
    args: Sequence[str],
    *,
    timeout_seconds: int,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        ["git", *args],
        cwd=repo_root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout_seconds,
        check=False,
    )
    if check and completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise RuntimeError(
            f"git {' '.join(args)} failed ({completed.returncode}): {detail}"
        )
    return completed


def _git_text(repo_root: Path, args: Sequence[str], *, timeout_seconds: int) -> str:
    return _run_git(repo_root, args, timeout_seconds=timeout_seconds).stdout.strip()


def parse_worktree_status(status_text: str) -> WorktreeState:
    status_counts: Counter[str] = Counter()
    staged = 0
    unstaged = 0
    untracked = 0
    conflicted = 0
    changed = 0
    for line in status_text.splitlines():
        if len(line) < 2:
            continue
        code = line[:2]
        status_counts[code] += 1
        changed += 1
        if code == "??":
            untracked += 1
            continue
        if code in CONFLICT_STATUSES:
            conflicted += 1
        if code[0] not in {" ", "?"}:
            staged += 1
        if code[1] not in {" ", "?"}:
            unstaged += 1
    return WorktreeState(
        changed_paths=changed,
        staged_paths=staged,
        unstaged_paths=unstaged,
        untracked_paths=untracked,
        conflicted_paths=conflicted,
        status_counts=dict(sorted(status_counts.items())),
    )


def parse_merge_forecast(completed: subprocess.CompletedProcess[str]) -> MergeForecast:
    lines = [line.rstrip() for line in completed.stdout.splitlines()]
    result_tree = next(
        (line for line in lines if re.fullmatch(r"[0-9a-f]{40,64}", line)), None
    )
    conflict_messages = tuple(line for line in lines if line.startswith("CONFLICT "))
    first_blank = lines.index("") if "" in lines else len(lines)
    candidate_paths = [
        line
        for line in lines[1:first_blank]
        if line and line != result_tree and not line.startswith("Auto-merging ")
    ]
    conflict_paths = set(candidate_paths)
    for message in conflict_messages:
        match = re.search(r" in (.+)$", message)
        if match:
            conflict_paths.add(match.group(1))
    if completed.returncode == 0:
        status = "clean"
    elif completed.returncode == 1 and conflict_messages:
        status = "conflicts"
    else:
        status = "error"
    messages = tuple(
        line
        for line in lines[first_blank + 1 :]
        if line.startswith("Auto-merging ") or line.startswith("CONFLICT ")
    )
    if completed.stderr.strip():
        messages = (*messages, *completed.stderr.strip().splitlines())
    return MergeForecast(
        status=status,
        result_tree=result_tree,
        conflict_paths=tuple(sorted(conflict_paths)),
        messages=messages[:200],
        exit_code=completed.returncode,
    )


def audit_repository(
    repo_root: Path = REPO_ROOT,
    *,
    upstream_ref: str = DEFAULT_UPSTREAM_REF,
    timeout_seconds: int = 120,
) -> SyncAudit:
    repo_root = repo_root.resolve()
    head = _git_text(repo_root, ["rev-parse", "HEAD"], timeout_seconds=timeout_seconds)
    upstream = _git_text(
        repo_root,
        ["rev-parse", "--verify", f"{upstream_ref}^{{commit}}"],
        timeout_seconds=timeout_seconds,
    )
    branch = (
        _git_text(
            repo_root,
            ["branch", "--show-current"],
            timeout_seconds=timeout_seconds,
        )
        or "DETACHED"
    )
    merge_base = _git_text(
        repo_root,
        ["merge-base", head, upstream],
        timeout_seconds=timeout_seconds,
    )
    counts = _git_text(
        repo_root,
        ["rev-list", "--left-right", "--count", f"{head}...{upstream}"],
        timeout_seconds=timeout_seconds,
    ).split()
    if len(counts) != 2:
        raise RuntimeError(f"unexpected ahead/behind output: {counts!r}")
    ahead, behind = (int(value) for value in counts)
    worktree = parse_worktree_status(
        _git_text(
            repo_root,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            timeout_seconds=timeout_seconds,
        )
    )
    merge_completed = _run_git(
        repo_root,
        ["merge-tree", "--write-tree", "--name-only", "--messages", head, upstream],
        timeout_seconds=timeout_seconds,
        check=False,
    )
    merge_forecast = parse_merge_forecast(merge_completed)

    reasons: list[str] = []
    if worktree.dirty:
        reasons.append(f"active worktree has {worktree.changed_paths} changed path(s)")
    if merge_forecast.status == "conflicts":
        reasons.append(
            f"trial merge reports {len(merge_forecast.conflict_paths)} conflict path(s)"
        )
    elif merge_forecast.status == "error":
        reasons.append("trial merge could not be evaluated cleanly")
    if behind:
        reasons.append(f"branch is {behind} commit(s) behind {upstream_ref}")
    if ahead:
        reasons.append(f"branch has {ahead} fork commit(s) not in {upstream_ref}")

    safe_for_in_place_sync = not worktree.dirty and merge_forecast.status == "clean"
    recommended_strategy = (
        "reviewed-in-place-merge"
        if safe_for_in_place_sync
        else "isolated-worktree-capability-by-capability"
    )
    return SyncAudit(
        schema_version=1,
        captured_at=datetime.now(timezone.utc).isoformat(),
        repository=str(repo_root),
        branch=branch,
        head=head,
        upstream_ref=upstream_ref,
        upstream=upstream,
        merge_base=merge_base,
        ahead=ahead,
        behind=behind,
        worktree=worktree,
        merge_forecast=merge_forecast,
        safe_for_in_place_sync=safe_for_in_place_sync,
        recommended_strategy=recommended_strategy,
        reasons=tuple(reasons),
    )


def write_json_atomic(path: Path, payload: dict[str, Any]) -> None:
    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        mode="w",
        encoding="utf-8",
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
        delete=False,
    ) as temporary:
        json.dump(payload, temporary, indent=2, sort_keys=True)
        temporary.write("\n")
        temporary_path = Path(temporary.name)
    os.replace(temporary_path, path)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--upstream-ref", default=DEFAULT_UPSTREAM_REF)
    parser.add_argument("--timeout-seconds", type=int, default=120)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--strict",
        action="store_true",
        help="Return nonzero unless an in-place sync is clean and the worktree is pristine.",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        audit = audit_repository(
            args.repo_root,
            upstream_ref=args.upstream_ref,
            timeout_seconds=args.timeout_seconds,
        )
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as exc:
        print(
            json.dumps({"ok": False, "error": str(exc)})
            if args.json
            else f"AUDIT FAILED: {exc}"
        )
        return 2
    payload = audit.to_json()
    if args.output is not None:
        write_json_atomic(args.output, payload)
    if args.json:
        print(json.dumps(payload, sort_keys=True))
    else:
        print(
            "KD4 SYNC AUDIT: "
            f"ahead={audit.ahead} behind={audit.behind} "
            f"dirty={audit.worktree.changed_paths} "
            f"forecast={audit.merge_forecast.status}"
        )
        print(f"Strategy: {audit.recommended_strategy}")
        for reason in audit.reasons:
            print(f"- {reason}")
        for path in audit.merge_forecast.conflict_paths:
            print(f"- conflict: {path}")
    return 1 if args.strict and not audit.safe_for_in_place_sync else 0


if __name__ == "__main__":
    raise SystemExit(main())
