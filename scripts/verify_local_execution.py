#!/usr/bin/env python3
"""Caching, execution, and reporting for verify_local."""

from __future__ import annotations

import argparse
from datetime import datetime
from datetime import timezone
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from types import ModuleType
from typing import Any
from typing import Iterable

from scripts.verify_local_context import (
    CACHE_PATH,
    CODEX_RS,
    CargoGraph,
    CommandResult,
    CommandSpec,
    FAILED,
    INCONCLUSIVE,
    LEDGER_PATH,
    LOG_DIR,
    Plan,
    REPO_ROOT,
    RULES_PATH,
    STATE_DIR,
    Scope,
    TOOLING_ERROR,
    VERIFIED,
    VERIFIED_NO_PROOF,
)


_RUNTIME: ModuleType | None = None


def configure_runtime(runtime: ModuleType) -> None:
    global _RUNTIME
    _RUNTIME = runtime


def _runtime() -> ModuleType:
    if _RUNTIME is None:
        raise RuntimeError("verify_local execution runtime is not configured")
    return _RUNTIME


def proof_input_files() -> list[Path]:
    candidates = [
        REPO_ROOT / "Cargo.lock",
        CODEX_RS / "Cargo.lock",
        CODEX_RS / "Cargo.toml",
        REPO_ROOT / "rust-toolchain.toml",
        REPO_ROOT / "rust-toolchain",
        CODEX_RS / "rust-toolchain.toml",
        CODEX_RS / "rust-toolchain",
        REPO_ROOT / ".config" / "nextest.toml",
        CODEX_RS / ".config" / "nextest.toml",
        REPO_ROOT / ".cargo" / "config.toml",
        CODEX_RS / ".cargo" / "config.toml",
        REPO_ROOT / "justfile",
        Path(_runtime().__file__).resolve(),
        RULES_PATH,
    ]
    return [path for path in candidates if path.exists()]


def selected_hash_roots(packages: Iterable[str], graph: CargoGraph) -> list[Path]:
    names = set(packages)
    names.update(graph.transitive_workspace_deps(names))
    roots: list[Path] = []
    for name in sorted(names):
        package = graph.packages_by_name.get(name)
        if package is not None:
            roots.append(package.root)
    return roots


def git_list_selected_files(roots: list[Path]) -> list[Path]:
    if not roots:
        return []
    rel_roots = [_runtime().rel(root) for root in roots]
    output = _runtime().git(
        ["ls-files", "--cached", "--others", "--exclude-standard", "--", *rel_roots],
        check=False,
    )
    return _runtime().normalize_paths(
        line for line in output.splitlines() if line.strip()
    )


def working_tree_hash(packages: Iterable[str], graph: CargoGraph) -> str:
    hasher = hashlib.sha256()
    for path in sorted(
        _runtime().git_list_selected_files(
            _runtime().selected_hash_roots(packages, graph)
        ),
        key=lambda p: p.as_posix(),
    ):
        full = REPO_ROOT / path
        hasher.update(path.as_posix().encode("utf-8") + b"\0")
        if full.exists() and full.is_file():
            hasher.update(oct(full.stat().st_mode & 0o777).encode("utf-8") + b"\0")
            hasher.update(full.read_bytes())
        else:
            hasher.update(b"DELETED")
        hasher.update(b"\0")
    for extra in _runtime().proof_input_files():
        hasher.update(_runtime().rel(extra).encode("utf-8") + b"\0")
        hasher.update(extra.read_bytes())
        hasher.update(b"\0")
    for key in ["RUSTFLAGS", "NEXTEST_PROFILE"]:
        hasher.update(
            key.encode("utf-8") + b"=" + os.environ.get(key, "").encode("utf-8") + b"\0"
        )
    for key, value in sorted(os.environ.items()):
        if key.startswith("CARGO_PROFILE_"):
            hasher.update(key.encode("utf-8") + b"=" + value.encode("utf-8") + b"\0")
    hasher.update(sys.platform.encode("utf-8"))
    return hasher.hexdigest()


def scoped_file_hash(paths: Iterable[Path]) -> str:
    hasher = hashlib.sha256()
    for path in sorted(paths, key=lambda p: p.as_posix()):
        full = REPO_ROOT / path
        hasher.update(path.as_posix().encode("utf-8") + b"\0")
        if full.exists() and full.is_file():
            hasher.update(oct(full.stat().st_mode & 0o777).encode("utf-8") + b"\0")
            hasher.update(full.read_bytes())
        else:
            hasher.update(b"DELETED")
        hasher.update(b"\0")
    for extra in _runtime().proof_input_files():
        hasher.update(_runtime().rel(extra).encode("utf-8") + b"\0")
        hasher.update(extra.read_bytes())
        hasher.update(b"\0")
    hasher.update(sys.platform.encode("utf-8"))
    return hasher.hexdigest()


