#!/usr/bin/env python3
"""Scope-locked local verification router for this checkout."""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
from datetime import datetime, timezone
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
from shutil import which
import signal
import subprocess
import sys
import time
from typing import Any, Iterable, Sequence
from urllib.parse import unquote, urlparse


REPO_ROOT = Path(__file__).resolve().parents[1]
CODEX_RS = REPO_ROOT / "codex-rs"
STATE_DIR = REPO_ROOT / ".codex" / "verify-local"
SCOPE_PATH = STATE_DIR / "scope.json"
CACHE_PATH = STATE_DIR / "cache.json"
LEDGER_PATH = STATE_DIR / "ledger.json"
LOG_DIR = STATE_DIR / "logs"
RULES_PATH = REPO_ROOT / "scripts" / "verify_local_rules.toml"
VERIFY_LOCAL_CONTROL_PATHS = {
    "justfile",
    "scripts/verify_local_rules.toml",
}
POWERSHELL_PARSE_SCRIPT = (
    "& { "
    "param($path) "
    "$tokens = $null; "
    "$errors = $null; "
    "$resolved = (Resolve-Path -LiteralPath $path).Path; "
    "[System.Management.Automation.Language.Parser]::ParseFile("
    "$resolved, [ref]$tokens, [ref]$errors) > $null; "
    "if ($errors.Count -gt 0) { "
    "$errors | ForEach-Object { Write-Error $_ }; "
    "exit 1 "
    "}"
    " }"
)
BASH_PARSE_SCRIPT_TEMPLATE = "set -o pipefail; sed 's/\\r$//' {path} | bash -n"

VERIFIED = "VERIFIED"
VERIFIED_NO_PROOF = "VERIFIED (no proof needed)"
FAILED = "FAILED"
INCONCLUSIVE = "INCONCLUSIVE"
NEEDS_SCOPE = "NEEDS_SCOPE"
TOOLING_ERROR = "TOOLING_ERROR"
NEEDS_REGEN = "NEEDS_REGEN"

EXIT_CODES = {
    VERIFIED: 0,
    VERIFIED_NO_PROOF: 0,
    FAILED: 1,
    INCONCLUSIVE: 2,
    NEEDS_SCOPE: 3,
    TOOLING_ERROR: 4,
    NEEDS_REGEN: 5,
}

TIMEOUTS = {
    "owner_test": 300,
    "owner_check": 180,
    "script": 180,
    "formatter": 180,
    "hygiene": 30,
    "schema": 900,
}

IGNORED_DIR_PARTS = {
    ".git",
    ".cache",
    ".turbo",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "__pycache__",
}

if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))
from scripts import rust_packages  # noqa: E402


@dataclass(frozen=True)
class PackageInfo:
    name: str
    root: Path
    manifest: Path
    package_id: str | None = None


@dataclass
class CargoGraph:
    packages_by_name: dict[str, PackageInfo] = field(default_factory=dict)
    packages_by_id: dict[str, PackageInfo] = field(default_factory=dict)
    deps_by_name: dict[str, set[str]] = field(default_factory=dict)
    reverse_deps_by_name: dict[str, set[str]] = field(default_factory=dict)

    def package_for_root(self, root: Path) -> PackageInfo | None:
        resolved = root.resolve()
        for package in self.packages_by_name.values():
            if package.root.resolve() == resolved:
                return package
        return None

    def transitive_workspace_deps(self, names: Iterable[str]) -> set[str]:
        seen: set[str] = set()
        stack = list(names)
        while stack:
            current = stack.pop()
            for dep in self.deps_by_name.get(current, set()):
                if dep not in seen:
                    seen.add(dep)
                    stack.append(dep)
        return seen

    def direct_reverse_deps(self, names: Iterable[str]) -> set[str]:
        result: set[str] = set()
        for name in names:
            result.update(self.reverse_deps_by_name.get(name, set()))
        return result


@dataclass(frozen=True)
class SurfaceRule:
    id: str
    paths: tuple[str, ...]
    owned_packages: tuple[str, ...]
    test_expr: str | None = None
    validation_command: tuple[str, ...] | None = None
    regen_command: tuple[str, ...] | None = None
    skip_owner_tests: bool = False
    # Extra cache inputs beyond `paths`: mirror the input list the surface's
    # checker script inspects so a cached green cannot survive an input change
    # that sits outside the active scope.
    hash_paths: tuple[str, ...] = ()


@dataclass
class Scope:
    scope_id: str
    source: str
    active_files: list[Path]
    owned_packages: list[str]
    ignored_dirty_files: list[Path] = field(default_factory=list)
    adjacent_packages: list[str] = field(default_factory=list)
    stale_reasons: list[str] = field(default_factory=list)
    dirty_groups: dict[str, list[Path]] = field(default_factory=dict)
    surface_rules: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class CommandSpec:
    id: str
    kind: str
    args: tuple[str, ...]
    cwd: Path = REPO_ROOT
    timeout: int = 300
    owner_packages: tuple[str, ...] = ()
    hash_paths: tuple[str, ...] = ()
    reason: str = ""

    def display(self) -> str:
        return shell_join(self.args)


@dataclass
class CommandResult:
    command: CommandSpec
    status: str
    exit_code: int | None
    duration: float
    log_path: Path | None = None
    summary: str = ""
    timed_out: bool = False
    cached: bool = False
    flaky: bool = False
    baseline: str | None = None


@dataclass
class Plan:
    mode: str
    scope: Scope | None
    commands: list[CommandSpec]
    skipped: list[dict[str, str]]
    verdict: str | None = None
    enabled_expansions: list[str] = field(default_factory=list)


def rel(path: Path) -> str:
    try:
        return path.resolve().relative_to(REPO_ROOT.resolve()).as_posix()
    except ValueError:
        return path.as_posix()


