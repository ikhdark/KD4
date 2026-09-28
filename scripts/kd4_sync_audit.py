#!/usr/bin/env python3
"""Audit KD4/upstream divergence and forecast a merge without changing the worktree."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from collections import Counter
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Sequence

try:
    from scripts.atomic_json import write_json_atomic
    from scripts.process_owner import run_owned
except ImportError:  # Direct script execution places scripts/ on sys.path.
    from atomic_json import write_json_atomic
    from process_owner import run_owned


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
    upstream_remote: str
    upstream_remote_tip: str | None
    upstream_ref_stale: bool | None
    upstream_remote_error: str | None
    merge_base: str
    ahead: int
    behind: int
    worktree: WorktreeState
    active_operations: tuple[str, ...]
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
    completed = run_owned(
        ["git", *args],
        preserve_descendants_on_success=True,
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
    fields = completed.stdout.split("\0")
    result_tree = (
        fields[0]
        if re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", fields[0]) is not None
        else None
    )
    paths_end = fields.index("", 1) if "" in fields[1:] else len(fields)
    conflict_paths = tuple(sorted(set(fields[1:paths_end])))
    messages: list[str] = []
    has_conflicts = False
    malformed = False
    cursor = paths_end + 1
    # With -z, each message is: path count, paths, type, message, all NUL-delimited.
    while cursor < len(fields) and fields[cursor]:
        try:
            path_count = int(fields[cursor])
        except ValueError:
            malformed = True
            break
        message_index = cursor + path_count + 2
        if path_count < 0 or message_index >= len(fields):
            malformed = True
            break
        has_conflicts |= fields[message_index - 1].startswith("CONFLICT")
        messages.append(fields[message_index].rstrip("\n"))
        cursor = message_index + 1
    if malformed or result_tree is None:
        status = "error"
    elif completed.returncode == 0:
        status = "clean"
    elif completed.returncode == 1 and has_conflicts:
        status = "conflicts"
    else:
        status = "error"
    if malformed:
        messages.append("malformed git merge-tree message output")
    if completed.stderr.strip():
        messages.extend(completed.stderr.strip().splitlines())
    return MergeForecast(
        status=status,
        result_tree=result_tree,
        conflict_paths=conflict_paths,
        messages=tuple(messages[:200]),
        exit_code=completed.returncode,
    )


def active_git_operations(repo_root: Path, *, timeout_seconds: int) -> tuple[str, ...]:
    git_dir = Path(
        _git_text(
            repo_root,
            ["rev-parse", "--absolute-git-dir"],
            timeout_seconds=timeout_seconds,
        )
    )
    markers = (
        ("rebase", "rebase-merge"),
        ("rebase", "rebase-apply"),
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
        ("revert", "REVERT_HEAD"),
        ("sequencer", "sequencer"),
        ("bisect", "BISECT_LOG"),
    )
    return tuple(
        sorted({name for name, marker in markers if (git_dir / marker).exists()})
    )


def validate_output_path(
    repo_root: Path, output: Path, *, timeout_seconds: int
) -> None:
    root = Path(
        _git_text(
            repo_root, ["rev-parse", "--show-toplevel"], timeout_seconds=timeout_seconds
        )
    ).resolve()
    # Atomic replacement changes the entry, not a final symlink's referent.
    destination = output.parent.resolve() / output.name
    try:
        relative = destination.relative_to(root)
    except ValueError:
        return
    ignored = _run_git(
        root,
        ["check-ignore", "--quiet", "--", relative.as_posix()],
        timeout_seconds=timeout_seconds,
        check=False,
    )
    if ignored.returncode == 0:
        return
    if ignored.returncode != 1:
        raise RuntimeError(
            f"could not check output destination: {ignored.stderr.strip()}"
        )
    raise RuntimeError(
        "--output would change the audited worktree; choose an outside path "
        "or an ignored, untracked destination"
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
    upstream_remote, separator, upstream_branch = upstream_ref.partition("/")
    if not separator or not upstream_remote or not upstream_branch:
        raise RuntimeError(
            f"upstream ref must have <remote>/<branch> form: {upstream_ref!r}"
        )
    upstream_remote_tip = None
    upstream_ref_stale = None
    upstream_remote_error = None
    try:
        remote_output = _git_text(
            repo_root,
            [
                "ls-remote",
                "--exit-code",
                upstream_remote,
                f"refs/heads/{upstream_branch}",
            ],
            timeout_seconds=timeout_seconds,
        )
        remote_fields = remote_output.split()
        if len(remote_fields) != 2 or not re.fullmatch(
            r"[0-9a-f]{40,64}", remote_fields[0]
        ):
            raise RuntimeError(
                f"unexpected ls-remote output for {upstream_ref}: {remote_output!r}"
            )
        upstream_remote_tip = remote_fields[0]
        upstream_ref_stale = upstream != upstream_remote_tip
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as exc:
        upstream_remote_error = str(exc)
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
        _run_git(
            repo_root,
            ["status", "--porcelain=v1", "--untracked-files=all"],
            timeout_seconds=timeout_seconds,
        ).stdout
    )
    active_operations = active_git_operations(
        repo_root, timeout_seconds=timeout_seconds
    )
    merge_completed = _run_git(
        repo_root,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--messages",
            "-z",
            head,
            upstream,
        ],
        timeout_seconds=timeout_seconds,
        check=False,
    )
    merge_forecast = parse_merge_forecast(merge_completed)

    reasons: list[str] = []
    if worktree.dirty:
        reasons.append(f"active worktree has {worktree.changed_paths} changed path(s)")
    if active_operations:
        reasons.append(f"Git operation in progress: {', '.join(active_operations)}")
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
    if upstream_ref_stale:
        reasons.append(f"local {upstream_ref} is stale relative to {upstream_remote}")
    if upstream_remote_error is not None:
        reasons.append("remote freshness is unknown; local-only diagnostics follow")

    safe_for_in_place_sync = (
        not worktree.dirty
        and not active_operations
        and merge_forecast.status == "clean"
        and upstream_ref_stale is False
    )
    recommended_strategy = (
        "reviewed-in-place-merge"
        if safe_for_in_place_sync
        else "isolated-worktree-capability-by-capability"
    )
    return SyncAudit(
        schema_version=2,
        captured_at=datetime.now(timezone.utc).isoformat(),
        repository=str(repo_root),
        branch=branch,
        head=head,
        upstream_ref=upstream_ref,
        upstream=upstream,
        upstream_remote=upstream_remote,
        upstream_remote_tip=upstream_remote_tip,
        upstream_ref_stale=upstream_ref_stale,
        upstream_remote_error=upstream_remote_error,
        merge_base=merge_base,
        ahead=ahead,
        behind=behind,
        worktree=worktree,
        active_operations=active_operations,
        merge_forecast=merge_forecast,
        safe_for_in_place_sync=safe_for_in_place_sync,
        recommended_strategy=recommended_strategy,
        reasons=tuple(reasons),
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--upstream-ref", default=DEFAULT_UPSTREAM_REF)
    parser.add_argument("--timeout-seconds", type=int, default=120)
    parser.add_argument(
        "--output",
        type=Path,
        help="Write JSON outside the checkout or to an ignored, untracked destination.",
    )
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
        if args.output is not None:
            validate_output_path(
                args.repo_root, args.output, timeout_seconds=args.timeout_seconds
            )
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
    errors = []
    if audit.upstream_remote_error is not None:
        errors.append(audit.upstream_remote_error)
        payload.update(ok=False, error=audit.upstream_remote_error)
    if args.output is not None:
        try:
            write_json_atomic(args.output, payload)
        except (OSError, ValueError) as exc:
            output_error = f"could not write audit output: {exc}"
            errors.append(output_error)
            payload.update(ok=False, error="; ".join(errors), output_error=output_error)
    if args.json:
        print(json.dumps(payload, sort_keys=True))
    else:
        freshness = (
            audit.upstream_ref_stale
            if audit.upstream_ref_stale is not None
            else "unknown"
        )
        print(
            "KD4 SYNC AUDIT: "
            f"ahead={audit.ahead} behind={audit.behind} "
            f"dirty={audit.worktree.changed_paths} "
            f"forecast={audit.merge_forecast.status}"
            f" upstream_stale={freshness}"
        )
        print(f"Strategy: {audit.recommended_strategy}")
        for reason in audit.reasons:
            print(f"- {reason}")
        for path in audit.merge_forecast.conflict_paths:
            print(f"- conflict: {path}")
        for error in errors:
            print(f"AUDIT FAILED: {error}")
    if errors:
        return 2
    return 1 if args.strict and not audit.safe_for_in_place_sync else 0


if __name__ == "__main__":
    raise SystemExit(main())