def load_cache() -> dict[str, Any]:
    if not CACHE_PATH.exists():
        return {"entries": {}}
    try:
        return json.loads(CACHE_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {"entries": {}}


def surface_paths_hash(patterns: Iterable[str], active_files: Iterable[Path]) -> str:
    roots = [REPO_ROOT / pattern for pattern in sorted(set(patterns))]
    files = set(_runtime().git_list_selected_files(roots))
    files.update(active_files)
    return _runtime().scoped_file_hash(sorted(files, key=lambda p: p.as_posix()))


def cache_key(
    command: CommandSpec, scope: Scope, graph: CargoGraph
) -> tuple[str, dict[str, Any]]:
    components: list[str] = []
    if command.owner_packages:
        components.append(_runtime().working_tree_hash(command.owner_packages, graph))
    if command.hash_paths:
        components.append(
            _runtime().surface_paths_hash(command.hash_paths, scope.active_files)
        )
    if not components:
        components.append(_runtime().scoped_file_hash(scope.active_files))
    input_hash = (
        components[0]
        if len(components) == 1
        else hashlib.sha256("\0".join(components).encode("utf-8")).hexdigest()
    )
    payload = {
        "command": list(command.args),
        "command_kind": command.kind,
        "owner_package": list(command.owner_packages),
        "input_hash": input_hash,
    }
    if command.kind not in {"owner_test", "owner_check"}:
        payload["scope_id"] = scope.scope_id
    key = hashlib.sha256(
        json.dumps(payload, sort_keys=True).encode("utf-8")
    ).hexdigest()
    return key, payload


def append_ledger(entry: dict[str, Any]) -> None:
    data = {"runs": []}
    if LEDGER_PATH.exists():
        try:
            data = json.loads(LEDGER_PATH.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            data = {"runs": []}
    data.setdefault("runs", []).append(entry)
    data["runs"] = data["runs"][-200:]
    _runtime().atomic_write_json(LEDGER_PATH, data)


def log_path_for(command: CommandSpec) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    safe = "".join(ch if ch.isalnum() or ch in "-_" else "-" for ch in command.id)[:80]
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    return LOG_DIR / f"{stamp}-{safe}.log"


def summarize_failure(output: str) -> str:
    lines = output.splitlines()
    interesting = [
        line
        for line in lines
        if "error" in line.lower()
        or "failed" in line.lower()
        or "panicked" in line.lower()
    ]
    return "\n".join((interesting or lines[-12:])[:8])


def execute_command(command: CommandSpec) -> CommandResult:
    start = time.monotonic()
    log_path = _runtime().log_path_for(command)
    args_list = list(command.args)
    if args_list and args_list[0] in ("python", "python3"):
        # Rules hardcode "python", which may not exist as an alias on
        # WSL/unix; the running interpreter always does.
        args_list[0] = sys.executable
    try:
        completed = _runtime().run_capture(
            args_list, cwd=command.cwd, timeout=command.timeout
        )
        duration = time.monotonic() - start
        combined = f"$ {command.display()}\n\nSTDOUT:\n{completed.stdout}\n\nSTDERR:\n{completed.stderr}"
        log_path.write_text(combined, encoding="utf-8", errors="replace")
        if completed.returncode == 0:
            return CommandResult(command, VERIFIED, 0, duration, log_path)
        return CommandResult(
            command,
            FAILED,
            completed.returncode,
            duration,
            log_path,
            _runtime().summarize_failure(completed.stdout + "\n" + completed.stderr),
        )
    except subprocess.TimeoutExpired as exc:
        duration = time.monotonic() - start
        stdout = exc.stdout if isinstance(exc.stdout, str) else ""
        stderr = exc.stderr if isinstance(exc.stderr, str) else ""
        log_path.write_text(
            f"$ {command.display()}\n\nTIMEOUT after {command.timeout}s\n\nSTDOUT:\n{stdout}\n\nSTDERR:\n{stderr}",
            encoding="utf-8",
            errors="replace",
        )
        return CommandResult(
            command,
            INCONCLUSIVE,
            None,
            duration,
            log_path,
            "command timed out",
            timed_out=True,
        )
    except FileNotFoundError:
        # A missing binary is this command's failure, not a run-wide
        # TOOLING_ERROR.
        duration = time.monotonic() - start
        log_path.write_text(
            f"$ {command.display()}\n\nCOMMAND NOT FOUND: {args_list[0]}",
            encoding="utf-8",
            errors="replace",
        )
        return CommandResult(
            command,
            FAILED,
            None,
            duration,
            log_path,
            f"command not found: {args_list[0]}",
        )


def baseline_ref_for_scope(scope: Scope) -> str:
    active = {path.as_posix() for path in scope.active_files}
    dirty = {path.as_posix() for path in _runtime().dirty_files()}
    if active & dirty:
        return _runtime().current_head()
    parent = _runtime().git(["rev-parse", "--verify", "HEAD^"], check=False).strip()
    return parent or _runtime().current_head()


def baseline_command_result(command: CommandSpec, scope: Scope) -> str:
    base = _runtime().baseline_ref_for_scope(scope)
    temp_root = STATE_DIR / "baseline-worktrees"
    temp_root.mkdir(parents=True, exist_ok=True)
    worktree = temp_root / f"baseline-{os.getpid()}-{int(time.time())}"
    try:
        add = _runtime().run_capture(
            ["git", "worktree", "add", "--detach", str(worktree), base], timeout=120
        )
        if add.returncode != 0:
            return "inconclusive"
        cwd = (
            worktree
            if command.cwd.resolve() == REPO_ROOT.resolve()
            else worktree / _runtime().rel(command.cwd)
        )
        completed = _runtime().run_capture(
            command.args, cwd=cwd, timeout=command.timeout
        )
        return "pre-existing" if completed.returncode != 0 else "new"
    except Exception:
        return "inconclusive"
    finally:
        _runtime().run_capture(
            ["git", "worktree", "remove", "--force", str(worktree)], timeout=120
        )


def reached_test_execution(result: CommandResult) -> bool:
    if result.log_path is None or not result.log_path.exists():
        return False
    text = result.log_path.read_text(encoding="utf-8", errors="replace").lower()
    # The log always echoes the `just test-fast ...` command line, so bare
    # substrings like "test" are vacuously present. Only retry when the run
    # produced actual test-runner execution output, and never when the build
    # step failed with compiler errors (rustc emits both bare "error:" and
    # coded "error[E0308]:" forms).
    if re.search(r"error(\[e\d+\])?:", text):
        return False
    return bool(
        re.search(
            r"(running \d+ tests?|test result:|starting \d+ tests?"
            r"|\bpass \[|\bfail \[|\d+ passed)",
            text,
        )
    )


def should_retry_for_flake(result: CommandResult, args: argparse.Namespace) -> bool:
    return (
        args.retry_flakes
        and not args.no_cache
        and result.command.kind == "owner_test"
        and not result.timed_out
        and result.duration < result.command.timeout
        and _runtime().reached_test_execution(result)
    )


def result_to_json(result: CommandResult) -> dict[str, Any]:
    return {
        "id": result.command.id,
        "command": list(result.command.args),
        "status": result.status,
        "exit_code": result.exit_code,
        "duration": result.duration,
        "log_path": str(result.log_path) if result.log_path else None,
        "summary": result.summary,
        "timed_out": result.timed_out,
        "cached": result.cached,
        "flaky": result.flaky,
        "baseline": result.baseline,
    }


def scope_to_json(scope: Scope | None) -> dict[str, Any] | None:
    if scope is None:
        return None
    return {
        "scope_id": scope.scope_id,
        "source": scope.source,
        "active_files": [path.as_posix() for path in scope.active_files],
        "owned_packages": list(scope.owned_packages),
        "ignored_dirty_files": [path.as_posix() for path in scope.ignored_dirty_files],
        "adjacent_packages": list(scope.adjacent_packages),
        "stale_reasons": list(scope.stale_reasons),
        "dirty_groups": {
            key: [path.as_posix() for path in paths]
            for key, paths in scope.dirty_groups.items()
        },
        "surface_rules": list(scope.surface_rules),
    }


def ledger_entry(
    plan: Plan,
    results: list[CommandResult],
    verdict: str,
    cache_miss_reasons: list[str],
) -> dict[str, Any]:
    return {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "scope": _runtime().scope_to_json(plan.scope),
        "verdict": verdict,
        "results": [_runtime().result_to_json(result) for result in results],
        "skipped": plan.skipped,
        "cache_miss_reasons": cache_miss_reasons,
    }


def execute_plan(
    plan: Plan, graph: CargoGraph, args: argparse.Namespace
) -> tuple[str, list[CommandResult], list[str]]:
    if plan.verdict is not None:
        return plan.verdict, [], []
    if plan.scope is None:
        return TOOLING_ERROR, [], []
    if not plan.commands:
        return VERIFIED_NO_PROOF, [], []
    cache = _runtime().load_cache()
    entries = cache.setdefault("entries", {})
    results: list[CommandResult] = []
    cache_miss_reasons: list[str] = []
    for command in plan.commands:
        key, payload = _runtime().cache_key(command, plan.scope, graph)
        entry = entries.get(key)
        if not args.no_cache and entry and entry.get("status") == VERIFIED:
            results.append(
                CommandResult(
                    command,
                    VERIFIED,
                    0,
                    float(entry.get("duration", 0.0)),
                    Path(entry["log_path"]) if entry.get("log_path") else None,
                    cached=True,
                )
            )
            continue
        cache_miss_reasons.append(
            f"{command.id}: {'no cached green' if entry is None else 'input_hash_changed'}"
        )
        result = _runtime().execute_command(command)
        if result.status == FAILED and _runtime().should_retry_for_flake(result, args):
            retry = _runtime().execute_command(command)
            if retry.status == VERIFIED:
                retry.flaky = True
                result = retry
        results.append(result)
        if result.status != VERIFIED:
            if args.baseline:
                result.baseline = _runtime().baseline_command_result(
                    command, plan.scope
                )
            _runtime().append_ledger(
                _runtime().ledger_entry(
                    plan, results, result.status, cache_miss_reasons
                )
            )
            return result.status, results, cache_miss_reasons
        if not args.no_cache and not args.cache_readonly:
            entries[key] = {
                **payload,
                "status": VERIFIED,
                "duration": result.duration,
                "timestamp": datetime.now(timezone.utc).isoformat(),
                "log_path": str(result.log_path) if result.log_path else None,
            }
            _runtime().atomic_write_json(CACHE_PATH, cache)
    _runtime().append_ledger(
        _runtime().ledger_entry(plan, results, VERIFIED, cache_miss_reasons)
    )
    return VERIFIED, results, cache_miss_reasons


def print_plan(
    plan: Plan,
    *,
    verdict: str | None = None,
    results: list[CommandResult] | None = None,
    cache_miss_reasons: list[str] | None = None,
) -> None:
    scope = plan.scope
    if scope is None:
        print("No scope selected.")
        return
    print(f"Scope: {scope.scope_id}")
    print(f"Source: {scope.source}")
    print("Scope freshness: " + ("stale" if scope.stale_reasons else "ok"))
    for reason in scope.stale_reasons:
        print(f"- {reason}")
    if scope.active_files:
        print("Owned files:")
        for path in scope.active_files:
            print(f"- {path.as_posix()}")
    if scope.owned_packages:
        print("Owned packages:")
        for package in scope.owned_packages:
            print(f"- {package}")
    if scope.ignored_dirty_files:
        print("Ignored dirty files:")
        for path in scope.ignored_dirty_files[:20]:
            print(f"- {path.as_posix()}")
        if len(scope.ignored_dirty_files) > 20:
            print(f"- ... {len(scope.ignored_dirty_files) - 20} more")
    if scope.dirty_groups:
        print("Dirty groups:")
        for group, paths in scope.dirty_groups.items():
            print(f"- {group}: {len(paths)} file(s)")
    if plan.enabled_expansions:
        print("Enabled expansion flags:")
        for flag in plan.enabled_expansions:
            print(f"- {flag}")
    if plan.commands:
        print("Planned commands:")
        for command in plan.commands:
            print(f"- {command.display()}")
            if command.reason:
                print(f"  why: {command.reason}")
    if plan.skipped:
        print("Skipped:")
        for skipped in plan.skipped:
            print(f"- {skipped['item']}: {skipped['reason']}")
    if results:
        print("Results:")
        for result in results:
            cached = "cached " if result.cached else ""
            flaky = " (passed on retry; flaky)" if result.flaky else ""
            print(
                f"- {cached}{result.command.display()}: {result.status}{flaky} in {result.duration:.1f}s"
            )
            if result.log_path:
                print(f"  log: {result.log_path}")
            if result.baseline:
                print(f"  baseline: {result.baseline}")
            if result.summary:
                print("  first useful output:")
                for line in result.summary.splitlines()[:8]:
                    print(f"    {line}")
                print(f"  rerun: {result.command.display()}")
                break
    if cache_miss_reasons:
        print("Cache miss reasons:")
        for reason in cache_miss_reasons:
            print(f"- {reason}")
    print(
        "Stop condition: stop after terminal verifier verdict; do not run related tests manually."
    )
    print(f"Verdict: {verdict or plan.verdict or 'PLANNED'}")


def plan_to_json(
    plan: Plan,
    verdict: str | None,
    results: list[CommandResult],
    cache_miss_reasons: list[str],
) -> dict[str, Any]:
    return {
        "scope": _runtime().scope_to_json(plan.scope),
        "planned": [
            {
                "id": c.id,
                "kind": c.kind,
                "command": list(c.args),
                "reason": c.reason,
                "owner_packages": list(c.owner_packages),
            }
            for c in plan.commands
        ],
        "skipped": plan.skipped,
        "results": [_runtime().result_to_json(result) for result in results],
        "cached": [
            _runtime().result_to_json(result) for result in results if result.cached
        ],
        "quarantined_failures": [],
        "rerun": next(
            (
                result.command.display()
                for result in results
                if result.status != VERIFIED
            ),
            None,
        ),
        "cache_miss_reasons": cache_miss_reasons,
        "verdict": verdict or plan.verdict or "PLANNED",
    }