def shell_join(args: Sequence[str]) -> str:
    rendered: list[str] = []
    for arg in args:
        if (
            not arg
            or any(ch.isspace() for ch in arg)
            or any(ch in arg for ch in "'\"|&<>()")
        ):
            rendered.append("'" + arg.replace("'", "''") + "'")
        else:
            rendered.append(arg)
    return " ".join(rendered)


def kill_process_tree(process: subprocess.Popen) -> None:
    if os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    else:
        try:
            os.killpg(os.getpgid(process.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
    try:
        process.kill()
    except OSError:
        pass


def run_capture(
    args: Sequence[str],
    *,
    cwd: Path = REPO_ROOT,
    timeout: int = 120,
    check: bool = False,
) -> subprocess.CompletedProcess[str]:
    # Kill the whole process tree on timeout: recipes fan out just -> shell
    # adapter -> pwsh -> cargo, and grandchildren holding the inherited pipe
    # handles would otherwise block communicate() forever after the direct
    # child is killed.
    popen_kwargs: dict[str, Any] = {}
    if os.name != "nt":
        popen_kwargs["start_new_session"] = True
    process = subprocess.Popen(
        list(args),
        cwd=cwd,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        **popen_kwargs,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        kill_process_tree(process)
        try:
            stdout, stderr = process.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", ""
        raise subprocess.TimeoutExpired(
            list(args), timeout, output=stdout, stderr=stderr
        ) from None
    completed = subprocess.CompletedProcess(
        list(args), process.returncode, stdout, stderr
    )
    if check and completed.returncode != 0:
        raise subprocess.CalledProcessError(
            completed.returncode,
            list(args),
            output=completed.stdout,
            stderr=completed.stderr,
        )
    return completed


READ_ONLY_GIT_SUBCOMMANDS = {"diff", "ls-files", "merge-base", "rev-parse", "status"}
DUBIOUS_OWNERSHIP_MARKERS = (
    "detected dubious ownership",
    "safe.directory",
)


def git_subcommand(args: Sequence[str]) -> str | None:
    index = 0
    while index < len(args):
        arg = args[index]
        if arg in {"-C", "-c"}:
            index += 2
            continue
        if arg.startswith("-c") and arg != "-c":
            index += 1
            continue
        if arg.startswith("-"):
            index += 1
            continue
        return arg
    return None


def git_is_read_only_inspection(args: Sequence[str]) -> bool:
    subcommand = git_subcommand(args)
    if subcommand == "branch":
        return "--show-current" in args
    return subcommand in READ_ONLY_GIT_SUBCOMMANDS


def git_failed_for_dubious_ownership(
    completed: subprocess.CompletedProcess[str],
) -> bool:
    if completed.returncode == 0:
        return False
    output = f"{completed.stdout}\n{completed.stderr}".lower()
    return any(marker in output for marker in DUBIOUS_OWNERSHIP_MARKERS)


def git_command(args: Sequence[str], *, safe_directory: bool = False) -> list[str]:
    command = ["git"]
    if safe_directory:
        command.extend(["-c", f"safe.directory={REPO_ROOT}"])
    command.extend(args)
    return command


def git_capture(
    args: Sequence[str], *, timeout: int = 60, check: bool = True
) -> subprocess.CompletedProcess[str]:
    completed = run_capture(git_command(args), timeout=timeout, check=False)
    if git_failed_for_dubious_ownership(completed) and git_is_read_only_inspection(
        args
    ):
        completed = run_capture(
            git_command(args, safe_directory=True), timeout=timeout, check=False
        )
    if check and completed.returncode != 0:
        raise subprocess.CalledProcessError(
            completed.returncode,
            completed.args,
            output=completed.stdout,
            stderr=completed.stderr,
        )
    return completed


def git(args: Sequence[str], *, timeout: int = 60, check: bool = True) -> str:
    return git_capture(args, timeout=timeout, check=check).stdout


def current_branch() -> str:
    return (
        git(["branch", "--show-current"], check=False).strip()
        or git(["rev-parse", "--short", "HEAD"], check=False).strip()
    )


def current_head() -> str:
    return git(["rev-parse", "HEAD"], check=False).strip()


def is_ancestor(base: str, head: str = "HEAD") -> bool:
    return (
        bool(base)
        and git_capture(
            ["merge-base", "--is-ancestor", base, head], timeout=30, check=False
        ).returncode
        == 0
    )


def normalize_path(value: str, *, strip_outer: bool = True) -> Path:
    raw = value.strip().strip('"') if strip_outer else value
    if raw.startswith("file:"):
        parsed = urlparse(raw)
        raw = unquote(parsed.path)
        if os.name == "nt" and raw.startswith("/") and len(raw) > 2 and raw[2] == ":":
            raw = raw[1:]
    raw = raw.replace("\\", os.sep).replace("/", os.sep)
    path = Path(raw)
    if not path.is_absolute():
        path = REPO_ROOT / path
    try:
        return path.resolve().relative_to(REPO_ROOT.resolve())
    except ValueError as exc:
        raise ValueError(f"path is outside repository: {value}") from exc


def normalize_paths(values: Iterable[str], *, strip_outer: bool = True) -> list[Path]:
    seen: set[str] = set()
    result: list[Path] = []
    for value in values:
        path = normalize_path(value, strip_outer=strip_outer)
        key = path.as_posix().lower() if os.name == "nt" else path.as_posix()
        if key not in seen:
            seen.add(key)
            result.append(path)
    return result


def stable_unique(paths: Iterable[Path]) -> list[Path]:
    seen: set[str] = set()
    result: list[Path] = []
    for path in paths:
        key = path.as_posix().lower() if os.name == "nt" else path.as_posix()
        if key not in seen:
            seen.add(key)
            result.append(path)
    return result


def path_id(path: Path) -> str:
    return "".join(ch if ch.isalnum() else "-" for ch in path.as_posix()).strip("-")


def bash_parse_script(path: Path) -> str:
    return BASH_PARSE_SCRIPT_TEMPLATE.format(path=shlex.quote(path.as_posix()))


def git_name_list(args: Sequence[str]) -> list[Path]:
    # core.quotepath=off keeps non-ASCII filenames as raw UTF-8 instead of
    # C-quoted octal escapes, which normalize_path would mangle into garbage
    # multi-component paths.
    output = git(["-c", "core.quotepath=off", *args, "-z"], check=False)
    delimiter = "\0" if "\0" in output else "\n"
    return normalize_paths(
        (path for path in output.split(delimiter) if path), strip_outer=False
    )


def staged_files() -> list[Path]:
    # Include deletions (D): a pure `git rm` is a change that needs proof;
    # excluding it yielded an empty scope and a vacuous VERIFIED.
    return git_name_list(["diff", "--cached", "--name-only", "--diff-filter=ACMRTD"])


def unstaged_files() -> list[Path]:
    return git_name_list(["diff", "--name-only", "--diff-filter=ACMRTD"])


def untracked_files() -> list[Path]:
    return git_name_list(["ls-files", "--others", "--exclude-standard"])


def dirty_files() -> list[Path]:
    return stable_unique([*staged_files(), *unstaged_files(), *untracked_files()])


def parse_last_json_value(output: str) -> Any:
    try:
        return json.loads(output)
    except json.JSONDecodeError:
        pass

    decoder = json.JSONDecoder()
    for line in reversed(output.splitlines()):
        stripped = line.strip()
        if not stripped or stripped[0] not in "[{":
            continue
        try:
            return json.loads(stripped)
        except json.JSONDecodeError:
            continue

    starts = [
        index
        for index, char in enumerate(output)
        if char in "[{" and (index == 0 or output[index - 1] == "\n")
    ]
    for start in reversed(starts):
        candidate = output[start:].lstrip()
        try:
            value, _end = decoder.raw_decode(candidate)
        except json.JSONDecodeError:
            continue
        return value

    preview = output.strip().replace("\n", "\\n")
    if len(preview) > 240:
        preview = preview[:237] + "..."
    raise ValueError(f"could not parse JSON from command output: {preview}")


def load_cargo_metadata() -> CargoGraph:
    completed = run_capture(
        ["cargo", "metadata", "--format-version", "1"],
        cwd=CODEX_RS,
        timeout=120,
        check=True,
    )
    metadata = parse_last_json_value(completed.stdout)
    workspace_ids = set(metadata.get("workspace_members", []))
    packages_by_id: dict[str, PackageInfo] = {}
    packages_by_name: dict[str, PackageInfo] = {}
    for package in metadata.get("packages", []):
        package_id = package["id"]
        if package_id not in workspace_ids:
            continue
        manifest = Path(package["manifest_path"]).resolve()
        info = PackageInfo(package["name"], manifest.parent, manifest, package_id)
        packages_by_id[package_id] = info
        packages_by_name[info.name] = info

    deps_by_name: dict[str, set[str]] = {name: set() for name in packages_by_name}
    reverse_deps_by_name: dict[str, set[str]] = {
        name: set() for name in packages_by_name
    }
    for node in (metadata.get("resolve") or {}).get("nodes", []):
        package = packages_by_id.get(node.get("id"))
        if package is None:
            continue
        for dep in node.get("deps", []):
            dep_info = packages_by_id.get(dep.get("pkg"))
            if dep_info is None:
                continue
            deps_by_name[package.name].add(dep_info.name)
            reverse_deps_by_name[dep_info.name].add(package.name)
    return CargoGraph(
        packages_by_name=packages_by_name,
        packages_by_id=packages_by_id,
        deps_by_name=deps_by_name,
        reverse_deps_by_name=reverse_deps_by_name,
    )


def path_matches_rule_pattern(path: str, pattern: str) -> bool:
    return (
        path == pattern
        or path.startswith(pattern.rstrip("/") + "/")
        or fnmatch.fnmatch(path, pattern)
    )


def load_rules() -> list[SurfaceRule]:
    if not RULES_PATH.exists():
        return []
    import tomllib

    data = tomllib.loads(RULES_PATH.read_text(encoding="utf-8"))
    rules: list[SurfaceRule] = []
    for entry in data.get("surface", []):
        if isinstance(entry, dict) and isinstance(entry.get("id"), str):
            rules.append(
                SurfaceRule(
                    id=entry["id"],
                    paths=tuple(
                        str(path).replace("\\", "/") for path in entry.get("paths", [])
                    ),
                    owned_packages=tuple(
                        str(package) for package in entry.get("owned_packages", [])
                    ),
                    test_expr=entry.get("test_expr")
                    if isinstance(entry.get("test_expr"), str)
                    else None,
                    validation_command=tuple(
                        str(part) for part in entry["validation_command"]
                    )
                    if isinstance(entry.get("validation_command"), list)
                    else None,
                    regen_command=tuple(str(part) for part in entry["regen_command"])
                    if isinstance(entry.get("regen_command"), list)
                    else None,
                    skip_owner_tests=bool(entry.get("skip_owner_tests", False)),
                    hash_paths=tuple(
                        str(path).replace("\\", "/")
                        for path in entry.get("hash_paths", [])
                    ),
                )
            )
    return rules


def matching_rules(
    paths: Iterable[Path], rules: Sequence[SurfaceRule]
) -> list[SurfaceRule]:
    rels = [path.as_posix() for path in paths]
    matches: list[SurfaceRule] = []
    for rule in rules:
        if any(
            path_matches_rule_pattern(path, pattern)
            for path in rels
            for pattern in rule.paths
        ):
            matches.append(rule)
    return matches


def package_for_path(path: Path, graph: CargoGraph) -> PackageInfo | None:
    root = rust_packages.nearest_package_root(
        REPO_ROOT / path, repo_root=REPO_ROOT, assume_file=True
    )
    if root is None:
        return None
    package = graph.package_for_root(root)
    if package is not None:
        return package
    name = rust_packages.package_name(root / "Cargo.toml")
    return PackageInfo(name, root, root / "Cargo.toml") if name else None


def owner_packages(
    paths: list[Path], graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> list[str]:
    packages: set[str] = set()
    for rule in matching_rules(paths, rules):
        packages.update(rule.owned_packages)
    for path in paths:
        package = package_for_path(path, graph)
        if package is not None:
            packages.add(package.name)
    return sorted(packages)


def is_ignored_build_output(path: Path) -> bool:
    return any(part in IGNORED_DIR_PARTS for part in path.parts)


def classify_dirty_group(
    path: Path, graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> str:
    for rule in matching_rules([path], rules):
        return f"contract:{rule.id}"
    if path.as_posix() in {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "justfile",
    }:
        return f"contract:{path.as_posix()}"
    package = package_for_path(path, graph)
    if package is not None:
        return f"package:{package.name}"
    return f"area:{path.parts[0]}" if path.parts else "root"


def group_dirty_files(
    paths: Iterable[Path], graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> dict[str, list[Path]]:
    groups: dict[str, list[Path]] = {}
    for path in paths:
        if is_ignored_build_output(path):
            continue
        groups.setdefault(classify_dirty_group(path, graph, rules), []).append(path)
    return groups


def atomic_write_json(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lock = path.with_suffix(path.suffix + ".lock")
    start = time.monotonic()
    fd: int | None = None
    while fd is None:
        try:
            fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        except FileExistsError:
            # A hard-killed run leaves its lock behind forever; break locks
            # that are clearly stale instead of erroring every later run.
            try:
                if time.time() - lock.stat().st_mtime > 300:
                    lock.unlink()
                    continue
            except OSError:
                pass
            if time.monotonic() - start > 10:
                raise TimeoutError(f"timed out waiting for lock: {lock}")
            time.sleep(0.05)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(str(os.getpid()))
        temp = path.with_name(path.name + f".{os.getpid()}.tmp")
        temp.write_text(
            json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        os.replace(temp, path)
    finally:
        try:
            lock.unlink()
        except FileNotFoundError:
            pass


def scope_state() -> dict[str, Any] | None:
    if not SCOPE_PATH.exists():
        return None
    try:
        return json.loads(SCOPE_PATH.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None


def scope_name_for_paths(paths: Sequence[Path]) -> str:
    if not paths:
        return "empty"
    digest = hashlib.sha256(
        "\n".join(path.as_posix() for path in paths).encode("utf-8")
    ).hexdigest()[:10]
    first = paths[0].stem or paths[0].name or "scope"
    return f"{first}-{digest}"


def create_scope(
    name: str, paths: list[Path], graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> dict[str, Any]:
    state = {
        "scope_id": name,
        "branch": current_branch(),
        "base_commit": current_head(),
        "owned_paths": [path.as_posix() for path in paths],
        "owned_packages": owner_packages(paths, graph, rules),
        "created_at": datetime.now(timezone.utc).isoformat(),
        "initial_dirty_paths": [path.as_posix() for path in dirty_files()],
    }
    atomic_write_json(SCOPE_PATH, state)
    return state


def scope_stale_reasons(state: dict[str, Any], graph: CargoGraph) -> list[str]:
    reasons: list[str] = []
    if state.get("branch") and state.get("branch") != current_branch():
        reasons.append("branch changed since scope creation")
    base = str(state.get("base_commit", ""))
    if base and not is_ancestor(base):
        reasons.append("base commit is no longer an ancestor of HEAD")
    owned_paths = [Path(path) for path in state.get("owned_paths", [])]
    dirty = {path.as_posix() for path in dirty_files()}
    if owned_paths and not any(path.as_posix() in dirty for path in owned_paths):
        reasons.append("none of the scoped files are currently dirty or staged")
    if len(dirty) >= 4:
        outside = [path for path in dirty if Path(path) not in owned_paths]
        if len(outside) > len(dirty) // 2:
            reasons.append("current dirty files are mostly outside the sticky scope")
    for package in state.get("owned_packages", []):
        if package not in graph.packages_by_name:
            reasons.append(f"owned package no longer exists: {package}")
    for path in owned_paths:
        if not (REPO_ROOT / path).exists() and path.as_posix() not in dirty:
            reasons.append(
                f"scope references deleted path without replacement: {path.as_posix()}"
            )
    return reasons


def build_scope(
    scope_id: str,
    source: str,
    active_files: list[Path],
    all_dirty: list[Path],
    graph: CargoGraph,
    rules: Sequence[SurfaceRule],
) -> Scope:
    active = stable_unique(active_files)
    packages = owner_packages(active, graph, rules)
    active_set = {path.as_posix() for path in active}
    ignored = [
        path
        for path in all_dirty
        if path.as_posix() not in active_set and not is_ignored_build_output(path)
    ]
    adjacent = sorted(graph.direct_reverse_deps(packages) - set(packages))
    return Scope(
        scope_id=scope_id,
        source=source,
        active_files=active,
        owned_packages=packages,
        ignored_dirty_files=ignored,
        adjacent_packages=adjacent,
        surface_rules=[rule.id for rule in matching_rules(active, rules)],
    )


def select_scope(
    args: argparse.Namespace, graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> tuple[Scope | None, str | None]:
    all_dirty = dirty_files()
    if args.scope_reset:
        if SCOPE_PATH.exists():
            SCOPE_PATH.unlink()
        return Scope("scope-reset", "scope-reset", [], []), None
    if args.scope_start:
        changed = normalize_paths(args.changed or [])
        if not changed:
            return None, "--scope-start requires at least one --changed path"
        state = create_scope(args.scope_start, changed, graph, rules)
        return build_scope(
            state["scope_id"], "scope-start", changed, all_dirty, graph, rules
        ), None
    if args.scope_add:
        state = scope_state()
        if state is None:
            return None, "--scope-add requires an existing sticky scope"
        additions = normalize_paths(args.scope_add)
        owned = stable_unique(
            [Path(path) for path in state.get("owned_paths", [])] + additions
        )
        state["owned_paths"] = [path.as_posix() for path in owned]
        state["owned_packages"] = owner_packages(owned, graph, rules)
        atomic_write_json(SCOPE_PATH, state)
        return build_scope(
            str(state.get("scope_id", "current")),
            "scope-add",
            owned,
            all_dirty,
            graph,
            rules,
        ), None
    if args.changed:
        paths = normalize_paths(args.changed)
        return build_scope(
            scope_name_for_paths(paths), "changed", paths, all_dirty, graph, rules
        ), None
    if args.scope == "current":
        state = scope_state()
        if state is None:
            return None, "no sticky scope exists"
        paths = [Path(path) for path in state.get("owned_paths", [])]
        scope = build_scope(
            str(state.get("scope_id", "current")),
            "scope-current",
            paths,
            all_dirty,
            graph,
            rules,
        )
        scope.stale_reasons = scope_stale_reasons(state, graph)
        if scope.stale_reasons:
            return scope, "sticky scope appears stale"
        return scope, None
    staged = staged_files()
    if args.staged:
        if not staged:
            return None, "--staged selected but no staged files were found"
        return build_scope(
            scope_name_for_paths(staged), "staged", staged, all_dirty, graph, rules
        ), None
    if args.all_dirty:
        # An explicit --all-dirty must win over the implicit staged-first
        # default, otherwise staged files silently shrink the requested scope.
        return build_scope("all-dirty", "all-dirty", all_dirty, [], graph, rules), None
    if staged:
        return build_scope(
            scope_name_for_paths(staged), "staged", staged, all_dirty, graph, rules
        ), None
    groups = group_dirty_files(all_dirty, graph, rules)
    if len(groups) == 1:
        paths = next(iter(groups.values()))
        return build_scope(
            scope_name_for_paths(paths), "single-dirty-group", paths, [], graph, rules
        ), None
    if not groups:
        return Scope("empty", "empty", [], []), None
    return Scope(
        "needs-scope", "dirty-groups", [], [], dirty_groups=groups
    ), "multiple dirty groups detected"


def enabled_expansions(args: argparse.Namespace) -> list[str]:
    result: list[str] = []
    for name in [
        "related",
        "related_tests",
        "allow_workspace",
        "all_dirty",
        "isolated",
        "regen",
        "baseline",
        "no_cache",
        "cache_readonly",
    ]:
        if getattr(args, name, False):
            result.append("--" + name.replace("_", "-"))
    return result


def test_exprs_for_scope(scope: Scope, rules: Sequence[SurfaceRule]) -> list[str]:
    return [
        rule.test_expr
        for rule in rules
        if rule.id in scope.surface_rules and rule.test_expr
    ]


def active_scope_has_tests(scope: Scope) -> bool:
    return any(
        "_test" in path.stem or path.stem == "tests" or "/tests/" in path.as_posix()
        for path in scope.active_files
    )


def owner_commands(
    packages: list[str],
    args: argparse.Namespace,
    scope: Scope,
    rules: Sequence[SurfaceRule],
) -> list[CommandSpec]:
    if not packages:
        return []
    exprs = test_exprs_for_scope(scope, rules)
    if (
        args.fast
        and not args.isolated
        and not exprs
        and not active_scope_has_tests(scope)
    ):
        return [
            CommandSpec(
                id=f"owner-check:{package}",
                kind="owner_check",
                args=("just", "check-lane", package),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(package,),
                reason="fast owner package compile proof",
            )
            for package in packages
        ]
    if args.isolated:
        return [
            CommandSpec(
                id=f"owner-test:{package}",
                kind="owner_test",
                args=("just", "test-lane-package", package),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(package,),
                reason="isolated owner package proof",
            )
            for package in packages
        ]
    if len(packages) == 1 and exprs:
        command_args = ["just", "test-fast", "-p", packages[0]]
        command_args.extend(
            ["-E", " | ".join(f"({expr.strip()})" for expr in exprs if expr.strip())]
        )
        return [
            CommandSpec(
                id=f"owner-test:{packages[0]}",
                kind="owner_test",
                args=tuple(command_args),
                timeout=TIMEOUTS["owner_test"],
                owner_packages=(packages[0],),
                reason="owner package proof",
            )
        ]
    return [
        CommandSpec(
            id=f"owner-test:{package}",
            kind="owner_test",
            args=("just", "test-fast", "-p", package),
            timeout=TIMEOUTS["owner_test"],
            owner_packages=(package,),
            reason="owner package proof",
        )
        for package in packages
    ]


def hygiene_command(scope: Scope) -> CommandSpec | None:
    paths = list(scope.active_files)
    if not paths:
        return None
    return CommandSpec(
        id="hygiene:diff-check",
        kind="hygiene",
        # Diff against HEAD so staged changes are checked too: the default
        # scope source is staged files, and a plain worktree-vs-index diff is
        # empty exactly for those.
        args=(
            "git",
            "diff",
            "--check",
            "HEAD",
            "--",
            *(path.as_posix() for path in paths),
        ),
        timeout=TIMEOUTS["hygiene"],
        reason="scoped diff whitespace check",
    )


def fmt_check_command() -> CommandSpec:
    return CommandSpec(
        id="formatter:fmt-check-fast",
        kind="formatter",
        args=("just", "fmt-check-fast"),
        timeout=TIMEOUTS["formatter"],
        reason="check-only fast Rust/just formatter",
    )


def python_executable() -> str:
    executable = Path(sys.executable)
    if executable.is_absolute():
        return str(executable)
    return which(sys.executable) or sys.executable


def root_maintenance_args(*args: str) -> tuple[str, ...]:
    return (python_executable(), "-m", "scripts.root_maintenance", *args)


def script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".py"
    ]


def powershell_script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".ps1"
    ]


def shell_script_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.parts and path.parts[0] == "scripts" and path.suffix == ".sh"
    ]


def verify_local_control_paths(scope: Scope) -> list[Path]:
    return [
        path
        for path in scope.active_files
        if path.as_posix() in VERIFY_LOCAL_CONTROL_PATHS
    ]


def justfile_check_command(scope: Scope) -> CommandSpec | None:
    if not any(path.as_posix() == "justfile" for path in scope.active_files):
        return None
    return CommandSpec(
        id="justfile:summary",
        kind="justfile_check",
        args=("just", "--summary"),
        timeout=TIMEOUTS["hygiene"],
        reason="parse justfile recipes",
    )


def script_validation_commands(scope: Scope) -> list[CommandSpec]:
    from scripts.root_maintenance import test_modules_for_changed_path

    paths = script_paths(scope)
    ps_paths = powershell_script_paths(scope)
    sh_paths = shell_script_paths(scope)
    control_paths = verify_local_control_paths(scope)
    if not paths and not ps_paths and not sh_paths and not control_paths:
        return []
    changed_args: list[str] = []
    for path in paths:
        changed_args.extend(["--changed", path.as_posix()])
    commands: list[CommandSpec] = []
    for path in ps_paths:
        commands.append(
            CommandSpec(
                id=f"script-syntax:powershell:{path_id(path)}",
                kind="script_syntax",
                args=(
                    "pwsh",
                    "-NoProfile",
                    "-Command",
                    POWERSHELL_PARSE_SCRIPT,
                    path.as_posix(),
                ),
                timeout=TIMEOUTS["script"],
                reason="PowerShell script parse check",
            )
        )
    for path in sh_paths:
        commands.append(
            CommandSpec(
                id=f"script-syntax:shell:{path_id(path)}",
                kind="script_syntax",
                args=("bash", "-lc", bash_parse_script(path)),
                timeout=TIMEOUTS["script"],
                reason="shell script parse check",
            )
        )
    if paths:
        commands.append(
            CommandSpec(
                id="script-lint:" + "+".join(path.stem for path in paths),
                kind="script_lint",
                args=root_maintenance_args(
                    "lint-python",
                    *changed_args,
                ),
                timeout=TIMEOUTS["script"],
                reason="scoped Python script lint check",
            )
        )
    test_paths = [
        path
        for path in (*paths, *ps_paths, *sh_paths)
        if test_modules_for_changed_path(path.as_posix())
    ]
    if not test_paths and control_paths:
        commands.append(
            CommandSpec(
                id="script-test:verify_local_controls",
                kind="script_test",
                args=root_maintenance_args(
                    "test-python",
                    "--module",
                    "scripts.test_verify_local",
                ),
                timeout=TIMEOUTS["script"],
                reason="nearest verify-local router tests",
            )
        )
        return commands
    if not test_paths:
        return commands
    test_changed_args: list[str] = []
    for path in test_paths:
        test_changed_args.extend(["--changed", path.as_posix()])
    commands.append(
        CommandSpec(
            id="script-test:" + "+".join(path.stem for path in test_paths),
            kind="script_test",
            args=root_maintenance_args(
                "test-python",
                *test_changed_args,
            ),
            timeout=TIMEOUTS["script"],
            reason="nearest Python script tests",
        )
    )
    return commands


def needs_python_formatter(path: Path) -> bool:
    return path.parts and path.parts[0] == "scripts" and path.suffix == ".py"


def needs_prettier_formatter(path: Path) -> bool:
    text = path.as_posix()
    return (
        text
        in {"package.json", "knip.json", "pnpm-workspace.yaml", "eslint.config.mjs"}
        or fnmatch.fnmatch(text, "docs/*.md")
        or fnmatch.fnmatch(text, ".github/workflows/*.yml")
        or (text.startswith("codex-cli/") and text.endswith(".js"))
        or (
            text.startswith("sdk/typescript/")
            and (text.endswith(".js") or text.endswith(".ts"))
        )
    )


def final_formatter_commands(scope: Scope) -> list[CommandSpec]:
    commands: list[CommandSpec] = []
    if any(
        path.as_posix().startswith("codex-rs/") or path.as_posix() == "justfile"
        for path in scope.active_files
    ):
        commands.append(fmt_check_command())
    if any(needs_python_formatter(path) for path in scope.active_files):
        changed_args: list[str] = []
        for path in script_paths(scope):
            changed_args.extend(["--changed", path.as_posix()])
        commands.append(
            CommandSpec(
                id="formatter:format-python",
                kind="formatter",
                args=root_maintenance_args(
                    "format-python",
                    *changed_args,
                ),
                timeout=TIMEOUTS["formatter"],
                reason="check-only Python script formatter",
            )
        )
    if any(needs_prettier_formatter(path) for path in scope.active_files):
        commands.append(
            CommandSpec(
                id="formatter:format-prettier",
                kind="formatter",
                args=root_maintenance_args("format-prettier"),
                timeout=TIMEOUTS["formatter"],
                reason="check-only Prettier formatter",
            )
        )
    return commands


def adjacent_commands(packages: list[str]) -> list[CommandSpec]:
    return [
        CommandSpec(
            id=f"adjacent-check:{package}",
            kind="owner_check",
            args=("just", "check-lane", package),
            timeout=TIMEOUTS["owner_check"],
            owner_packages=(package,),
            reason="explicit adjacent compile check",
        )
        for package in packages[:3]
    ]


def related_test_commands(packages: list[str]) -> list[CommandSpec]:
    return [
        CommandSpec(
            id=f"related-test:{package}",
            kind="related_test",
            args=("just", "test-lane-package", package),
            timeout=TIMEOUTS["owner_test"],
            owner_packages=(package,),
            reason="explicit related package test proof",
        )
        for package in packages[:3]
    ]


def surface_commands(
    args: argparse.Namespace, scope: Scope, rules: Sequence[SurfaceRule]
) -> list[CommandSpec]:
    commands: list[CommandSpec] = []
    for rule in rules:
        if rule.id not in scope.surface_rules:
            continue
        command_args = (
            rule.regen_command
            if args.regen and rule.regen_command
            else rule.validation_command
        )
        if command_args is None:
            continue
        regenerating = args.regen and rule.regen_command is not None
        commands.append(
            CommandSpec(
                id=f"surface:{rule.id}:{'regen' if regenerating else 'validate'}",
                kind="surface_regen" if regenerating else "surface_validation",
                args=command_args,
                timeout=TIMEOUTS["schema"],
                owner_packages=rule.owned_packages,
                hash_paths=tuple(dict.fromkeys(rule.paths + rule.hash_paths)),
                reason=f"{rule.id} surface {'regeneration and validation' if regenerating else 'validation'}",
            )
        )
    return commands


def plan_commands(
    args: argparse.Namespace, scope: Scope, rules: Sequence[SurfaceRule]
) -> Plan:
    mode = "final" if args.final else "fast" if args.fast else "plan"
    enabled = enabled_expansions(args)
    skipped: list[dict[str, str]] = []
    if scope.stale_reasons:
        return Plan(
            mode, scope, [], skipped, verdict=INCONCLUSIVE, enabled_expansions=enabled
        )
    if scope.source == "scope-reset":
        return Plan(
            mode,
            scope,
            [],
            skipped,
            verdict=VERIFIED_NO_PROOF,
            enabled_expansions=enabled,
        )
    if scope.source == "dirty-groups":
        return Plan(
            mode, scope, [], skipped, verdict=NEEDS_SCOPE, enabled_expansions=enabled
        )
    if not scope.active_files:
        return Plan(
            mode,
            scope,
            [],
            skipped,
            verdict=VERIFIED_NO_PROOF,
            enabled_expansions=enabled,
        )
    skipped.append(
        {"item": "workspace tests", "reason": "blocked unless --allow-workspace is set"}
    )
    if scope.adjacent_packages and not args.related_tests:
        skipped.append(
            {"item": "related tests", "reason": "blocked unless --related-tests is set"}
        )
    skipped.append(
        {"item": "just fmt", "reason": "mutating formatter is not verification"}
    )
    if scope.adjacent_packages:
        skipped.append(
            {
                "item": ", ".join(scope.adjacent_packages),
                "reason": "adjacent packages are compile-check only and require --related or --allow-workspace",
            }
        )
    commands = surface_commands(args, scope, rules)
    matched_rules = [rule for rule in rules if rule.id in scope.surface_rules]
    owner_packages = scope.owned_packages
    if any(rule.skip_owner_tests for rule in matched_rules):
        suppressed = sorted(
            {
                package
                for rule in matched_rules
                if rule.skip_owner_tests
                for package in rule.owned_packages
            }
        )
        owner_packages = [
            package for package in owner_packages if package not in suppressed
        ]
        if suppressed:
            skipped.append(
                {
                    "item": ", ".join(suppressed),
                    "reason": "surface rule owns focused validation",
                }
            )
    commands.extend(owner_commands(owner_packages, args, scope, rules))
    commands.extend(script_validation_commands(scope))
    if (args.related or args.allow_workspace) and scope.adjacent_packages:
        commands.extend(adjacent_commands(scope.adjacent_packages))
    if args.related_tests and scope.adjacent_packages:
        commands.extend(related_test_commands(scope.adjacent_packages))
    if mode == "final":
        commands = [*final_formatter_commands(scope), *commands]
    j = justfile_check_command(scope)
    if j is not None:
        commands.append(j)
    h = hygiene_command(scope)
    if h is not None:
        commands.append(h)
    return Plan(mode, scope, commands, skipped, enabled_expansions=enabled)


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
        Path(__file__).resolve(),
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
    rel_roots = [rel(root) for root in roots]
    output = git(
        ["ls-files", "--cached", "--others", "--exclude-standard", "--", *rel_roots],
        check=False,
    )
    return normalize_paths(line for line in output.splitlines() if line.strip())


def working_tree_hash(packages: Iterable[str], graph: CargoGraph) -> str:
    hasher = hashlib.sha256()
    for path in sorted(
        git_list_selected_files(selected_hash_roots(packages, graph)),
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
    for extra in proof_input_files():
        hasher.update(rel(extra).encode("utf-8") + b"\0")
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
    for extra in proof_input_files():
        hasher.update(rel(extra).encode("utf-8") + b"\0")
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
    files = set(git_list_selected_files(roots))
    files.update(active_files)
    return scoped_file_hash(sorted(files, key=lambda p: p.as_posix()))


def cache_key(
    command: CommandSpec, scope: Scope, graph: CargoGraph
) -> tuple[str, dict[str, Any]]:
    components: list[str] = []
    if command.owner_packages:
        components.append(working_tree_hash(command.owner_packages, graph))
    if command.hash_paths:
        components.append(surface_paths_hash(command.hash_paths, scope.active_files))
    if not components:
        components.append(scoped_file_hash(scope.active_files))
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
    atomic_write_json(LEDGER_PATH, data)


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
    log_path = log_path_for(command)
    args_list = list(command.args)
    if args_list and args_list[0] in ("python", "python3"):
        # Rules hardcode "python", which may not exist as an alias on
        # WSL/unix; the running interpreter always does.
        args_list[0] = sys.executable
    try:
        completed = run_capture(args_list, cwd=command.cwd, timeout=command.timeout)
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
            summarize_failure(completed.stdout + "\n" + completed.stderr),
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
    dirty = {path.as_posix() for path in dirty_files()}
    if active & dirty:
        return current_head()
    parent = git(["rev-parse", "--verify", "HEAD^"], check=False).strip()
    return parent or current_head()


def baseline_command_result(command: CommandSpec, scope: Scope) -> str:
    base = baseline_ref_for_scope(scope)
    temp_root = STATE_DIR / "baseline-worktrees"
    temp_root.mkdir(parents=True, exist_ok=True)
    worktree = temp_root / f"baseline-{os.getpid()}-{int(time.time())}"
    try:
        add = run_capture(
            ["git", "worktree", "add", "--detach", str(worktree), base], timeout=120
        )
        if add.returncode != 0:
            return "inconclusive"
        cwd = (
            worktree
            if command.cwd.resolve() == REPO_ROOT.resolve()
            else worktree / rel(command.cwd)
        )
        completed = run_capture(command.args, cwd=cwd, timeout=command.timeout)
        return "pre-existing" if completed.returncode != 0 else "new"
    except Exception:
        return "inconclusive"
    finally:
        run_capture(
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
        and reached_test_execution(result)
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
        "scope": scope_to_json(plan.scope),
        "verdict": verdict,
        "results": [result_to_json(result) for result in results],
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
    cache = load_cache()
    entries = cache.setdefault("entries", {})
    results: list[CommandResult] = []
    cache_miss_reasons: list[str] = []
    for command in plan.commands:
        key, payload = cache_key(command, plan.scope, graph)
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
        result = execute_command(command)
        if result.status == FAILED and should_retry_for_flake(result, args):
            retry = execute_command(command)
            if retry.status == VERIFIED:
                retry.flaky = True
                result = retry
        results.append(result)
        if result.status != VERIFIED:
            if args.baseline:
                result.baseline = baseline_command_result(command, plan.scope)
            append_ledger(
                ledger_entry(plan, results, result.status, cache_miss_reasons)
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
            atomic_write_json(CACHE_PATH, cache)
    append_ledger(ledger_entry(plan, results, VERIFIED, cache_miss_reasons))
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
        "scope": scope_to_json(plan.scope),
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
        "results": [result_to_json(result) for result in results],
        "cached": [result_to_json(result) for result in results if result.cached],
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


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Scope-locked local verification router"
    )
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--plan", action="store_true")
    modes.add_argument("--fast", action="store_true")
    modes.add_argument("--final", action="store_true")
    parser.add_argument("--changed", action="append", default=[])
    parser.add_argument("--staged", action="store_true")
    parser.add_argument("--all-dirty", action="store_true")
    parser.add_argument("--scope-start")
    parser.add_argument("--scope", choices=["current"])
    parser.add_argument("--scope-add", action="append", default=[])
    parser.add_argument("--scope-reset", action="store_true")
    parser.add_argument("--related", action="store_true")
    parser.add_argument("--related-tests", action="store_true")
    parser.add_argument("--allow-workspace", action="store_true")
    parser.add_argument("--isolated", action="store_true")
    parser.add_argument("--regen", action="store_true")
    parser.add_argument("--baseline", action="store_true")
    parser.add_argument("--retry-flakes", action="store_true")
    parser.add_argument("--no-cache", action="store_true")
    parser.add_argument("--cache-readonly", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    if not args.plan and not args.fast and not args.final:
        args.plan = True
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        graph = load_cargo_metadata()
        rules = load_rules()
        scope, scope_error = select_scope(args, graph, rules)
        if scope is None:
            if args.json:
                print(
                    json.dumps({"verdict": NEEDS_SCOPE, "error": scope_error}, indent=2)
                )
            else:
                print(f"{NEEDS_SCOPE}: {scope_error}")
            return EXIT_CODES[NEEDS_SCOPE]
        plan = plan_commands(args, scope, rules)
        if scope_error:
            plan.verdict = plan.verdict or (
                NEEDS_SCOPE if scope.source == "dirty-groups" else INCONCLUSIVE
            )
            plan.skipped.append({"item": "scope selection", "reason": scope_error})
        if args.plan:
            verdict = plan.verdict or "PLANNED"
            if args.json:
                print(
                    json.dumps(
                        plan_to_json(plan, verdict, [], []), indent=2, default=str
                    )
                )
            else:
                print_plan(plan, verdict=verdict)
            return 0 if verdict == "PLANNED" else EXIT_CODES.get(verdict, 0)
        verdict, results, cache_miss_reasons = execute_plan(plan, graph, args)
        if args.json:
            print(
                json.dumps(
                    plan_to_json(plan, verdict, results, cache_miss_reasons),
                    indent=2,
                    default=str,
                )
            )
        else:
            print_plan(
                plan,
                verdict=verdict,
                results=results,
                cache_miss_reasons=cache_miss_reasons,
            )
        return EXIT_CODES.get(verdict, 4)
    except subprocess.CalledProcessError as exc:
        message = exc.stderr or exc.output or str(exc)
        if args.json:
            print(json.dumps({"verdict": TOOLING_ERROR, "error": message}, indent=2))
        else:
            print(f"{TOOLING_ERROR}: {message}")
        return EXIT_CODES[TOOLING_ERROR]
    except Exception as exc:
        if args.json:
            print(json.dumps({"verdict": TOOLING_ERROR, "error": str(exc)}, indent=2))
        else:
            print(f"{TOOLING_ERROR}: {exc}")
        return EXIT_CODES[TOOLING_ERROR]


if __name__ == "__main__":
    raise SystemExit(main())
