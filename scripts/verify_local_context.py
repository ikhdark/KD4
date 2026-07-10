#!/usr/bin/env python3
"""Shared context, models, and repository inspection for verify_local."""

from __future__ import annotations

from dataclasses import dataclass
from dataclasses import field
import fnmatch
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import time
from types import ModuleType
from typing import Any
from typing import Iterable
from typing import Sequence
from urllib.parse import unquote
from urllib.parse import urlparse


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


_RUNTIME: ModuleType | None = None


def configure_runtime(runtime: ModuleType) -> None:
    global _RUNTIME
    _RUNTIME = runtime


def _runtime() -> ModuleType:
    if _RUNTIME is None:
        raise RuntimeError("verify_local context runtime is not configured")
    return _RUNTIME


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
        return _runtime().shell_join(self.args)


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
        _runtime().kill_process_tree(process)
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
    subcommand = _runtime().git_subcommand(args)
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
    completed = _runtime().run_capture(
        _runtime().git_command(args), timeout=timeout, check=False
    )
    if _runtime().git_failed_for_dubious_ownership(
        completed
    ) and _runtime().git_is_read_only_inspection(args):
        completed = _runtime().run_capture(
            _runtime().git_command(args, safe_directory=True),
            timeout=timeout,
            check=False,
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
    return _runtime().git_capture(args, timeout=timeout, check=check).stdout


def current_branch() -> str:
    return (
        _runtime().git(["branch", "--show-current"], check=False).strip()
        or _runtime().git(["rev-parse", "--short", "HEAD"], check=False).strip()
    )


def current_head() -> str:
    return _runtime().git(["rev-parse", "HEAD"], check=False).strip()


def is_ancestor(base: str, head: str = "HEAD") -> bool:
    return (
        bool(base)
        and _runtime()
        .git_capture(
            ["merge-base", "--is-ancestor", base, head], timeout=30, check=False
        )
        .returncode
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
        path = _runtime().normalize_path(value, strip_outer=strip_outer)
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
    output = _runtime().git(["-c", "core.quotepath=off", *args, "-z"], check=False)
    delimiter = "\0" if "\0" in output else "\n"
    return _runtime().normalize_paths(
        (path for path in output.split(delimiter) if path), strip_outer=False
    )


def staged_files() -> list[Path]:
    # Include deletions (D): a pure `git rm` is a change that needs proof;
    # excluding it yielded an empty scope and a vacuous VERIFIED.
    return _runtime().git_name_list(
        ["diff", "--cached", "--name-only", "--diff-filter=ACMRTD"]
    )


def unstaged_files() -> list[Path]:
    return _runtime().git_name_list(["diff", "--name-only", "--diff-filter=ACMRTD"])


def untracked_files() -> list[Path]:
    return _runtime().git_name_list(["ls-files", "--others", "--exclude-standard"])


def dirty_files() -> list[Path]:
    return _runtime().stable_unique(
        [
            *_runtime().staged_files(),
            *_runtime().unstaged_files(),
            *_runtime().untracked_files(),
        ]
    )


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
    completed = _runtime().run_capture(
        ["cargo", "metadata", "--format-version", "1"],
        cwd=CODEX_RS,
        timeout=120,
        check=True,
    )
    metadata = _runtime().parse_last_json_value(completed.stdout)
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
            _runtime().path_matches_rule_pattern(path, pattern)
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
    for rule in _runtime().matching_rules(paths, rules):
        packages.update(rule.owned_packages)
    for path in paths:
        package = _runtime().package_for_path(path, graph)
        if package is not None:
            packages.add(package.name)
    return sorted(packages)


def is_ignored_build_output(path: Path) -> bool:
    return any(part in IGNORED_DIR_PARTS for part in path.parts)


def classify_dirty_group(
    path: Path, graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> str:
    for rule in _runtime().matching_rules([path], rules):
        return f"contract:{rule.id}"
    if path.as_posix() in {
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "justfile",
    }:
        return f"contract:{path.as_posix()}"
    package = _runtime().package_for_path(path, graph)
    if package is not None:
        return f"package:{package.name}"
    return f"area:{path.parts[0]}" if path.parts else "root"


def group_dirty_files(
    paths: Iterable[Path], graph: CargoGraph, rules: Sequence[SurfaceRule]
) -> dict[str, list[Path]]:
    groups: dict[str, list[Path]] = {}
    for path in paths:
        if _runtime().is_ignored_build_output(path):
            continue
        groups.setdefault(
            _runtime().classify_dirty_group(path, graph, rules), []
        ).append(path)
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
