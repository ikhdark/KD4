#!/usr/bin/env python3
"""Strict manifest-driven runner for repository-owned Rust test targets."""

from __future__ import annotations

import argparse
import codecs
import filecmp
import fnmatch
import hashlib
import json
import math
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from contextlib import ExitStack, nullcontext
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

import tomllib

# Isolated Python omits the script directory from sys.path. Resolve siblings
# from this file, never the caller's cwd or PYTHONPATH.
if not __package__:
    sys.path.insert(0, str(Path(__file__).resolve().parent))

try:
    from .process_owner import owned_process, CleanupFailed, OwnedThreadPoolExecutor, run_owned
    from .rust_tool_env import cargo_package_specs, local_rust_env
    from .validation_metrics import ValidationMetrics
except ImportError:
    from process_owner import owned_process, CleanupFailed, OwnedThreadPoolExecutor, run_owned
    from rust_tool_env import cargo_package_specs, local_rust_env
    from validation_metrics import ValidationMetrics

REPO_ROOT = Path(__file__).resolve().parents[1]
CODEX_RS_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_MANIFEST = CODEX_RS_ROOT / ".config" / "kd4-rust-tests.toml"
SCHEMA_VERSION = 1

# Windows test binaries link an 8 MiB main stack and libtest builds its per-test
# worker threads from RUST_MIN_STACK. Keep the runner aligned with the justfile
# and `scripts/rust_build_status.py`.
RUST_MIN_STACK_BYTES = "8388608"
MAX_FAILURE_STREAM_CHARS = 4096

# Windows sandbox helpers that sandbox code locates by file name rather than
# through `CARGO_BIN_EXE_*`.
WINDOWS_RESOURCE_HELPERS = ("codex-windows-sandbox-setup", "codex-command-runner")


class ExecutionReceipts(dict):
    """Compatibility projection plus the execution identities behind it."""

    def __init__(self, projection, executed, required=None, *, gates=None):
        super().__init__(projection)
        self.executed = set(executed)
        self.required = None if required is None else set(required)
        self.gates = gates

    @staticmethod
    def identities(values):
        return [{"binary": binary, "helpers": sorted(helpers), "test": test}
                for binary, helpers, test in sorted(values, key=lambda row: (row[0], sorted(row[1]), row[2]))]

    def completed_tests(self):
        binaries = sorted({binary for binary, _, _ in self.executed})
        return {binary: sorted({test for owner, _, test in self.executed if owner == binary})
                for binary in binaries}


class RunnerError(RuntimeError):
    """Raised when a declared test contract cannot be honored."""

    def __init__(
        self,
        message: str,
        *,
        outcome: str = "failed",
        result: subprocess.CompletedProcess[str] | None = None,
        completed_gates: dict[str, list[str]] | None = None,
        admission_status: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(message)
        self.outcome = outcome
        self.result = result
        self.completed_gates = completed_gates if completed_gates is not None else {}
        self.completed_tests: dict[str, list[str]] = {}
        self.admission_status = admission_status


@dataclass(frozen=True)
class Helper:
    name: str
    package: str
    binary: str
    platform: str | None


@dataclass(frozen=True)
class Target:
    name: str
    package: str
    selector_kind: str
    selector_value: str | None
    helpers: tuple[str, ...]
    helpers_by_test_prefix: Mapping[str, tuple[str, ...]] = field(default_factory=dict)

    @property
    def all_helpers(self) -> tuple[str, ...]:
        return tuple(
            dict.fromkeys(
                [
                    *self.helpers,
                    *(
                        name
                        for names in self.helpers_by_test_prefix.values()
                        for name in names
                    ),
                ]
            )
        )

    def selection_args(self) -> list[str]:
        args = ["-p", self.package]
        if self.selector_kind == "lib":
            args.append("--lib")
        else:
            args.extend([f"--{self.selector_kind}", self.selector_value or ""])
        return args


@dataclass(frozen=True)
class GateStep:
    target: str
    filterset: str | None
    tests: tuple[str, ...]
    helpers: tuple[str, ...] | None = None


@dataclass(frozen=True)
class Gate:
    name: str
    description: str
    steps: tuple[GateStep, ...]


@dataclass(frozen=True)
class Manifest:
    version: int
    helpers: Mapping[str, Helper]
    targets: Mapping[str, Target]
    gates: Mapping[str, Gate]

    @classmethod
    def load(cls, path: Path) -> Manifest:
        try:
            with path.open("rb") as manifest_file:
                raw = tomllib.load(manifest_file)
        except (OSError, tomllib.TOMLDecodeError) as exc:
            raise RunnerError(f"cannot read Rust test manifest {path}: {exc}") from exc
        return cls.from_data(raw)

    @classmethod
    def from_data(cls, raw: Any) -> Manifest:
        root = _require_table(raw, "manifest")
        _reject_unknown(root, {"version", "helpers", "targets", "gates"}, "manifest")

        version = _require_int(root.get("version"), "manifest.version")
        if version != SCHEMA_VERSION:
            raise RunnerError(
                f"manifest.version must be {SCHEMA_VERSION}, found {version}"
            )

        helpers_raw = _require_table(root.get("helpers"), "manifest.helpers")
        targets_raw = _require_table(root.get("targets"), "manifest.targets")
        gates_raw = _require_table(root.get("gates"), "manifest.gates")

        helpers: dict[str, Helper] = {}
        for name, value in helpers_raw.items():
            helper_name = _require_name(name, "helper")
            table = _require_table(value, f"helpers.{helper_name}")
            _reject_unknown(
                table,
                {"package", "bin", "platform"},
                f"helpers.{helper_name}",
            )
            package = _require_string(
                table.get("package"), f"helpers.{helper_name}.package"
            )
            binary = _require_string(table.get("bin"), f"helpers.{helper_name}.bin")
            platform_value = table.get("platform")
            platform = None
            if platform_value is not None:
                platform = _require_string(
                    platform_value, f"helpers.{helper_name}.platform"
                )
                if platform not in {"windows", "linux", "macos"}:
                    raise RunnerError(
                        f"helpers.{helper_name}.platform must be windows, linux, or macos"
                    )
            helpers[helper_name] = Helper(helper_name, package, binary, platform)

        targets: dict[str, Target] = {}
        for name, value in targets_raw.items():
            target_name = _require_name(name, "target")
            table = _require_table(value, f"targets.{target_name}")
            _reject_unknown(
                table,
                {"package", "lib", "test", "bin", "helpers", "helpers_by_test_prefix"},
                f"targets.{target_name}",
            )
            package = _require_string(
                table.get("package"), f"targets.{target_name}.package"
            )
            has_lib = "lib" in table
            if sum(key in table for key in ("lib", "test", "bin")) != 1:
                raise RunnerError(
                    f"targets.{target_name} must declare exactly one of lib, test or bin"
                )
            if has_lib:
                if table["lib"] is not True:
                    raise RunnerError(f"targets.{target_name}.lib must be true")
                selector_kind = "lib"
                selector_value = None
            else:
                selector_kind = "test" if "test" in table else "bin"
                selector_value = _require_string(
                    table[selector_kind], f"targets.{target_name}.{selector_kind}"
                )
            helper_names = _require_string_list(
                table.get("helpers"), f"targets.{target_name}.helpers"
            )
            _reject_duplicates(helper_names, f"targets.{target_name}.helpers")
            for helper_name in helper_names:
                if helper_name not in helpers:
                    raise RunnerError(
                        f"targets.{target_name}.helpers references unknown helper {helper_name!r}"
                    )
            prefix_helpers = {}
            for prefix, names in _require_table(
                table.get("helpers_by_test_prefix", {}),
                f"targets.{target_name}.helpers_by_test_prefix",
            ).items():
                if not re.fullmatch(r"[A-Za-z0-9_]+(?:::[A-Za-z0-9_]+)*::", prefix):
                    raise RunnerError(f"invalid test module prefix {prefix!r}")
                names = _require_string_list(names, f"helper prefix {prefix}")
                _reject_duplicates(names, f"helper prefix {prefix}")
                if not set(names).issubset(helpers):
                    raise RunnerError(
                        f"helper prefix {prefix} references unknown helper"
                    )
                if any(
                    prefix.startswith(other) or other.startswith(prefix)
                    for other in prefix_helpers
                ):
                    raise RunnerError(f"overlapping helper prefix {prefix!r}")
                prefix_helpers[prefix] = tuple(names)
            targets[target_name] = Target(
                target_name,
                package,
                selector_kind,
                selector_value,
                tuple(helper_names),
                prefix_helpers,
            )

        gates: dict[str, Gate] = {}
        for name, value in gates_raw.items():
            gate_name = _require_name(name, "gate")
            table = _require_table(value, f"gates.{gate_name}")
            _reject_unknown(table, {"description", "steps"}, f"gates.{gate_name}")
            description = _require_string(
                table.get("description", gate_name), f"gates.{gate_name}.description"
            )
            steps_raw = table.get("steps")
            if not isinstance(steps_raw, list) or not steps_raw:
                raise RunnerError(f"gates.{gate_name}.steps must be a non-empty array")
            steps: list[GateStep] = []
            for index, value in enumerate(steps_raw):
                prefix = f"gates.{gate_name}.steps[{index}]"
                step = _require_table(value, prefix)
                _reject_unknown(step, {"target", "filter", "tests", "helpers"}, prefix)
                target_name = _require_string(step.get("target"), f"{prefix}.target")
                if target_name not in targets:
                    raise RunnerError(
                        f"{prefix}.target references unknown target {target_name!r}"
                    )
                filter_value = step.get("filter")
                filterset = None
                if filter_value is not None:
                    filterset = _require_string(filter_value, f"{prefix}.filter")
                tests = _require_string_list(step.get("tests", []), f"{prefix}.tests")
                if not tests and (filterset is None or "tests" in step):
                    raise RunnerError(
                        f"{prefix}.tests must not be empty; omit it for a filter-only step"
                    )
                _reject_duplicates(tests, f"{prefix}.tests")
                step_helpers = None
                if "helpers" in step:
                    names = _require_string_list(step["helpers"], f"{prefix}.helpers")
                    _reject_duplicates(names, f"{prefix}.helpers")
                    if not set(names).issubset(targets[target_name].all_helpers):
                        raise RunnerError(
                            f"{prefix}.helpers must be a subset of target helpers"
                        )
                    step_helpers = tuple(names)
                steps.append(
                    GateStep(target_name, filterset, tuple(tests), step_helpers)
                )
            gates[gate_name] = Gate(gate_name, description, tuple(steps))

        return cls(version, helpers, targets, gates)

    def target(self, name: str) -> Target:
        try:
            return self.targets[name]
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test target {name!r}") from exc

    def gate(self, name: str) -> Gate:
        try:
            return self.gates[name]
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test gate {name!r}") from exc


def _require_table(value: Any, location: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise RunnerError(f"{location} must be a table")
    return value


def _require_string(value: Any, location: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise RunnerError(f"{location} must be a non-empty string")
    return value


def _require_int(value: Any, location: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise RunnerError(f"{location} must be an integer")
    return value


def _require_name(value: Any, kind: str) -> str:
    return _require_string(value, f"{kind} name")


def _require_string_list(value: Any, location: str) -> list[str]:
    if not isinstance(value, list):
        raise RunnerError(f"{location} must be an array of strings")
    return [
        _require_string(item, f"{location}[{index}]")
        for index, item in enumerate(value)
    ]


def _reject_unknown(table: Mapping[str, Any], allowed: set[str], location: str) -> None:
    unknown = sorted(set(table) - allowed)
    if unknown:
        raise RunnerError(f"{location} contains unknown keys: {', '.join(unknown)}")


def _reject_duplicates(values: Sequence[str], location: str) -> None:
    seen: set[str] = set()
    duplicates: list[str] = []
    for value in values:
        if value in seen and value not in duplicates:
            duplicates.append(value)
        seen.add(value)
    if duplicates:
        raise RunnerError(f"{location} contains duplicates: {', '.join(duplicates)}")


@dataclass(frozen=True)
class MetadataIndex:
    target_directory: Path
    packages: Mapping[str, Mapping[str, Any]]

    @classmethod
    def from_json(cls, raw: Any) -> MetadataIndex:
        root = _require_table(raw, "cargo metadata")
        target_directory = _require_string(
            root.get("target_directory"), "cargo metadata.target_directory"
        )
        packages_raw = root.get("packages")
        if not isinstance(packages_raw, list):
            raise RunnerError("cargo metadata.packages must be an array")
        packages: dict[str, Mapping[str, Any]] = {}
        for index, value in enumerate(packages_raw):
            package = _require_table(value, f"cargo metadata.packages[{index}]")
            name = _require_string(
                package.get("name"), f"cargo metadata.packages[{index}].name"
            )
            if name in packages:
                raise RunnerError(
                    f"cargo metadata contains duplicate package name {name!r}"
                )
            packages[name] = package
        return cls(Path(target_directory), packages)

    def validate_manifest(self, manifest: Manifest) -> None:
        for helper in manifest.helpers.values():
            self.validate_helper(helper)
        for target in manifest.targets.values():
            self.validate_target(target)

    def validate_helper(self, helper: Helper) -> None:
        package = self._package(helper.package, f"helper {helper.name!r}")
        if not self._has_target(package, helper.binary, "bin"):
            raise RunnerError(
                f"helper {helper.name!r} declares missing binary "
                f"{helper.package}/{helper.binary}"
            )

    def validate_target(self, target: Target) -> None:
        package = self._package(target.package, f"target {target.name!r}")
        if target.selector_kind == "lib":
            if not self._has_kind(package, "lib"):
                raise RunnerError(
                    f"target {target.name!r} declares --lib for package "
                    f"{target.package!r}, which has no library target"
                )
        elif not self._has_target(
            package, target.selector_value or "", target.selector_kind
        ):
            raise RunnerError(
                f"target {target.name!r} declares missing test target "
                f"{target.package}/{target.selector_value}"
            )

    def package_id(self, package_name: str) -> str:
        package = self._package(package_name, f"package {package_name!r}")
        return _require_string(package.get("id"), f"cargo package {package_name!r}.id")

    def _package(self, name: str, owner: str) -> Mapping[str, Any]:
        try:
            return self.packages[name]
        except KeyError as exc:
            raise RunnerError(
                f"{owner} declares unknown Cargo package {name!r}"
            ) from exc

    @staticmethod
    def _targets(package: Mapping[str, Any]) -> list[Mapping[str, Any]]:
        targets = package.get("targets")
        if not isinstance(targets, list):
            raise RunnerError("cargo metadata package targets must be an array")
        return [_require_table(target, "cargo metadata target") for target in targets]

    @classmethod
    def _has_target(cls, package: Mapping[str, Any], name: str, kind: str) -> bool:
        return any(
            target.get("name") == name
            and isinstance(target.get("kind"), list)
            and kind in target["kind"]
            for target in cls._targets(package)
        )

    @classmethod
    def _has_kind(cls, package: Mapping[str, Any], kind: str) -> bool:
        return any(
            isinstance(target.get("kind"), list) and kind in target["kind"]
            for target in cls._targets(package)
        )


_PATH_MODULE = re.compile(
    r'#\[path\s*=\s*"([^"]+)"\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;'
)
_LIB_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}


def _rust_module_owners(
    metadata: MetadataIndex, raw: str, cwd: Path,
    declarations: dict | None = None,
) -> tuple[str, list[tuple[str, str | None, str]], set[tuple[str, str | None]]] | None:
    """Return legacy module candidates plus independently registered selectors."""
    if not raw.endswith(".rs"):
        return None
    candidates = [Path(raw)] if Path(raw).is_absolute() else [cwd / raw, REPO_ROOT / raw]
    for path in (candidate.resolve() for candidate in candidates):
        owner = None
        for name, package in metadata.packages.items():
            manifest_path = package.get("manifest_path")
            if not isinstance(manifest_path, str):
                continue
            root = Path(manifest_path).resolve().parent
            if path.is_relative_to(root) and (
                owner is None or len(root.parts) > len(owner[1].parts)
            ):
                owner = (name, root, package)
        if owner is None:
            continue
        targets = []
        for target in MetadataIndex._targets(owner[2]):
            kinds = target.get("kind")
            src_path = target.get("src_path")
            if not isinstance(kinds, list) or not isinstance(src_path, str):
                continue
            if "test" in kinds or "bin" in kinds:
                selector = ("test" if "test" in kinds else "bin", target.get("name"))
            elif _LIB_KINDS.intersection(kinds):
                selector = ("lib", None)
            else:
                continue
            targets.append((selector, Path(src_path).resolve()))
        # A file that is a target root belongs only to that target.
        roots = [(selector, src) for selector, src in targets if src == path]
        modules = []
        registered = set()
        for (kind, value), src in roots or [
            (selector, src) for selector, src in targets if path.is_relative_to(src.parent)
        ]:
            module = _rust_module(src, path, declarations=declarations)
            modules.append((kind, value, module))
            # Layout alone does not establish membership in sibling shards.
            # Preserve gate matching, but verify target advice against actual
            # declarations, excluding comments and literals.
            try:
                resolved = _rust_module_source(src, module, owner[1])
            except (OSError, UnicodeError, ValueError):
                resolved = None
            if resolved is not None and resolved[0] == path:
                registered.add((kind, value))
        return (owner[0], modules, registered) if modules else None
    return None


def _rust_module(
    crate_root: Path, path: Path, depth: int = 0, *, declarations: dict | None = None
) -> str:
    """Module path of `path` below `crate_root`, honoring sibling `#[path]`."""
    if path == crate_root:
        return ""
    if declarations is None:
        declarations = {}
    if depth < 8:
        if path.parent not in declarations:
            siblings = []
            for sibling in sorted(path.parent.glob("*.rs")):
                try:
                    text = sibling.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                siblings.append((sibling, _PATH_MODULE.findall(text)))
            declarations[path.parent] = siblings
        for sibling, modules in declarations[path.parent]:
            if sibling == path:
                continue
            for declared, module in modules:
                if declared == path.name:
                    parent = _rust_module(
                        crate_root, sibling.resolve(), depth + 1, declarations=declarations
                    )
                    return f"{parent}::{module}" if parent else module
    parts = list(path.relative_to(crate_root.parent).with_suffix("").parts)
    if parts[-1:] == ["mod"]:
        parts.pop()
    return "::".join(parts)


def _rust_scope_items(
    text: str,
) -> dict[tuple[str, str], list[tuple[str | None, str | None]]]:
    """Read named module/function declarations at this scope, excluding literal bodies."""
    token = re.compile(
        r'//[^\n]*|/\*|r(?P<hashes>\#*)".*?"(?P=hashes)|'
        r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'|[A-Za-z_][A-Za-z_0-9]*|[^\s]',
        re.DOTALL,
    )
    tokens = []
    cursor = 0
    while match := token.search(text, cursor):
        value = match.group()
        cursor = match.end()
        if value.startswith("//"):
            continue
        if value == "/*":
            depth = 1
            while depth:
                nested = re.search(r"/\*|\*/", text[cursor:])
                if nested is None:
                    raise ValueError("unclosed Rust comment in verification source")
                cursor += nested.end()
                depth += 1 if nested.group() == "/*" else -1
            continue
        tokens.append((value, match.start(), cursor))
    items: dict[tuple[str, str], list[tuple[str | None, str | None]]] = {}
    index = 0
    path = None
    while index < len(tokens):
        value = tokens[index][0]
        if (
            value == "#"
            and index + 5 < len(tokens)
            and [item[0] for item in tokens[index : index + 4]]
            == ["#", "[", "path", "="]
            and tokens[index + 5][0] == "]"
        ):
            literal = tokens[index + 4][0]
            if not literal.startswith('"'):
                raise ValueError(
                    "unsupported Rust path attribute in verification route"
                )
            path = json.loads(literal)
            index += 6
            continue
        if value in {"mod", "fn"} and index + 2 < len(tokens):
            name = tokens[index + 1][0]
            after_name = index + 2
            if value == "fn":
                items.setdefault((value, name), []).append((None, None))
            elif tokens[after_name][0] == ";":
                items.setdefault((value, name), []).append((path, None))
                path = None
                index += 3
                continue
            elif tokens[after_name][0] == "{":
                depth = 1
                end = after_name + 1
                while end < len(tokens) and depth:
                    depth += (tokens[end][0] == "{") - (tokens[end][0] == "}")
                    end += 1
                if depth:
                    raise ValueError("unclosed Rust module in verification source")
                body = text[tokens[after_name][2] : tokens[end - 1][1]]
                items.setdefault((value, name), []).append((path, body))
                path = None
                index = end
                continue
        if value in {"{", "[", "("}:
            closing = {"{": "}", "[": "]", "(": ")"}[value]
            depth = 1
            index += 1
            while index < len(tokens) and depth:
                depth += (tokens[index][0] == value) - (tokens[index][0] == closing)
                index += 1
            if value == "{":
                path = None
            continue
        if value == ";":
            path = None
        index += 1
    return items


def _rust_module_source(
    binary_root: Path, module: str, repo_root: Path
) -> tuple[Path, str | None] | None:
    """Resolve declared modules, leaving a terminal file's text unread."""
    source = binary_root.resolve()
    if not source.is_file():
        return None
    text = source.read_text(encoding="utf-8")
    module_dir = attribute_dir = source.parent
    components = module.split("::") if module else []
    for index, component in enumerate(components):
        declarations = _rust_scope_items(text).get(("mod", component), [])
        if len(declarations) != 1:
            return None
        path, body = declarations[0]
        if body is not None:
            module_dir = attribute_dir / path if path else module_dir / component
            attribute_dir = module_dir
            text = body
            continue
        if path:
            candidates = [attribute_dir / path]
        else:
            candidates = [
                module_dir / f"{component}.rs",
                module_dir / component / "mod.rs",
            ]
        candidates = [
            candidate.resolve() for candidate in candidates if candidate.is_file()
        ]
        if len(candidates) != 1 or not candidates[0].is_relative_to(
            repo_root.resolve()
        ):
            return None
        source = candidates[0]
        if index == len(components) - 1:
            return source, None
        text = source.read_text(encoding="utf-8")
        attribute_dir = source.parent
        module_dir = (
            source.parent if source.name == "mod.rs" else source.with_suffix("")
        )
    return source, text


def _rust_test_source(binary_root: Path, identity: str, repo_root: Path) -> Path | None:
    """Resolve the exact selected module chain and its declared test function."""
    components = identity.split("::")
    resolved = _rust_module_source(binary_root, "::".join(components[:-1]), repo_root)
    if resolved is None:
        return None
    source, text = resolved
    if text is None:
        text = source.read_text(encoding="utf-8")
    return (
        source
        if len(_rust_scope_items(text).get(("fn", components[-1]), [])) == 1
        else None
    )


Executor = Callable[..., subprocess.CompletedProcess[str]]

# Cargo and nextest put machine-readable output on stdout and build progress,
# rendered diagnostics, and test status on stderr. Keep streams the runner does
# not parse visible while retaining their full contents for failure recovery.
# Capture policy controls live display, not retention: every child stream is
# retained so failures and interruption do not require another test execution.
CAPTURE_NONE = "none"
CAPTURE_STDOUT = "stdout"
CAPTURE_BOTH = "both"


def _output_lines(
    result: subprocess.CompletedProcess[str], stream: str
) -> Iterable[str]:
    path = getattr(result, f"{stream}_path", None)
    if path is not None:
        with path.open(encoding="utf-8", errors="replace") as output:
            while line := output.readline(1_048_576):
                if len(line) == 1_048_576 and not line.endswith("\n"):
                    # An oversized diagnostic is in the artifact; it cannot be
                    # a trustworthy test status or a small helper-artifact record.
                    while line and not line.endswith("\n"):
                        line = output.readline(1_048_576)
                    continue
                yield line
    else:
        yield from (getattr(result, stream) or "").splitlines()


def _stdout_text(result: subprocess.CompletedProcess[str]) -> str:
    # JSON inventory/metadata is control data, not an unbounded diagnostic log.
    path = getattr(result, "stdout_path", None)
    return path.read_text(encoding="utf-8", errors="replace") if path else result.stdout


def _failure_diagnostic_excerpt(
    result: subprocess.CompletedProcess[str], stream: str, tail: str,
) -> str:
    """Display selected diagnostics, never replace retained bytes or test proof.

    A nextest failure may precede thousands of passes; a linker command may be
    one enormous line. Tail-only receipts make the caller read the log again to
    learn the cause. Keep a bounded, explicitly partial excerpt plus the tail.
    The existing line iterator bounds memory and skips oversized records.
    """
    path = getattr(result, f"{stream}_path", None)
    if path is None:
        return tail
    heading = "Selected failure diagnostics (partial; full log below):\n"
    separator = "\n... remaining log omitted; tail follows ...\n"
    tail = tail[-MAX_FAILURE_STREAM_CHARS:]
    tail_size = min(len(tail), MAX_FAILURE_STREAM_CHARS // 4)
    budget = MAX_FAILURE_STREAM_CHARS - len(heading) - len(separator) - tail_size
    selected: list[str] = []
    context = 0
    try:
        # Complete short logs should not be summarized or scanned again.
        if path.stat().st_size <= len(tail.encode("utf-8")):
            return tail
        for raw in _output_lines(result, stream):
            line = re.sub(r"\x1b\[[0-9;]*m", "", raw).rstrip("\r\n")
            diagnostic = bool(re.search(
                r"^\s*(?:error(?:\[E\d+\])?:|(?:lld-link|rust-lld): error:|"
                r"(?:=\s*)?note: (?:lld-link|rust-lld): error:)|"
                r"\bpanicked at\b", line,
            ))
            status = bool(re.match(
                r"\s*(?:FAIL|LEAK-FAIL|TIMEOUT|EXECFAIL|ABORT)\s+\[", line,
            ))
            if not (diagnostic or status or context):
                continue
            if diagnostic:
                context = 4
            elif context:
                context -= 1
            # Bound long assertion values separately so they cannot displace
            # every later failure. This is an excerpt, not exact source data.
            if len(line) > 512:
                line = line[:384] + " ... [line shortened] ... " + line[-96:]
            line += "\n"
            if len(line) > budget:
                break
            selected.append(line)
            budget -= len(line)
    except OSError:
        # Diagnostic selection must not mask the command's original failure.
        return tail
    if not selected:
        return tail
    return heading + "".join(selected) + separator + (tail[-tail_size:] if tail_size else "")


def _nextest_results(
    result: subprocess.CompletedProcess[str],
) -> Iterable[tuple[str, str, str]]:
    for stream in ("stdout", "stderr"):
        for line in _output_lines(result, stream):
            # Nextest's final/immediate-final output repeats status headers
            # after its summary. Those are presentation copies, not another
            # execution. Keep every pre-summary result, including real
            # duplicate executions and failures, for exactly-once validation.
            if re.match(r"\s*Summary\s+\[[^]\r\n]+\]\s+\d+ tests? run:", line):
                break
            match = re.fullmatch(
                r"\s*(PASS|LEAK|FAIL|LEAK-FAIL|TIMEOUT|EXECFAIL)\s+\[[^]\r\n]+\]\s+(?:\(\d+/\d+\)\s+)?(\S+)\s+(\S+)\s*",
                line,
            )
            if match:
                yield match.groups()


def _nextest_selected_count(result: subprocess.CompletedProcess[str]) -> int | None:
    for stream in ("stdout", "stderr"):
        for line in _output_lines(result, stream):
            match = re.search(r"\bStarting\s+(\d+) tests? across\b", line)
            if match:
                return int(match[1])
    return None


def _stop_process_tree(process: subprocess.Popen) -> None:
    if getattr(process, "_codex_owned_job", None) is not None:
        process._codex_owned_job.stop(time.monotonic() + 15)
    elif os.name == "nt":
        subprocess.run(
            ["taskkill", "/PID", str(process.pid), "/T", "/F"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=15,
            check=True,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait(timeout=15)


def _stream_retained_log(
    path: Path, destination: Any, stopped: threading.Event
) -> None:
    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    with path.open("rb") as output:
        while True:
            chunk = output.read(8192)
            if chunk:
                text = decoder.decode(chunk)
            elif stopped.is_set():
                text = decoder.decode(b"", final=True)
            else:
                stopped.wait(0.05)
                continue
            try:
                destination.write(text)
                destination.flush()
            except (OSError, ValueError):
                # A closed terminal must not prevent durable result retention.
                return
            if not chunk:
                return


def _default_executor(
    args: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    capture: str,
    timings: dict[str, Any] | None = None,
) -> subprocess.CompletedProcess[str]:
    timeout_value = env.get("CODEX_RUST_TEST_TIMEOUT_SECS")
    try:
        timeout = float(timeout_value) if timeout_value is not None else None
        if timeout is not None and (not math.isfinite(timeout) or timeout <= 0):
            raise ValueError
    except ValueError as exc:
        raise RunnerError(
            "command timeout must be a finite positive number of seconds"
        ) from exc
    paths: dict[str, Path] = {}
    cleanup_started = None

    def record_cleanup() -> None:
        if timings is not None and cleanup_started is not None:
            timings["cleanup_seconds"] = time.monotonic() - cleanup_started

    with ExitStack() as timing_cleanup, ExitStack() as stack:
        timing_cleanup.callback(record_cleanup)
        streams = {}
        followers = []
        stopped = threading.Event()
        for name, enabled in (
            ("stdout", capture != CAPTURE_NONE),
            ("stderr", capture == CAPTURE_BOTH),
        ):
            log_dir = Path(env.get("CODEX_RUST_TEST_LOG_DIR", tempfile.gettempdir()))
            log_dir.mkdir(parents=True, exist_ok=True)
            log = stack.enter_context(
                tempfile.NamedTemporaryFile(
                    prefix=f"rust-test-{name}-",
                    suffix=".log",
                    dir=log_dir,
                    delete=False,
                )
            )
            paths[name] = Path(log.name)
            streams[name] = log
            if not enabled:
                follower = threading.Thread(
                    target=_stream_retained_log,
                    args=(paths[name], getattr(sys, name), stopped),
                    daemon=True,
                )
                follower.start()
                followers.append(follower)

        def finish_streams() -> None:
            stopped.set()
            for follower in followers:
                follower.join()

        # The process owner exits first, so final output is drained before logs close.
        stack.callback(finish_streams)
        process = stack.enter_context(
            owned_process(
                list(args),
                cwd=cwd,
                env=dict(env),
                **streams,
                start_new_session=os.name != "nt",
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP
                if os.name == "nt"
                else 0,
            )
        )
        wait_started = time.monotonic()
        try:
            returncode = process.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
            cleanup_started = time.monotonic()
            if timings is not None:
                timings["process_wait_seconds"] = cleanup_started - wait_started
            try:
                _stop_process_tree(process)
            except (
                OSError,
                subprocess.SubprocessError,
                KeyboardInterrupt,
                CleanupFailed,
            ) as cleanup_error:
                try:
                    process.kill()
                    process.wait(timeout=5)
                except (OSError, subprocess.SubprocessError):
                    pass
                raise RunnerError(
                    f"command cleanup_failed; process tree {process.pid} is unconfirmed; "
                    + "; ".join(f"Full {name}: {path}" for name, path in paths.items()),
                    outcome="cleanup_failed",
                ) from cleanup_error
            outcome = "cancelled" if isinstance(exc, KeyboardInterrupt) else "timed_out"
            logs = "\n".join(f"Full {name}: {path}" for name, path in paths.items())
            raise RunnerError(
                f"command {outcome}: {subprocess.list2cmdline(list(args))}\n{logs}",
                outcome=outcome,
            ) from exc
        finally:
            if cleanup_started is None:
                cleanup_started = time.monotonic()
                if timings is not None:
                    timings["process_wait_seconds"] = cleanup_started - wait_started
    tails = {}
    for name, path in paths.items():
        with path.open("rb") as output:
            output.seek(max(0, path.stat().st_size - MAX_FAILURE_STREAM_CHARS * 4))
            tails[name] = (
                output.read()
                .decode("utf-8", errors="replace")
                .replace("\r\n", "\n")[-MAX_FAILURE_STREAM_CHARS:]
            )
    result = subprocess.CompletedProcess(
        list(args), returncode, tails.get("stdout"), tails.get("stderr")
    )
    for name, path in paths.items():
        setattr(result, f"{name}_path", path)
    return result


def current_platform() -> str:
    if os.name == "nt":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def _nextest_binary_id(target: Target) -> str:
    """The binary ID nextest reports for the target's test binary."""
    if target.selector_kind == "lib":
        return target.package
    suffix = (
        f"bin/{target.selector_value}"
        if target.selector_kind == "bin"
        else target.selector_value
    )
    return f"{target.package}::{suffix}"


def _exact_test_ids(args: Sequence[str]) -> list[str] | None:
    """IDs an exact `-E 'test(=ID) | ...'` selection can reach, else None.

    Nextest unions filtersets, and the remaining filtering options only narrow
    that union. Name filters and libtest arguments need discovery instead.
    """
    ids: list[str] = []
    index = 0
    while index < len(args):
        token = args[index]
        if token in {"-E", "--filterset"}:
            expression = args[index + 1]
            index += 2
        elif token.startswith("--filterset="):
            expression = token.split("=", 1)[1]
            index += 1
        elif token == "--run-ignored":
            index += 2
            continue
        elif token.startswith("-") and token != "--":
            index += 1
            continue
        else:
            return None
        for term in expression.split("|"):
            match = re.fullmatch(r"\s*test\(=([^()\s]+)\)\s*", term)
            if match is None:
                return None
            ids.append(match[1])
    return ids or None


def _required_exact_test_ids(args: Sequence[str]) -> list[str] | None:
    """Reconcile explicit IDs after intentional libtest name/skip narrowing."""
    head, tail = (list(args), [])
    if "--" in head:
        separator = head.index("--")
        head, tail = head[:separator], head[separator + 1:]
    selected = _exact_test_ids(head)
    if selected is None:
        return None
    names, skips = [], []
    index = 0
    while index < len(tail):
        token = tail[index]
        if token == "--skip":
            skips.append(tail[index + 1])
            index += 2
            continue
        if token.startswith("--skip="):
            skips.append(token.split("=", 1)[1])
        elif not token.startswith("-"):
            names.append(token)
        index += 1
    matches = (lambda name, pattern: name == pattern) if "--exact" in tail else (lambda name, pattern: pattern in name)
    return sorted({name for name in selected
                   if (not names or any(matches(name, pattern) for pattern in names))
                   and not any(matches(name, pattern) for pattern in skips)})


def _validate_cargo_profile(cargo_profile: str | None) -> None:
    if cargo_profile is not None and re.fullmatch(r"[A-Za-z0-9_-]+", cargo_profile) is None:
        raise RunnerError("Cargo profile must be a profile name, not a filesystem path")


class RustTestRunner:
    def __init__(
        self,
        manifest: Manifest,
        metadata: MetadataIndex,
        *,
        target_dir: Path | None = None,
        platform: str | None = None,
        executor: Executor = _default_executor,
        profile: str | None = None,
        cargo_profile: str | None = None,
        no_fail_fast: bool = False,
        command_timeout_seconds: float | None = None,
        env: Mapping[str, str] | None = None,
        cwd: Path = CODEX_RS_ROOT,
        success_output: str = "never",
    ) -> None:
        self.manifest = manifest
        self.manifest_path = DEFAULT_MANIFEST
        self.metadata = metadata
        self.target_dir = (target_dir or metadata.target_directory).resolve()
        self.platform = platform or current_platform()
        self.executor = executor
        self.cwd = cwd
        self.no_fail_fast = no_fail_fast
        if success_output not in SUCCESS_OUTPUT_VALUES:
            raise RunnerError(
                f"--success-output must be one of {', '.join(SUCCESS_OUTPUT_VALUES)}"
            )
        self.success_output = success_output
        _validate_cargo_profile(cargo_profile)
        self.cargo_profile = cargo_profile
        self.base_env = {
            key: value
            for key, value in (os.environ if env is None else env).items()
            # Only helpers built for this run may satisfy a helper lookup.
            if not key.upper().startswith("CARGO_BIN_EXE_")
            and key.upper() not in {"KD4_AGENT_TASK_FIXTURE_SEED"}
        }
        self.environment_updates = local_rust_env(self.base_env, repo_root=REPO_ROOT)
        self.base_env.update(self.environment_updates)
        self.base_env["CODEX_RUST_TEST_LOG_DIR"] = str(
            self.target_dir / "test-runner-logs"
        )
        if command_timeout_seconds is not None:
            if (
                not math.isfinite(command_timeout_seconds)
                or command_timeout_seconds <= 0
            ):
                raise RunnerError(
                    "command timeout must be a finite positive number of seconds"
                )
            self.base_env["CODEX_RUST_TEST_TIMEOUT_SECS"] = str(command_timeout_seconds)
        # Ordinary acceptance must never update or force-pass its expected outputs.
        self.base_env["INSTA_UPDATE"] = "no"
        self.base_env["INSTA_FORCE_PASS"] = "0"
        self.base_env.setdefault("RUST_MIN_STACK", RUST_MIN_STACK_BYTES)
        self._sccache_disabled = False
        self.metrics: ValidationMetrics | None = None
        # CARGO_INCREMENTAL stays untouched on purpose. The shared launcher policy
        # points RUSTC_WRAPPER at sccache, and sccache aborts
        # the whole build when that variable asks for incremental compilation
        # while refusing to honor it when it asks for "0". Leaving it unset lets
        # Cargo's dev/test profile defaults give workspace crates the incremental
        # cache -- the only one a narrow edit/test loop can use -- while sccache
        # still serves the registry dependencies it does cache.
        if profile is not None:
            self.base_env["NEXTEST_PROFILE"] = profile

    def target(self, name: str) -> Target:
        target = self.manifest.target(name)
        self.metadata.validate_target(target)
        return target

    def gate(self, name: str) -> Gate:
        return self.manifest.gate(name)

    def active_helpers(self, target_names: Iterable[str]) -> list[Helper]:
        return self._active_helper_names(
            name for target in target_names for name in self.target(target).all_helpers
        )

    def _active_helper_names(self, names: Iterable[str]) -> list[Helper]:
        selected: list[Helper] = []
        seen: set[str] = set()
        for helper_name in names:
            if helper_name in seen:
                continue
            helper = self.manifest.helpers[helper_name]
            if helper.platform is not None and helper.platform != self.platform:
                continue
            self.metadata.validate_helper(helper)
            selected.append(helper)
            seen.add(helper_name)
        return selected

    def _group_gate_steps(
        self,
        names: Sequence[str],
        *,
        exact_only: bool = False,
        resolved_tests: Mapping[GateStep, tuple[str, ...]] | None = None,
    ) -> list[GateStep]:
        groups: dict[
            tuple[str, str, str | None, frozenset[str]],
            tuple[tuple[str, ...], list[GateStep]],
        ] = {}
        for name in dict.fromkeys(names):
            for step in self.gate(name).steps:
                if exact_only and step.filterset is not None:
                    continue
                if resolved_tests is not None and step in resolved_tests:
                    step = replace(step, tests=resolved_tests[step])
                target = self.target(step.target)
                helpers = tuple(
                    helper.name
                    for helper in self._active_helper_names(
                        target.all_helpers if step.helpers is None else step.helpers
                    )
                )
                # A step proves its tests with exactly its declared helpers, so
                # steps share a run only when both Cargo target and helpers match.
                key = (
                    target.package,
                    target.selector_kind,
                    target.selector_value,
                    frozenset(helpers),
                )
                groups.setdefault(key, (helpers, []))[1].append(step)
        grouped = []
        for helpers, steps in groups.values():
            filters = list(
                dict.fromkeys(self._gate_filter_args(step)[1] for step in steps)
            )
            grouped.append(
                GateStep(
                    steps[0].target,
                    filters[0]
                    if len(filters) == 1
                    else " | ".join(f"({value})" for value in filters),
                    tuple(dict.fromkeys(test for step in steps for test in step.tests))
                    if all(step.tests for step in steps)
                    else (),
                    helpers,
                )
            )
        return grouped

    def plan(self, name: str) -> dict[str, Any]:
        if name in self.manifest.targets:
            target = self.target(name)
            helpers = self.active_helpers([name])
            return {
                "kind": "target",
                "name": name,
                "target_dir": str(self.target_dir),
                "selection": target.selection_args(),
                "helpers": [helper.name for helper in helpers],
                "helper_selection": "upper bound; exact test(=ID) filters or "
                "nextest discovery narrow helpers"
                if target.helpers_by_test_prefix
                else "exact",
                "discovery_builds_test_binary": bool(target.helpers_by_test_prefix),
                "environment_defaults": self.environment_updates,
                "builds": self._helper_build_commands(helpers),
                "run": self._run_command(target, [], report_results=True),
            }
        if name in self.manifest.gates:
            grouped = self._group_gate_steps([name])
            helpers = self._active_helper_names(
                helper for step in grouped for helper in step.helpers or ()
            )
            steps = []
            for batch in self._gate_batches(grouped):
                targets = [self.target(step.target) for step in batch]
                # Steps of one batch share the invocation that proves them.
                run = self._gate_run_command(
                    targets[0],
                    self._batch_filter_args(targets, batch),
                    batch=targets[1:],
                )
                for target, step in zip(targets, batch):
                    steps.append(
                        {
                            "target": step.target,
                            "tests": list(step.tests),
                            "helpers": list(step.helpers or ()),
                            "list": self._list_command(
                                target, self._gate_filter_args(step)
                            ),
                            "run": run,
                        }
                    )
            return {
                "kind": "gate",
                "name": name,
                "target_dir": str(self.target_dir),
                "helpers": [helper.name for helper in helpers],
                "builds": self._helper_build_commands(helpers),
                "steps": steps,
            }
        raise RunnerError(f"unknown named Rust test target or gate {name!r}")

    def gates_for(self, paths: Sequence[str], *, cwd: Path | None = None) -> dict[str, Any]:
        """Gates whose declared test IDs live in the module owning each file.

        Ownership comes from Cargo target roots, module file layout, and sibling
        `#[path]` declarations. A gate that reaches a changed module only through
        a caller elsewhere is not selected, and filter-only steps are reported
        rather than listed with nextest.
        """
        selected: dict[str, list[str]] = {}
        target_paths: dict[str, list[str]] = {}
        unmapped: list[str] = []
        not_evaluated: set[str] = set()
        # Capture each sibling declaration once for this ownership query, not
        # once per input/target/recursive parent. Never carry it into another
        # query or validation run: additions, removals and edits must be seen.
        declarations: dict = {}
        for raw in dict.fromkeys(paths):
            owners = _rust_module_owners(
                self.metadata, raw, cwd or Path.cwd(), declarations
            )
            target_paths[raw] = []
            if not owners:
                unmapped.append(raw)
                continue
            package, modules, registered = owners
            target_paths[raw] = sorted(
                target.name for target in self.manifest.targets.values()
                if target.package == package
                and (target.selector_kind, target.selector_value) in registered
            )
            gates = []
            for gate in self.manifest.gates.values():
                for step in gate.steps:
                    target = self.manifest.targets[step.target]
                    if target.package != package:
                        continue
                    for kind, value, module in modules:
                        if (target.selector_kind, target.selector_value) != (kind, value):
                            continue
                        if not step.tests:
                            not_evaluated.add(gate.name)
                        elif any(
                            not module or test == module or test.startswith(f"{module}::")
                            for test in step.tests
                        ):
                            gates.append(gate.name)
            selected[raw] = list(dict.fromkeys(gates))
        return {
            "gates": sorted({gate for gates in selected.values() for gate in gates}),
            "paths": selected,
            "targets": sorted({name for names in target_paths.values() for name in names}),
            "target_paths": target_paths,
            "unresolved_targets": [raw for raw, names in target_paths.items() if not names],
            "ambiguous_targets": {
                raw: names for raw, names in target_paths.items() if len(names) > 1
            },
            "target_scope": "declared module registration only; cfg and macros are not "
            "evaluated; multiple owners are reported, not selected",
            **({"unmapped": unmapped} if unmapped else {}),
            **(
                {"filter_only_gates_not_evaluated": sorted(not_evaluated)}
                if not_evaluated
                else {}
            ),
            "scope": "direct module ownership; callers in other modules are not selected",
        }

    def run_target(
        self,
        name: str,
        filter_args: Sequence[str],
        *,
        no_fail_fast: bool | None = None,
        allow_all: bool = False,
    ) -> dict[str, list[str]]:
        args = validate_filtering_args(filter_args)
        target = self.target(name)
        require_core_lib_filter(target, args, allow_all=allow_all)
        helpers = self.active_helpers([name])
        required_ids = _required_exact_test_ids(args)
        selected = required_ids
        discovered_build: dict[str, Any] = {}
        print(
            f"Rust test target {name}: {subprocess.list2cmdline(target.selection_args())}; "
            f"target-dir={self.target_dir}; helper upper bound="
            + (", ".join(helper.name for helper in helpers) or "none"),
            file=sys.stderr,
        )
        if self.environment_updates:
            print(
                "Rust environment defaults: "
                + json.dumps(self.environment_updates, sort_keys=True),
                file=sys.stderr,
            )
        # Narrowing is worthwhile only if some prefix can omit an active helper.
        # Otherwise the direct run already rejects an empty selection.
        if any(
            len(self._active_helper_names(names)) < len(helpers)
            for names in (target.helpers, *target.helpers_by_test_prefix.values())
        ):
            # Exact IDs already bound the selection, so their helpers need no
            # discovery invocation; an ID the run cannot select only adds helpers.
            if selected is None:
                # Let nextest interpret filters, exclusions and ignored tests.
                # Listing builds the unit binary without unrelated helpers.
                print(
                    "Selecting tests with nextest list (this compiles the test binary).",
                    file=sys.stderr,
                )
                try:
                    selected = self._list_tests(target, args, discovered_build=discovered_build)
                    if required_ids is None:
                        required_ids = sorted(selected)
                except RunnerError:
                    self._report_failed_tests(name, [], [], None)
                    raise
            required = []
            for test in selected:
                required.extend(
                    next(
                        (
                            names
                            for prefix, names in target.helpers_by_test_prefix.items()
                            if test.startswith(prefix)
                        ),
                        target.helpers,
                    )
                )
            helpers = self._active_helper_names(required)
        try:
            env = self._helper_environment([target], helpers, self._build_helpers(helpers))
        except RunnerError:
            self._report_failed_tests(name, [], [], selected)
            raise
        # Discovery already compiled this invocation's test binary. Inserting a
        # helper build can change Cargo's feature fingerprints and otherwise
        # compile it again. Reuse that build, not test results or a previous run.
        binaries_metadata = (
            self._retain_text(json.dumps(discovered_build), prefix="discovered-build-")
            if discovered_build else None
        )
        command = self._run_command(
            target, args, no_fail_fast=no_fail_fast, report_results=True,
            binaries_metadata=binaries_metadata,
        )
        failure = None
        try:
            result = self._checked(command, env=env, capture=self._test_run_capture())
        except RunnerError as error:
            if error.result is None:
                self._report_failed_tests(name, [], [], selected)
                raise
            failure = error
            result = error.result
        binary_id = _nextest_binary_id(target)
        outcomes: dict[str, list[str]] = {}
        for status, binary, test in _nextest_results(result):
            if binary == binary_id:
                outcomes.setdefault(test, []).append(status)
        passed = sorted(
            test
            for test, statuses in outcomes.items()
            if statuses in (["PASS"], ["LEAK"])
        )
        receipts = {binary_id: passed} if passed and result.returncode in {0, 100} else {}
        helper_ids = frozenset(helper.name for helper in helpers)
        receipts = ExecutionReceipts(receipts,
            ((binary_id, helper_ids, test) for test in receipts.get(binary_id, [])),
            ((binary_id, helper_ids, test) for test in required_ids) if required_ids is not None else None)
        missing = sorted(set(required_ids or ()) - set(passed))
        if failure is None and required_ids is not None and missing:
            failure = RunnerError(
                f"target {name!r} has unfulfilled exact test IDs: {missing}",
                outcome="not_executed", result=result)
        elif failure is None and (
            not passed or any(statuses not in (["PASS"], ["LEAK"]) for statuses in outcomes.values())
        ):
            failure = RunnerError(
                f"target {name!r} did not report completed tests passed exactly once",
                outcome="not_executed", result=result)
        # These are execution receipts, not a cache or proof after input changes.
        rendered = json.dumps({"completed_tests": receipts}, sort_keys=True)
        path = self._retain_text(rendered, prefix="completed-tests-")
        print(
            f"Completed test receipts (reuse only while inputs match): {path or rendered}",
            file=sys.stderr,
        )
        if failure is not None:
            failure.completed_tests = receipts
            failed = sorted(test for test, statuses in outcomes.items()
                            if any(status not in {"PASS", "LEAK"} for status in statuses))
            self._report_failed_tests(name, passed, failed, selected,
                                      selected_count=_nextest_selected_count(result) if selected is None else None)
            raise failure
        return receipts

    def _report_failed_tests(
        self, name: str, passed: Sequence[str], failed: Sequence[str],
        selected: Sequence[str] | None,
        *, selected_count: int | None = None,
    ) -> None:
        """Keep the failure inventory outside the bounded diagnostic excerpt."""
        not_run = (len(set(selected) - set(passed) - set(failed)) if selected is not None
                   else max(0, selected_count - len(set(passed) | set(failed)))
                   if selected_count is not None else "unknown (selection count unavailable)")
        print(f"Test results {name}: passed={len(passed)}, failed={len(failed)}, "
              f"not-run={not_run}", file=sys.stderr)
        print("Failed test IDs (complete):", file=sys.stderr)
        for test in sorted(set(failed)):
            print(f"{_nextest_binary_id(self.target(name))} {test}", file=sys.stderr)
        if not failed:
            print("(none reported; no failed-only rerun available)", file=sys.stderr)
            return
        command = ["python", "scripts/rust_test_runner.py", "--manifest",
                   str(self.manifest_path), "--target-dir", str(self.target_dir)]
        if self.cargo_profile:
            command.extend(["--cargo-profile", self.cargo_profile])
        command.extend(["run-target", name, "--no-fail-fast"])
        if self.base_env.get("NEXTEST_PROFILE"):
            command.extend(["--profile", self.base_env["NEXTEST_PROFILE"]])
        command.extend(["--", "--run-ignored", "all", "--ignore-default-filter", "-E",
                        " | ".join(f"test(={test})" for test in sorted(set(failed)))])
        # Quote every argument: even a single test expression contains shell
        # metacharacters. PowerShell and POSIX shells escape apostrophes differently.
        quote = (lambda value: "'" + value.replace("'", "''") + "'") if os.name == "nt" else shlex.quote
        print("Rerun failed only (from repository root):\n"
              + ("& " if os.name == "nt" else "")
              + " ".join(quote(arg) for arg in command), file=sys.stderr)

    def check_gates(
        self, names: Sequence[str], *, include_generated: bool = True
    ) -> dict[GateStep, tuple[str, ...]]:
        """Verify declared filter/ID parity without running tests or helpers."""
        if not names:
            raise RunnerError("at least one gate is required")
        # Generated exact filters can share discovery. Explicit filters must
        # prove their own contract: another step must not hide over-selection.
        discovery_steps = (
            self._group_gate_steps(names, exact_only=True) if include_generated else []
        )
        discovery_steps.extend(
            step
            for name in dict.fromkeys(names)
            for step in self.gate(name).steps
            if step.filterset is not None
        )
        resolved_tests = {}
        # Invocation-local discovery only, never cached test proof. Identical
        # selections shared by multiple gates need one Cargo/list pass, while
        # every declaration still checks its own exact/ignored-test contract.
        listings: dict[tuple[str, ...], dict[str, bool]] = {}
        for step in discovery_steps:
            target = self.target(step.target)
            filter_args = self._gate_filter_args(step)
            key = tuple(self._list_command(target, filter_args))
            if key not in listings:
                listings[key] = self._list_tests(target, filter_args)
            listed = listings[key]
            actual = set(listed)
            expected = set(step.tests) if step.tests else actual
            if not actual:
                raise RunnerError(
                    f"gate {step.target!r} selected no tests", outcome="not_executed"
                )
            if actual != expected:
                missing = sorted(expected - actual)
                unexpected = sorted(actual - expected)
                details: list[str] = []
                if missing:
                    details.append(f"missing={missing}")
                if unexpected:
                    details.append(f"unexpected={unexpected}")
                raise RunnerError(
                    f"gates {list(names)!r} step {step.target!r} selected the wrong test-ID set: "
                    + ", ".join(details)
                )
            ignored = sorted(test for test, is_ignored in listed.items() if is_ignored)
            if ignored:
                raise RunnerError(
                    f"gate requires ignored tests that would not execute: {ignored}",
                    outcome="skipped",
                )
            if not step.tests:
                resolved_tests[step] = tuple(sorted(actual))
        return resolved_tests

    def run_gate(self, name: str) -> None:
        self.run_gates([name])

    def run_gates(
        self, names: Sequence[str], *, quiet: bool = False, discover: bool = False
    ) -> dict[str, list[str]]:
        """Verify exact selections, build helpers once, and execute each test once
        per declared helper set with only that set exported."""
        if not names:
            raise RunnerError("at least one gate is required")
        # Exact generated selections are proved by completed results below.
        # Explicit filters must always prove parity before batching: execution
        # of the declared IDs alone cannot detect an over-broad source filter.
        try:
            resolved_tests = self.check_gates(names, include_generated=discover)
        except RunnerError:
            for name in dict.fromkeys(names):
                for step in self.gate(name).steps:
                    self._report_failed_tests(step.target, [], [], step.tests or None)
            raise
        grouped = self._group_gate_steps(names, resolved_tests=resolved_tests)

        required_by_gate = {
            name: self._group_gate_steps([name], resolved_tests=resolved_tests)
            for name in dict.fromkeys(names)
        }
        proved: set[tuple[str, frozenset[str], str]] = set()

        def completed_gates() -> dict[str, list[str]]:
            gates = {
                name: sorted({test for step in steps for test in step.tests})
                for name, steps in required_by_gate.items()
                if all(
                    (
                        _nextest_binary_id(self.target(step.target)),
                        frozenset(step.helpers or ()),
                        test,
                    )
                    in proved
                    for step in steps
                    for test in step.tests
                )
            }
            required = {
                (_nextest_binary_id(self.target(step.target)), frozenset(step.helpers or ()), test)
                for step in grouped for test in step.tests
            }
            return ExecutionReceipts(gates, proved, required, gates=gates)

        failures: list[RunnerError] = []
        reports = {step: (step.target, [], [], step.tests) for step in grouped}
        try:
            artifacts = self._build_helpers(
                self._active_helper_names(
                    helper for step in grouped for helper in step.helpers or ()
                )
            )
        except RunnerError as error:
            if error.outcome in {"cancelled", "timed_out", "cleanup_failed"}:
                raise
            failures.append(error)
            artifacts = {}
        for batch in self._gate_batches(grouped):
            helpers = self._active_helper_names(batch[0].helpers or ())
            # A failed shared build proves no helper artifact, but must not
            # suppress independent helper-free tests or trigger a build retry.
            if any(helper.name not in artifacts for helper in helpers):
                continue
            targets = [self.target(step.target) for step in batch]
            try:
                env = self._helper_environment(targets, helpers, artifacts)
                result = self._checked(
                    self._gate_run_command(
                        targets[0],
                        self._batch_filter_args(targets, batch),
                        batch=targets[1:],
                    ),
                    env=env,
                    capture=self._test_run_capture(),
                )
            except RunnerError as error:
                if error.outcome in {"cancelled", "timed_out", "cleanup_failed"}:
                    if not failures:
                        error.completed_gates = completed_gates()
                        raise
                    # Gate runs capture their output, so an earlier group's
                    # failure is reported nowhere else; the stop keeps its outcome.
                    raise RunnerError(
                        f"{error}\ngate runs that failed before the stop:\n"
                        + "\n".join(str(failure) for failure in failures),
                        outcome=error.outcome,
                        completed_gates=completed_gates(),
                    ) from error
                failures.append(error)
                # Nextest's test-failure exit still carries independent PASS
                # receipts. Build/transport/abnormal exits are not test proof.
                if error.result is None:
                    continue
                result = error.result
            # Require completed per-test results from this execution, not just
            # a successful exit or discovery. Suppress summary repetitions and
            # unrelated filtered-out skips, and disallow retries for this proof.
            # Each result must come from the binary of the step declaring it.
            expected = {
                _nextest_binary_id(target): set(step.tests)
                for target, step in zip(targets, batch)
            }
            passed: dict[tuple[str, str], int] = {}
            failed: set[tuple[str, str]] = set()
            unexpected = False
            for status, binary, test in _nextest_results(result):
                if test in expected.get(binary, ()):
                    key = (binary, test)
                    if status in {"PASS", "LEAK"}:
                        passed[key] = min(2, passed.get(key, 0) + 1)
                    else:
                        failed.add(key)
                else:
                    unexpected = True
            required = {
                (binary, test) for binary, tests in expected.items() for test in tests
            }
            for target, step in zip(targets, batch):
                binary = _nextest_binary_id(target)
                reports[step] = (step.target,
                    sorted(test for owner, test in passed if owner == binary and (owner, test) not in failed),
                    sorted(test for owner, test in failed if owner == binary), step.tests)
            if result.returncode not in {0, 100}:
                # Preserve observed diagnostics, but abnormal exits cannot
                # establish per-test proof or missing-result proof failures.
                continue
            if (
                not unexpected
                and all(count == 1 for count in passed.values())
                and not failed.intersection(passed)
                and (result.returncode == 0 or set(passed) < required)
            ):
                proved.update(
                    (binary, frozenset(batch[0].helpers or ()), test)
                    for binary, test in passed
                )
            if (
                unexpected
                or failed
                or set(passed) != required
                or any(count != 1 for count in passed.values())
            ):
                # `--status-level pass` hides SKIP lines and `--no-tests=fail`
                # fails an empty run before this point, so a missing or ignored
                # test is only known as not executed; `check-gates` names it.
                steps = ", ".join(repr(step.target) for step in batch)
                failures.append(
                    RunnerError(
                        f"gate {steps} did not report every required test passed exactly once: "
                        + self._gate_failure_summary(required, passed, unexpected)
                        # A test-failure exit already contributed the command,
                        # diagnostic excerpt and retained-log paths above. Add
                        # only the missing proof, not the same failure twice.
                        + ("\n" + self._failure_detail(result) if result.returncode == 0 else ""),
                        outcome="not_executed",
                    )
                )
            elif not quiet:
                for step in batch:
                    print(f"gate {step.target}: {len(step.tests)} passed")
        if failures:
            for report in reports.values():
                self._report_failed_tests(*report)
            outcomes = {error.outcome for error in failures}
            raise RunnerError(
                "gate runs failed after completing the unblocked selected targets:\n"
                + "\n".join(str(error) for error in failures),
                outcome=next(iter(outcomes)) if len(outcomes) == 1 else "failed",
                completed_gates=completed_gates(),
            )
        return completed_gates()

    def _gate_filter_args(self, step: GateStep) -> list[str]:
        expression = step.filterset or " | ".join(
            f"test(={test})" for test in step.tests
        )
        return ["-E", expression]

    def _gate_failure_summary(
        self,
        required: set[tuple[str, str]],
        passed: Mapping[tuple[str, str], int],
        unexpected: bool,
    ) -> str:
        missing = sorted(
            f"{binary} {test}" for binary, test in required - passed.keys()
        )
        duplicates = sorted(
            f"{binary} {test}" for (binary, test), count in passed.items() if count != 1
        )
        inventory = json.dumps(
            {
                "expected": sorted(f"{binary} {test}" for binary, test in required),
                "passed": {
                    f"{binary} {test}": n for (binary, test), n in passed.items()
                },
                "unexpected": unexpected,
            },
            indent=2,
        )
        path = self._retain_text(inventory, prefix="gate-proof-")
        if path is None:
            return inventory
        summary = (
            f"expected={len(required)}, passed={len(passed)}, "
            f"missing({len(missing)})={missing[:8]}, "
            f"duplicates({len(duplicates)})={duplicates[:8]}, unexpected={unexpected}"
        )
        return summary[:MAX_FAILURE_STREAM_CHARS] + f"\nFull gate proof: {path}"

    def _gate_batches(self, grouped: Sequence[GateStep]) -> list[list[GateStep]]:
        """Share one nextest invocation among compatible grouped steps.

        Cargo resolves one package's test features identically for each of its
        test targets, so steps of one package with the same helper set run in a
        single invocation: one metadata and build pass whose graph compiles
        their test binaries together. Integration tests also receive their
        package's own binaries, so they batch only with each other.
        """
        batches: dict[tuple[str, frozenset[str], bool], list[GateStep]] = {}
        for step in grouped:
            target = self.target(step.target)
            key = (
                target.package,
                frozenset(step.helpers or ()),
                target.selector_kind == "test",
            )
            batches.setdefault(key, []).append(step)
        return list(batches.values())

    @staticmethod
    def _batch_filter_args(
        targets: Sequence[Target], steps: Sequence[GateStep]
    ) -> list[str]:
        def exact(step: GateStep) -> str:
            return (
                " | ".join(f"test(={test})" for test in step.tests)
                if step.tests
                else step.filterset or "none()"
            )

        if len(steps) == 1:
            return ["-E", exact(steps[0])]
        # Qualify each step's IDs by binary so a same-named test in a sibling
        # binary of the batch is never selected.
        return [
            "-E",
            " | ".join(
                f"(binary_id(={_nextest_binary_id(target)}) & ({exact(step)}))"
                for target, step in zip(targets, steps)
            ),
        ]

    def _gate_run_command(
        self,
        target: Target,
        filter_args: Sequence[str],
        *,
        batch: Sequence[Target] = (),
    ) -> list[str]:
        # The proof owns its execution policy: no profile may let one failing
        # test cancel another step's tests, least of all in a shared batch.
        return [
            *self._run_command(target, filter_args, no_fail_fast=True, batch=batch),
            "--color",
            "never",
            "--status-level",
            "pass",
            "--final-status-level",
            "none",
            "--retries",
            "0",
        ]

    def _selection_command(
        self, verb: str, target: Target, *batch: Target
    ) -> list[str]:
        # Batched targets share `target`'s package and add only their selector.
        return [
            "cargo",
            "nextest",
            verb,
            "--target-dir",
            str(self.target_dir),
            *(["--cargo-profile", self.cargo_profile] if self.cargo_profile else []),
            *target.selection_args(),
            *(arg for other in batch for arg in other.selection_args()[2:]),
        ]

    def _list_command(self, target: Target, filter_args: Sequence[str]) -> list[str]:
        return [*self._selection_command("list", target), "-T", "json", *filter_args]

    def _run_command(
        self,
        target: Target,
        filter_args: Sequence[str],
        *,
        no_fail_fast: bool | None = None,
        batch: Sequence[Target] = (),
        report_results: bool = False,
        binaries_metadata: Path | None = None,
    ) -> list[str]:
        args = validate_filtering_args(filter_args)
        keep_going = self.no_fail_fast if no_fail_fast is None else no_fail_fast
        return [
            *(
                ["cargo", "nextest", "run", "--binaries-metadata", str(binaries_metadata)]
                if binaries_metadata is not None
                else self._selection_command("run", target, *batch)
            ),
            "--no-tests=fail",
            "--show-progress",
            "none",
            "--success-output",
            self.success_output,
            *(["--no-fail-fast"] if keep_going else []),
            *(
                [
                    "--color",
                    "never",
                    "--status-level",
                    "pass",
                    "--final-status-level",
                    "none",
                    "--retries",
                    "0",
                ]
                if report_results
                else []
            ),
            *args,
        ]

    def _test_run_capture(self) -> str:
        # Nextest writes status lines and passing-test output to stderr. Its log
        # is retained for receipts either way; stream it only when requested.
        return CAPTURE_BOTH if self.success_output == "never" else CAPTURE_STDOUT

    def _helper_build(self, helpers: Sequence[Helper]) -> list[str] | None:
        # Cargo unifies dependency features across every `-p` package, so a
        # scope of only the selected helpers' packages compiles a separate
        # feature variant of a helper's dependency graph for each combination
        # of helpers. Resolve against every active helper package instead: each
        # helper keeps one variant that every run reuses, and `--bin` still
        # builds exactly the selected binaries in a single invocation.
        if not helpers:
            return None
        packages = dict.fromkeys(
            helper.package
            for helper in self.manifest.helpers.values()
            if helper.platform in (None, self.platform)
            and helper.package in self.metadata.packages
        )
        return [
            "cargo",
            "build",
            "--message-format=json-render-diagnostics",
            "--target-dir",
            str(self.target_dir),
            *(["--profile", self.cargo_profile] if self.cargo_profile else []),
            *(arg for package in packages for arg in ("-p", package)),
            *(arg for helper in helpers for arg in ("--bin", helper.binary)),
        ]

    def _helper_build_commands(self, helpers: Sequence[Helper]) -> list[list[str]]:
        command = self._helper_build(helpers)
        return [] if command is None else [command]

    def _list_tests(
        self, target: Target, filter_args: Sequence[str],
        *, discovered_build: dict[str, Any] | None = None,
    ) -> dict[str, bool]:
        args = _list_only_args(validate_filtering_args(filter_args))
        result = self._checked(
            self._list_command(target, args), env=self.base_env, capture=CAPTURE_STDOUT
        )
        output = _stdout_text(result)
        root: dict[str, Any] = {}
        tests = parse_nextest_list(
            output, parsed_payload=root if discovered_build is not None else None
        )
        if not tests:
            raise RunnerError(
                f"named target {target.name!r} selected zero tests with args {args!r}",
                outcome="zero_tests",
            )
        if discovered_build is not None:
            suites = root["rust-suites"]
            binary_id = _nextest_binary_id(target)
            # Nextest flattens RustTestBinarySummary into each full suite.
            # Preserve build metadata (including non-test binaries and output
            # directories), and project only the selected target's binary.
            # Older/incomplete metadata or a failed retention falls back to
            # Cargo, never to an unverified executable or a cached pass.
            fields = ("binary-id", "binary-name", "package-id", "kind",
                      "binary-path", "build-platform")
            if (
                isinstance(root.get("rust-build-meta"), dict)
                and set(suites) == {binary_id}
                and all(isinstance(suites[binary_id].get(key), str) for key in fields)
                and suites[binary_id]["binary-id"] == binary_id
            ):
                discovered_build.update({
                    "rust-build-meta": root["rust-build-meta"],
                    "rust-binaries": {
                        binary_id: {key: suites[binary_id][key] for key in fields}
                    },
                })
        return tests

    def _build_helpers(self, helpers: Sequence[Helper]) -> dict[str, Path]:
        print(
            "Selected Rust helpers: "
            + (", ".join(helper.name for helper in helpers) or "none"),
            file=sys.stderr,
        )
        command = self._helper_build(helpers)
        if command is None:
            return {}
        result = self._checked(command, env=self.base_env, capture=CAPTURE_STDOUT)
        return self._helper_artifacts(helpers, _output_lines(result, "stdout"))

    def _helper_environment(
        self,
        targets: Sequence[Target],
        helpers: Sequence[Helper],
        artifacts: Mapping[str, Path],
    ) -> dict[str, str]:
        env = dict(self.base_env)
        # `cargo_bin` falls back to `<lane>/debug/<name>` when its variable is
        # unset, so an undeclared helper would silently run whatever an earlier
        # build left in this persistent lane. Point every manifest helper at a
        # path that cannot exist; the selected helpers below replace it. Cargo
        # rebuilds a package's own binaries for its integration tests.
        fresh_packages = {
            target.package for target in targets if target.selector_kind == "test"
        }
        for helper in self.manifest.helpers.values():
            if helper.package not in fresh_packages:
                undeclared = str(
                    self.target_dir / "test-runner-undeclared-helpers" / helper.binary
                )
                env[f"CARGO_BIN_EXE_{helper.binary}"] = undeclared
                env[f"CARGO_BIN_EXE_{helper.binary.replace('-', '_')}"] = undeclared
        for helper in helpers:
            executable = artifacts[helper.name]
            env[f"CARGO_BIN_EXE_{helper.binary}"] = str(executable)
            env[f"CARGO_BIN_EXE_{helper.binary.replace('-', '_')}"] = str(executable)
        resource_dirs = self._stage_windows_resources(helpers, artifacts)
        if resource_dirs:
            # Native sandbox setup also locates helpers by executable name. Prefer
            # this build over an older installed executable inherited through PATH
            # without exposing the other binaries an earlier build left in the lane.
            env["PATH"] = os.pathsep.join(
                [*map(str, resource_dirs), env.get("PATH", "")]
            )
        return env

    def _stage_windows_resources(
        self, helpers: Sequence[Helper], artifacts: Mapping[str, Path]
    ) -> list[Path]:
        """Mirror exactly the selected sandbox helpers beside test executables.

        Windows sandbox code resolves these helpers in `codex-resources` next to
        the running test binary before it consults PATH. That directory persists
        in the lane, so a copy staged by an earlier run would otherwise satisfy
        an undeclared helper or outlive a rebuild.
        """
        if self.platform != "windows":
            return []
        selected = {
            helper.binary: artifacts[helper.name]
            for helper in helpers
            if helper.binary in WINDOWS_RESOURCE_HELPERS
        }
        resource_dirs = list(
            dict.fromkeys(
                profile_dir / "deps" / "codex-resources"
                for profile_dir in (
                    self.target_dir
                    / (
                        {"dev": "debug", "test": "debug", "bench": "release"}.get(
                            self.cargo_profile, self.cargo_profile
                        )
                        or "debug"
                    ),
                    *(executable.parent for executable in selected.values()),
                )
            )
        )
        for resources in resource_dirs:
            for binary in WINDOWS_RESOURCE_HELPERS:
                staged = resources / f"{binary}.exe"
                source = selected.get(binary)
                try:
                    if source is None:
                        staged.unlink(missing_ok=True)
                    elif not staged.is_file() or not filecmp.cmp(
                        source, staged, shallow=False
                    ):
                        resources.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(source, staged)
                except OSError as exc:
                    action = "remove undeclared" if source is None else "stage"
                    raise RunnerError(
                        f"cannot {action} Windows helper {staged}: {exc}"
                    ) from exc
        return resource_dirs if selected else []

    def _helper_artifacts(
        self, helpers: Sequence[Helper], output: str | Iterable[str]
    ) -> dict[str, Path]:
        # A grouped build has one log: decode it once, retaining every match
        # so duplicate artifacts cannot silently satisfy a helper's proof.
        executables: dict[tuple[str, str], list[Path]] = {
            (self.metadata.package_id(helper.package), helper.binary): []
            for helper in helpers
        }
        for line in output.splitlines() if isinstance(output, str) else output:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                not isinstance(message, dict)
                or message.get("reason") != "compiler-artifact"
            ):
                continue
            target = message.get("target")
            if not isinstance(target, dict):
                continue
            package_id, binary = message.get("package_id"), target.get("name")
            if (
                isinstance(package_id, str)
                and isinstance(binary, str)
                and (package_id, binary) in executables
                and isinstance(target.get("kind"), list)
                and "bin" in target["kind"]
                and isinstance(message.get("executable"), str)
            ):
                executables[package_id, binary].append(Path(message["executable"]))
        artifacts = {}
        for helper in helpers:
            matches = executables[self.metadata.package_id(helper.package), helper.binary]
            if len(matches) != 1 or not matches[0].is_file():
                raise RunnerError(
                    f"helper build did not produce exactly one executable artifact for "
                    f"{helper.package}/{helper.binary}"
                )
            artifacts[helper.name] = matches[0].resolve()
        return artifacts

    def _execute(
        self,
        args: Sequence[str],
        *,
        env: Mapping[str, str],
        capture: str,
        timings: dict[str, Any] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        try:
            with ExitStack() as cleanup:
                child_env = dict(env)
                # All nextest workers in this invocation share an immutable,
                # freshly migrated seed. Never reuse one from a previous build.
                if (
                    list(args[:3]) == ["cargo", "nextest", "run"]
                    and "codex-agent-task-store" in args
                ):
                    child_env["KD4_AGENT_TASK_FIXTURE_SEED"] = cleanup.enter_context(
                        tempfile.TemporaryDirectory(prefix="kd4-agent-task-seed-")
                    )
                return self.executor(
                    list(args), cwd=self.cwd, env=child_env, capture=capture,
                    **({"timings": timings} if self.executor is _default_executor else {}),
                )
        except CleanupFailed as error:
            (self.target_dir / ".lane-cleanup-unconfirmed").write_text(str(error))
            raise RunnerError(str(error), outcome="cleanup_failed") from error

    def _checked(
        self,
        args: Sequence[str],
        *,
        env: Mapping[str, str],
        capture: str,
    ) -> subprocess.CompletedProcess[str]:
        effective_env = dict(env)
        if self._sccache_disabled:
            effective_env["RUSTC_WRAPPER"] = ""
        rendered = subprocess.list2cmdline(list(args))
        if len(rendered) > MAX_FAILURE_STREAM_CHARS:
            path = self._retain_text(rendered, prefix="command-")
            if path is not None:
                rendered = rendered[:256] + f"...\nFull command: {path}"
        for attempt in range(2):
            phase = (
                "compile/discover"
                if list(args[:3]) == ["cargo", "nextest", "list"]
                else "test-only"
                if "--binaries-metadata" in args
                else "helper-build"
                if list(args[:2]) == ["cargo", "build"]
                else "build/test"
            )
            command_metrics = (
                self.metrics.command(args, phase, attempt + 1)
                if self.metrics is not None else None
            )
            started = time.monotonic()
            result = None
            print(
                f"Rust phase {phase}: starting {rendered}",
                file=sys.stderr,
            )
            try:
                result = self._execute(
                    args, env=effective_env, capture=capture, timings=command_metrics,
                )
            except BaseException as error:
                if command_metrics is not None:
                    command_metrics["outcome"] = (
                        "cancelled" if isinstance(error, KeyboardInterrupt)
                        else getattr(error, "outcome", "failed")
                    )
                raise
            finally:
                elapsed = time.monotonic() - started
                with self.metrics.phase("reconciliation") if self.metrics else nullcontext():
                    reported = (
                        self._reported_durations(result) if result is not None else {}
                    )
                if command_metrics is not None:
                    command_metrics["wall_seconds"] = elapsed
                    command_metrics["reported_build_seconds"] = reported.get("cargo-reported")
                    command_metrics["reported_test_seconds"] = reported.get("tests-reported")
                    if result is not None:
                        command_metrics["exit_code"] = result.returncode
                        command_metrics["outcome"] = "passed" if result.returncode == 0 else "failed"
                    self.metrics.checkpoint()
                print(
                    f"Rust phase {phase}: wall={elapsed:.3f}s; "
                    f"exit={result.returncode if result is not None else 'interrupted'}"
                    + "".join(
                        f"; {key}={value:.3f}s" for key, value in reported.items()
                    ),
                    file=sys.stderr,
                )
            if attempt or not self._cache_transport_failure(
                args, effective_env, result
            ):
                break
            print(
                "Compiler cache connection failed; retrying compilation once without sccache.\n"
                + self._failure_detail(result, include_stdout=False),
                file=sys.stderr,
            )
            self._sccache_disabled = True
            self.base_env["RUSTC_WRAPPER"] = ""
            effective_env["RUSTC_WRAPPER"] = ""
        if result.returncode != 0:
            # A CAPTURE_STDOUT command captured only machine-readable output and
            # already streamed its diagnostics to the terminal.
            detail = self._failure_detail(
                result, include_stdout=capture == CAPTURE_BOTH
            )
            if result.returncode in (-1, 0xFFFFFFFF):
                detail = (
                    "Process exited 0xFFFFFFFF without a normal Cargo exit code. "
                    "Inspect retained logs and process-owner/OS termination evidence; "
                    "this is not classified as a test failure or retried automatically.\n"
                    + detail
                )
            if detail:
                raise RunnerError(
                    f"command failed ({rendered}), exit code {result.returncode}:\n{detail}",
                    result=result,
                )
            raise RunnerError(
                f"command failed ({rendered}), exit code {result.returncode}",
                result=result,
            )
        return result

    @staticmethod
    def _reported_durations(
        result: subprocess.CompletedProcess[str],
    ) -> dict[str, float]:
        """Separate Cargo/Nextest-reported times without another build or test run."""
        durations: dict[str, float] = {}
        for stream in ("stdout", "stderr"):
            for line in _output_lines(result, stream):
                line = re.sub(r"\x1b\[[0-9;]*m", "", line)
                if match := re.search(
                    r"Finished .* in ((?:[\d.]+(?:ms|[hms])\s*)+)", line
                ):
                    units = {"h": 3600, "m": 60, "s": 1, "ms": 0.001}
                    durations["cargo-reported"] = sum(
                        float(value) * units[unit]
                        for value, unit in re.findall(r"([\d.]+)(ms|[hms])", match[1])
                    )
                if match := re.search(r"Summary\s+\[\s*([\d.]+)s\]", line):
                    durations["tests-reported"] = float(match[1])
        return durations

    @staticmethod
    def _cache_transport_failure(
        args: Sequence[str],
        env: Mapping[str, str],
        result: subprocess.CompletedProcess[str],
    ) -> bool:
        wrapper = (
            env.get("RUSTC_WRAPPER", "").replace("\\", "/").rsplit("/", 1)[-1].lower()
        )
        if not result.returncode or wrapper not in {"sccache", "sccache.exe"}:
            return False
        if list(args[:2]) != ["cargo", "build"] and list(args[:3]) not in (
            ["cargo", "nextest", "list"],
            ["cargo", "nextest", "run"],
        ):
            return False
        transport = compile_failed = False
        for stream in ("stdout", "stderr"):
            for line in _output_lines(result, stream):
                # Never replay tests or retry ordinary compiler diagnostics.
                if (
                    "Nextest run ID" in line
                    or "error[E" in line
                    or '"level":"error"' in line
                ):
                    return False
                compile_failed |= "error: could not compile" in line
                transport |= (
                    "sccache: caused by: error reading compile response from server"
                    in line
                )
                transport |= "sccache: caused by: failed to connect to server" in line
        # A failed discovery can lose Cargo's final compilation summary when
        # sccache disconnects. Listing cannot execute tests, so it remains safe
        # to retry without that summary; keep the stricter run/build guard.
        return transport and (
            compile_failed or list(args[:3]) == ["cargo", "nextest", "list"]
        )

    def _retain_text(self, text: str, *, prefix: str) -> Path | None:
        try:
            log_dir = self.target_dir / "test-runner-logs"
            log_dir.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                newline="",
                suffix=".log",
                prefix=prefix,
                dir=log_dir,
                delete=False,
            ) as log:
                log.write(text)
                return Path(log.name)
        except OSError:
            return None

    def _failure_detail(
        self,
        result: subprocess.CompletedProcess[str],
        *,
        include_stdout: bool = True,
    ) -> str:
        streams = [
            (name, _failure_diagnostic_excerpt(result, name, output))
            for name, output in (
                ("stdout", result.stdout if include_stdout else None),
                ("stderr", result.stderr),
            )
            if output
        ]
        full_detail = "\n".join(f"{name}:\n{output}" for name, output in streams)
        paths = [
            f"Full {name}: {path}"
            for name in ("stdout", "stderr")
            if (path := getattr(result, f"{name}_path", None)) is not None
        ]
        if paths:
            return full_detail + "\n" + "\n".join(paths)
        if all(len(output) <= MAX_FAILURE_STREAM_CHARS for _, output in streams):
            return full_detail

        # Captured output must remain recoverable without rerunning a failed build.
        log_path = self._retain_text(full_detail, prefix="failure-")
        if log_path is None:
            return full_detail

        half = MAX_FAILURE_STREAM_CHARS // 2
        summary = "\n".join(
            f"{name}:\n"
            + (
                output
                if len(output) <= MAX_FAILURE_STREAM_CHARS
                else output[:half]
                + "\n... omitted; see full output ...\n"
                + output[-half:]
            )
            for name, output in streams
        )
        return summary + f"\nFull output: {log_path}"


def parse_nextest_list(
    output: str, *, parsed_payload: dict[str, Any] | None = None
) -> dict[str, bool]:
    try:
        payload = json.loads(output)
    except json.JSONDecodeError as exc:
        raise RunnerError(f"cargo nextest list returned invalid JSON: {exc}") from exc
    root = _require_table(payload, "cargo nextest list output")
    suites = _require_table(
        root.get("rust-suites"), "cargo nextest list output.rust-suites"
    )
    tests: dict[str, bool] = {}
    listed_ids: set[str] = set()
    for suite_name, suite_value in suites.items():
        suite = _require_table(suite_value, f"rust-suites.{suite_name}")
        testcases = _require_table(
            suite.get("testcases"), f"rust-suites.{suite_name}.testcases"
        )
        for test_id, testcase_value in testcases.items():
            test_id = _require_string(test_id, "nextest test ID")
            testcase = _require_table(
                testcase_value, f"rust-suites.{suite_name}.testcases.{test_id}"
            )
            ignored = testcase.get("ignored", False)
            if not isinstance(ignored, bool):
                raise RunnerError(
                    f"nextest ignored state for {test_id!r} must be boolean"
                )
            if test_id in listed_ids:
                raise RunnerError(f"nextest listed duplicate test ID {test_id!r}")
            listed_ids.add(test_id)
            if not _testcase_matches_filter(testcase):
                continue
            tests[test_id] = ignored
    # `test-count` covers every listed case, including the ones a filterset
    # excluded, so compare it against the full listing rather than the selection.
    declared_count = root.get("test-count")
    if isinstance(declared_count, int) and declared_count != len(listed_ids):
        raise RunnerError(
            f"nextest test-count {declared_count} does not match parsed count {len(listed_ids)}"
        )
    if parsed_payload is not None:
        parsed_payload.update(root)
    return tests


def _testcase_matches_filter(testcase: Mapping[str, Any]) -> bool:
    """Nextest lists non-matching cases with a `filter-match` mismatch status."""
    filter_match = testcase.get("filter-match")
    if filter_match is None:
        return True
    if not isinstance(filter_match, dict):
        raise RunnerError("nextest filter-match must be a table")
    return filter_match.get("status") == "matches"


_TARGET_OVERRIDE_OPTIONS = {
    "-p",
    "--package",
    "--workspace",
    "--exclude",
    "--all",
    "--lib",
    "--bin",
    "--bins",
    "--example",
    "--examples",
    "--test",
    "--tests",
    "--bench",
    "--benches",
    "--all-targets",
    "--manifest-path",
    "--target",
    "--target-dir",
}


# Run-only options that `cargo nextest list` rejects, so the selection preview
# has to drop them before it lists the same selection the run will execute.
_RUN_ONLY_OPTIONS = {"--no-fail-fast", "--nff", "--fail-fast", "--ff"}


def _list_only_args(args: Sequence[str]) -> list[str]:
    kept: list[str] = []
    after_separator = False
    for token in args:
        if token == "--":
            after_separator = True
        elif not after_separator and token in _RUN_ONLY_OPTIONS:
            continue
        kept.append(token)
    return kept


def validate_filtering_args(raw_args: Sequence[str]) -> list[str]:
    args = list(raw_args)
    index = 0
    after_separator = False
    while index < len(args):
        token = args[index]
        if token == "--":
            after_separator = True
            index += 1
            continue
        if token == "--no-tests" or token.startswith("--no-tests="):
            raise RunnerError("--no-tests is runner-owned and is forced to fail")
        if not after_separator and (
            token in _TARGET_OVERRIDE_OPTIONS
            or token.startswith(
                (
                    "--package=",
                    "--exclude=",
                    "--test=",
                    "--bin=",
                    "--bench=",
                    "--example=",
                    "--manifest-path=",
                    "--target=",
                    "--target-dir=",
                )
            )
            or (token.startswith("-p") and token != "-p")
        ):
            raise RunnerError(
                f"{token} cannot override a named target; choose another manifest target"
            )
        if after_separator:
            if token in {"--ignored", "--include-ignored", "--exact"}:
                index += 1
                continue
            if token == "--skip":
                if index + 1 >= len(args):
                    raise RunnerError("--skip requires a filter value")
                index += 2
                continue
            if token.startswith("--skip=") or not token.startswith("-"):
                index += 1
                continue
            if token in {"-E", "--filterset"} or token.startswith("--filterset="):
                raise RunnerError(
                    f"unsupported test filtering option {token!r}: put {token.split('=', 1)[0]} before --"
                )
            raise RunnerError(f"unsupported test filtering option {token!r}; {FILTERING_ARGS_HELP}")
        if token in {"-E", "--filterset", "--run-ignored"}:
            if index + 1 >= len(args) or not args[index + 1].strip():
                raise RunnerError(f"{token} requires a value")
            if token == "--run-ignored" and args[index + 1] not in {
                "default",
                "only",
                "all",
            }:
                raise RunnerError("--run-ignored must be default, only, or all")
            index += 2
            continue
        if token.startswith("--filterset="):
            if not token.split("=", 1)[1].strip():
                raise RunnerError("--filterset requires a value")
            index += 1
            continue
        if token.startswith("--run-ignored="):
            if token.split("=", 1)[1] not in {"default", "only", "all"}:
                raise RunnerError("--run-ignored must be default, only, or all")
            index += 1
            continue
        if (
            token == "--ignore-default-filter"
            or token in _RUN_ONLY_OPTIONS
            or not token.startswith("-")
        ):
            index += 1
            continue
        raise RunnerError(f"unsupported test filtering option {token!r}; {FILTERING_ARGS_HELP}")
    if _required_exact_test_ids(args) == []:
        raise RunnerError("exact test selection selects zero tests after name/skip filters", outcome="zero_tests")
    return args


def require_core_lib_filter(
    target: Target | None, args: Sequence[str], *, allow_all: bool
) -> None:
    # The guard belongs to the codex-core library suite, not to one manifest
    # name for it: an alias must not run the whole suite unfiltered.
    if (
        allow_all
        or target is None
        or (target.package, target.selector_kind) != ("codex-core", "lib")
    ):
        return
    index = 0
    while index < len(args):
        token = args[index]
        if token in {"--run-ignored", "--skip"}:
            index += 2
            continue
        if token in {"-E", "--filterset"}:
            if index + 1 < len(args) and args[index + 1].strip():
                return
            index += 2
            continue
        if token.startswith("--filterset=") and token.split("=", 1)[1].strip():
            return
        if token.strip() and not token.startswith("-"):
            return
        index += 1
    raise RunnerError(
        f"{target.name} requires an explicit test filter (-E <filterset> or a test name). "
        "Use a named core-gate for its declared scope, or --all only when the full "
        "library suite is intended."
    )


def guard_generic_recipe_args(
    raw_args: Sequence[str], *, recipe: str | None = None
) -> None:
    args = list(raw_args)
    if "--" in args:
        args = args[: args.index("--")]
    packages = cargo_package_specs(args)
    for spec in packages:
        if not re.fullmatch(r"[A-Za-z0-9_*?\[\]!-]+(?:@[A-Za-z0-9.+-]+)?", spec):
            raise RunnerError(
                f"unsupported package selection {spec!r}; use a package name "
                "or name@version (named core targets for codex-core)"
            )
    if any(token in {"--workspace", "--all"} for token in args) or any(
        fnmatch.fnmatchcase("codex-core", spec.split("@", 1)[0]) for spec in packages
    ):
        owner = f"{recipe} cannot" if recipe else "generic Rust test recipes cannot"
        raise RunnerError(
            f"{owner} select codex-core; the package is owned by named targets. "
            "Use just core-test <target>, just core-test-fast <target>, or "
            "just core-gate <gate>; just core-test-list prints the names."
        )
    if not packages:
        owner = recipe or "generic Rust test recipes"
        raise RunnerError(
            f"{owner} require an explicit -p/--package selection; "
            "the default workspace includes codex-core. Use just core-test "
            "<target> or just core-gate <gate> for core tests."
        )


def crate_validation_commands(
    mode: str, package: str, raw_args: Sequence[str]
) -> list[list[str]]:
    """Preflight the entire ladder before any formatter or Cargo process starts."""
    if mode not in {"focused", "local", "full"}:
        raise RunnerError(f"unknown crate validation mode {mode!r}")
    if not re.fullmatch(r"[A-Za-z0-9_-]+", package):
        raise RunnerError("crate validation requires one exact package name")
    args = list(raw_args)
    separator = args.index("--") if "--" in args else len(args)
    selection, forwarded = args[:separator], args[separator:]
    allow_all = "--all-tests" in selection
    selection = [arg for arg in selection if arg != "--all-tests"]
    args = [*selection, *forwarded]
    # Package selection remains owned by the recipe, never by forwarded flags.
    if cargo_package_specs(selection) or any(
        arg in {"--workspace", "--all"} for arg in selection
    ):
        raise RunnerError("crate validation cannot override its package")
    guard_generic_recipe_args(["-p", package, *args], recipe="crate validation")
    has_target = any(
        arg == "--lib"
        or arg.startswith(("--test=", "--bin="))
        and bool(arg.split("=", 1)[1])
        or arg in {"--test", "--bin"}
        and index + 1 < len(selection)
        and not selection[index + 1].startswith("-")
        for index, arg in enumerate(selection)
    )
    if not allow_all and (
        not has_target
        or any(
            arg in {"--all-targets", "--tests", "--bins", "--examples", "--benches"}
            for arg in selection
        )
    ):
        raise RunnerError(
            "select --lib, --test <target>, or --bin <target>; "
            "use --all-tests only for an intentional whole-package run"
        )
    commands = []
    if mode != "focused":
        commands.append(
            ["just", "fmt-check"]
            if mode == "full"
            else ["just", "fmt-check-fast", "--only", "rust", "--rust-package", package]
        )
    commands.append(["just", "test-fast", "-p", package, "--no-tests=fail", *args])
    return commands


def run_crate_validation(mode: str, package: str, raw_args: Sequence[str]) -> int:
    """Await every read-only check; only the test command owns a Cargo lane."""
    commands = crate_validation_commands(mode, package, raw_args)
    env = dict(os.environ)
    env.update(local_rust_env(env, repo_root=REPO_ROOT))

    def run(command: list[str]) -> subprocess.CompletedProcess:
        # Start a shared cache outside the outer owned job, not inside a nested
        # lane whose ancestor cleanup would otherwise kill that cache server.
        return run_owned(
            command, cwd=CODEX_RS_ROOT, env=env,
            # CREATE_NO_WINDOW needs explicit handles to preserve diagnostics.
            stdout=sys.stdout, stderr=sys.stderr,
            prepare_sccache=command[1] == "test-fast",
        )

    if len(commands) == 1:
        return run(commands[0]).returncode
    # A check failure is a result, not cancellation of its independent sibling.
    # Exceptions/cancellation still stop and reap descendants through the shared
    # process owner. Preserve deterministic recipe-order failure precedence.
    with OwnedThreadPoolExecutor(max_workers=len(commands)) as executor:
        futures = [
            executor.submit(run, command)
            for command in commands
        ]
        codes = [future.result().returncode for future in futures]
    return next((code for code in codes if code), 0)


def load_metadata(
    executor: Executor = _default_executor,
    *,
    cwd: Path = CODEX_RS_ROOT,
    command_timeout_seconds: float | None = None,
) -> MetadataIndex:
    env = dict(os.environ)
    env.update(local_rust_env(env, repo_root=REPO_ROOT))
    if command_timeout_seconds is not None:
        if not math.isfinite(command_timeout_seconds) or command_timeout_seconds <= 0:
            raise RunnerError("command timeout must be a finite positive number")
        env["CODEX_RUST_TEST_TIMEOUT_SECS"] = str(command_timeout_seconds)
    try:
        result = executor(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=cwd,
            env=env,
            capture=CAPTURE_BOTH,
        )
    except CleanupFailed as error:
        # Metadata never builds into a lane, so there is no lane to mark.
        raise RunnerError(str(error), outcome="cleanup_failed") from error
    log_paths = "\n".join(
        f"Full {name}: {path}"
        for name in ("stdout", "stderr")
        if (path := getattr(result, f"{name}_path", None)) is not None
    )
    if result.returncode != 0:
        # Keep both streams: cargo puts warnings on stderr and can put the real
        # error on stdout, so preferring one stream hides the other.
        streams = [
            (name, (getattr(result, name, None) or "").strip())
            for name in ("stdout", "stderr")
        ]
        detail = "\n".join(f"{name}:\n{text}" for name, text in streams if text)
        raise RunnerError(f"cargo metadata --no-deps failed:\n{detail}\n{log_paths}")
    try:
        payload = json.loads(_stdout_text(result))
    except json.JSONDecodeError as exc:
        raise RunnerError(
            f"cargo metadata returned invalid JSON: {exc}\n{log_paths}"
        ) from exc
    return MetadataIndex.from_json(payload)


def _resolve_target_dir(
    value: str | None, metadata: MetadataIndex, *, cwd: Path = CODEX_RS_ROOT
) -> Path:
    # CLI paths belong to the invoking shell, not the Cargo subprocess cwd.
    # Re-rooting `codex-rs/target/...` under CODEX_RS_ROOT silently creates a
    # cold target and bypasses admission for the intended warm directory.
    if value:
        path = Path(value)
        return path if path.is_absolute() else Path.cwd() / path
    # Inherited Cargo environment paths keep Cargo's working-directory rules.
    configured = (
        os.environ.get("CODEX_CARGO_LANE_TARGET_DIR")
        or os.environ.get("CARGO_TARGET_DIR")
    )
    if configured is None:
        return metadata.target_directory
    path = Path(configured)
    return path if path.is_absolute() else cwd / path


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    target_dir_help = "Cargo target directory; relative paths use the caller's working directory."
    parser.add_argument("--target-dir", help=target_dir_help)
    parser.add_argument(
        "--admission-timeout-seconds", type=float, default=0.0,
        help="Opt into waiting for the exact target directory (default 0: report busy immediately). Place before the subcommand.",
    )
    parser.add_argument(
        "--cargo-profile",
        help="Cargo build profile for both tests and their helpers (for example dev-small).",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # Execution policy the calling recipe owns. `--target-dir` uses SUPPRESS so a
    # subcommand that omits it keeps the value parsed before the subcommand.
    run_options = argparse.ArgumentParser(add_help=False)
    run_options.add_argument("--profile")
    run_options.add_argument("--no-fail-fast", action="store_true")
    run_options.add_argument(
        "--command-timeout-seconds",
        type=float,
        help="Deadline for each child command, including process-tree cleanup; unlimited by default.",
    )
    run_options.add_argument(
        "--target-dir", default=argparse.SUPPRESS, help=target_dir_help
    )
    run_options.add_argument(
        "--success-output",
        choices=SUCCESS_OUTPUT_VALUES,
        help="Stream passing-test output (for example benchmark numbers); default never. "
        "Display only: selection, receipts and execution identity are unchanged.",
    )

    subparsers.add_parser("check-manifest")
    subparsers.add_parser("list-targets")
    check_gates = subparsers.add_parser("check-gates")
    check_gates.add_argument("names", nargs="+")
    plan = subparsers.add_parser("plan")
    plan.add_argument("name")
    gates_for = subparsers.add_parser(
        "gates-for",
        help="List gates and declared named-target ownership for these Rust files; no tests run.",
    )
    gates_for.add_argument("paths", nargs="+")
    run_target = subparsers.add_parser("run-target", parents=[run_options])
    run_target.add_argument(
        "--all",
        action="store_true",
        help="Explicitly allow an unfiltered core_lib run.",
    )
    run_target.add_argument("name")
    run_target.add_argument(
        "filter_args", nargs=argparse.REMAINDER, help=FILTERING_ARGS_HELP
    )
    run_gate = subparsers.add_parser("run-gate", parents=[run_options])
    run_gate.add_argument("names", nargs="+")
    guard = subparsers.add_parser(
        "_guard-generic", aliases=["guard-args"], help=argparse.SUPPRESS
    )
    guard.add_argument("--recipe")
    guard.add_argument("guarded_args", nargs=argparse.REMAINDER)
    return parser


# Execution policy the runner owns even when a recipe forwards it positionally.
_RUNNER_OWNED_RUN_OPTIONS = {"--no-fail-fast", "--all"}
# Nextest's display policy for passing tests (for example benchmark numbers).
# Selection, status lines, receipts and execution identity do not change.
SUCCESS_OUTPUT_VALUES = ("never", "immediate", "final", "immediate-final")
FILTERING_ARGS_HELP = (
    "accepted filtering: -E/--filterset EXPR, --run-ignored default|only|all, "
    "--ignore-default-filter, test-name filters; after a further --: names, "
    "--skip PATTERN, --exact, --ignored, --include-ignored. Runner-owned, here or "
    "before the name: --no-fail-fast, --all, --profile NAME, --command-timeout-seconds N, "
    "--success-output never|immediate|final|immediate-final (shows passing-test output)"
)


def _split_runner_owned_options(
    filter_args: Sequence[str],
) -> tuple[list[str], set[str], float | None, str | None, str | None]:
    """Separates runner-owned execution flags from caller filtering args.

    Recipes forward execution flags, the nextest profile, the passing-test
    output policy and the per-command deadline after the target name. Leave
    libtest arguments and filtering-option values intact.
    """
    remaining: list[str] = []
    owned: set[str] = set()
    timeout = None
    profile = None
    success_output = None
    after_separator = False
    tokens = iter(filter_args)
    for token in tokens:
        if token == "--":
            after_separator = True
        if not after_separator and token in _RUNNER_OWNED_RUN_OPTIONS:
            owned.add(token)
            continue
        if not after_separator and (
            token == "--success-output" or token.startswith("--success-output=")
        ):
            value = token.split("=", 1)[1] if "=" in token else next(tokens, "")
            if value not in SUCCESS_OUTPUT_VALUES:
                raise RunnerError(
                    f"--success-output must be one of {', '.join(SUCCESS_OUTPUT_VALUES)}"
                )
            if success_output not in (None, value):
                raise RunnerError(
                    f"conflicting --success-output values {success_output!r} and {value!r}"
                )
            success_output = value
            continue
        if not after_separator and (
            token == "--command-timeout-seconds"
            or token.startswith("--command-timeout-seconds=")
        ):
            value = token.split("=", 1)[1] if "=" in token else next(tokens, "")
            try:
                timeout = float(value)
                if not math.isfinite(timeout) or timeout <= 0:
                    raise ValueError
            except ValueError as exc:
                raise RunnerError(
                    "command timeout must be a finite positive number of seconds"
                ) from exc
            continue
        if not after_separator and (
            token == "--profile" or token.startswith("--profile=")
        ):
            value = token.split("=", 1)[1] if "=" in token else next(tokens, "")
            if not value or value.startswith("-"):
                raise RunnerError("--profile requires a nextest profile name")
            if profile not in (None, value):
                raise RunnerError(
                    f"conflicting --profile values {profile!r} and {value!r}"
                )
            profile = value
            continue
        remaining.append(token)
        if not after_separator and token in {"-E", "--filterset", "--run-ignored"}:
            value = next(tokens, None)
            if value is not None:
                remaining.append(value)
    return remaining, owned, timeout, profile, success_output


def _main(args: argparse.Namespace, metrics: ValidationMetrics | None = None) -> int:
    try:
        if not math.isfinite(args.admission_timeout_seconds) or args.admission_timeout_seconds < 0:
            raise RunnerError("admission timeout must be a finite nonnegative number")
        if args.command in {"_guard-generic", "guard-args"}:
            # This runs on every generic recipe invocation: never read the
            # manifest or shell out to Cargo here.
            guarded_args = list(args.guarded_args)
            if guarded_args[:1] == ["--"]:
                guarded_args = guarded_args[1:]
            guard_generic_recipe_args(guarded_args, recipe=args.recipe)
            return 0

        _validate_cargo_profile(args.cargo_profile)
        filter_args: list[str] = []
        no_fail_fast = getattr(args, "no_fail_fast", False)
        allow_all = getattr(args, "all", False)
        if args.command == "run-target":
            filter_args = list(args.filter_args)
            if filter_args[:1] == ["--"]:
                filter_args = filter_args[1:]
            filter_args, owned, timeout, profile, success_output = (
                _split_runner_owned_options(filter_args)
            )
            if timeout is not None:
                args.command_timeout_seconds = timeout
            if profile is not None:
                if args.profile not in (None, profile):
                    raise RunnerError(
                        f"conflicting --profile values {args.profile!r} and {profile!r}"
                    )
                args.profile = profile
            if success_output is not None:
                if args.success_output not in (None, success_output):
                    raise RunnerError(
                        "conflicting --success-output values "
                        f"{args.success_output!r} and {success_output!r}"
                    )
                args.success_output = success_output
            no_fail_fast = no_fail_fast or "--no-fail-fast" in owned
            allow_all = allow_all or "--all" in owned
            validate_filtering_args(filter_args)

        manifest = Manifest.load(args.manifest)
        # Resolve the complete selection before metadata, provenance, or waiting
        # for a lane. Admission still rechecks the manifest and runner inputs.
        if args.command == "run-target":
            require_core_lib_filter(
                manifest.target(args.name), filter_args, allow_all=allow_all
            )
        elif args.command in {"run-gate", "check-gates"}:
            for name in args.names:
                manifest.gate(name)
        elif args.command == "plan" and args.name not in manifest.targets and args.name not in manifest.gates:
            raise RunnerError(f"unknown named Rust test target or gate {args.name!r}")
        execution_fingerprint = None
        if args.command in {"run-target", "run-gate"}:
            inputs = Path(__file__).read_bytes() + Path(args.manifest).read_bytes()
            execution_fingerprint = hashlib.sha256(inputs).hexdigest()
        if args.command == "list-targets":
            for name in manifest.targets:
                print(f"target\t{name}")
            for name in manifest.gates:
                print(f"gate\t{name}")
            return 0
        timeout = getattr(args, "command_timeout_seconds", None)
        with metrics.phase("preparation") if metrics else nullcontext():
            metadata = (
                load_metadata(command_timeout_seconds=timeout)
                if timeout is not None
                else load_metadata()
            )
        runner = RustTestRunner(
            manifest,
            metadata,
            target_dir=_resolve_target_dir(args.target_dir, metadata),
            profile=getattr(args, "profile", None),
            cargo_profile=args.cargo_profile,
            no_fail_fast=no_fail_fast,
            command_timeout_seconds=getattr(args, "command_timeout_seconds", None),
            success_output=getattr(args, "success_output", None) or "never",
        )
        runner.metrics = metrics
        runner.manifest_path = args.manifest
        # Metadata is already available: reject impossible Cargo selections
        # before waiting for a lane. Keep admission's input rechecks intact.
        if args.command == "run-target":
            runner.target(args.name)
        elif args.command in {"run-gate", "check-gates"}:
            for name in dict.fromkeys(args.names):
                for step in manifest.gate(name).steps:
                    runner.target(step.target)
        if metrics is not None:
            metrics.bind(runner.target_dir)
            metrics.record["proof"]["obligations"] = (
                [args.name] if args.command == "run-target" else list(args.names)
            )
        return _dispatch_with_admission(
            args, runner, metadata, execution_fingerprint, filter_args, allow_all
        )
    except RunnerError as exc:
        if metrics is not None:
            metrics.record["outcome"] = (
                exc.outcome if metrics.record["commands"]
                or exc.outcome in {"busy", "cancelled", "timed_out", "cleanup_failed"} else "blocked"
            )
            partial = exc.completed_tests if isinstance(exc.completed_tests, ExecutionReceipts) else exc.completed_gates
            metrics.record["proof"]["completed_tests"] = (
                partial.completed_tests() if isinstance(partial, ExecutionReceipts) else partial)
            if isinstance(partial, ExecutionReceipts):
                metrics.record["proof"]["executions"] = partial.identities(partial.executed)
                metrics.record["proof"]["required_executions"] = (
                    partial.identities(partial.required) if partial.required is not None else None)
                metrics.record["proof"]["satisfied_gates"] = partial.gates
        print(f"rust_test_runner: {exc}", file=sys.stderr)
        if exc.admission_status is not None:
            if metrics is not None:
                exc.admission_status["invocation"] = [
                    sys.executable, str(Path(__file__).resolve()), *metrics.record["argv"]
                ]
                metrics.record["admission"] = exc.admission_status
            print(json.dumps(exc.admission_status, sort_keys=True), file=sys.stderr)
            return int(exc.admission_status["exit_code"])
        return 2


def main(argv: Sequence[str] | None = None) -> int:
    actual_argv = list(sys.argv[1:] if argv is None else argv)
    args = build_parser().parse_args(actual_argv)
    metrics = (
        ValidationMetrics(actual_argv)
        if args.command in {"run-target", "run-gate", "check-gates"} else None
    )
    outcome = "blocked"
    try:
        result = _main(args, metrics)
        outcome = "passed" if result == 0 else (
            metrics.record["outcome"] if metrics is not None else "failed"
        )
        return result
    except KeyboardInterrupt:
        outcome = "cancelled"
        raise
    except Exception:
        outcome = "failed" if metrics is not None and metrics.record["commands"] else "blocked"
        raise
    finally:
        if metrics is not None:
            metrics.finish(outcome)


def _selected_definitions(
    manifest: Manifest, command: str, names: Sequence[str]
) -> tuple[Any, ...]:
    """Manifest definitions an admitted launch executes: gates, targets, helpers."""
    gates = () if command == "run-target" else tuple(manifest.gates.get(name) for name in names)
    target_names = names if command == "run-target" else [
        step.target for gate in gates if gate is not None for step in gate.steps
    ]
    targets = tuple(manifest.targets.get(name) for name in dict.fromkeys(target_names))
    helpers = tuple(manifest.helpers.get(name) for name in dict.fromkeys(
        helper for target in targets if target is not None for helper in target.all_helpers
    ))
    return manifest.version, gates, targets, helpers


def _dispatch_with_admission(
    args: argparse.Namespace, runner: RustTestRunner, metadata: MetadataIndex,
    execution_fingerprint: str | None, filter_args: Sequence[str], allow_all: bool,
) -> int:
    # Keep imports off the generic guard and planning path. This uses the same
    # lease primitive as the existing lane owner, not a result-sharing service.
    with ExitStack() as cleanup:
        admission = None
        if args.command in {"run-target", "run-gate", "check-gates"}:
            try:
                if __package__:
                    from .rust_build_status import RustAdmissionBusy, reserve_rust_test_target
                else:
                    from rust_build_status import RustAdmissionBusy, reserve_rust_test_target

                with runner.metrics.phase("admission") if runner.metrics else nullcontext():
                    admission = cleanup.enter_context(reserve_rust_test_target(
                        runner.target_dir, timeout_seconds=args.admission_timeout_seconds,
                        cargo_profile=runner.cargo_profile or "test",
                    ))
            except KeyboardInterrupt as exc:
                raise RunnerError("Rust admission cancelled before dispatch", outcome="cancelled") from exc
            except (RustAdmissionBusy, TimeoutError) as exc:
                busy = exc if isinstance(exc, RustAdmissionBusy) else RustAdmissionBusy(
                    str(exc), resource=runner.target_dir, wait_option="--admission-timeout-seconds",
                )
                status = {**busy.status, "selected_targets": (
                    [args.name] if args.command == "run-target" else list(args.names)
                )}
                raise RunnerError(str(busy), outcome="busy", admission_status=status) from exc
            except (OSError, RuntimeError, ValueError) as exc:
                raise RunnerError(f"Rust admission failed: {exc}") from exc
            print(f"Rust admission: wait={admission['wait_seconds']:.3f}s; target={runner.target_dir}", file=sys.stderr)
            # Definitions this launch selects must not silently become obsolete
            # while it waits; fail closed rather than auto-retrying changed work.
            # Unrelated manifest entries and runner edits cannot change it: this
            # process executes the code and definitions it loaded, which the
            # receipt's launch-time runner_input_fingerprint identifies.
            names = [args.name] if args.command == "run-target" else list(args.names)
            if (_selected_definitions(Manifest.load(args.manifest), args.command, names)
                    != _selected_definitions(runner.manifest, args.command, names)):
                raise RunnerError("Rust test manifest changed this launch's selected definitions while waiting for admission; rerun with current inputs")
        with runner.metrics.phase("provenance") if runner.metrics else nullcontext():
            dependencies = (
                execution_dependency_manifest(metadata, Path(args.manifest))
                if args.command in {"run-target", "run-gate"}
                else None
            )
            if runner.metrics is not None and dependencies is not None:
                runner.metrics.dependencies(dependencies)
        if dependencies is not None:
            # Keep bulk provenance out of every model-visible validation result.
            # The full pre-execution observation remains hash-bound and locally
            # recoverable; omitted inputs never authorize narrower freshness or
            # automatic replay. If retention fails, keep all evidence inline.
            rendered = json.dumps(dependencies, sort_keys=True, ensure_ascii=False)
            encoded = rendered.encode("utf-8")
            if len(encoded) > 4096:
                retained = runner._retain_text(rendered, prefix="execution-dependencies-")
                if retained is not None:
                    dependencies = {
                        **dependencies,
                        "file_inputs": [],
                        "source_roots": [],
                        "omitted_file_inputs": len(dependencies["file_inputs"])
                        + dependencies["omitted_file_inputs"],
                        "omitted_source_roots": len(dependencies["source_roots"])
                        + dependencies["omitted_source_roots"],
                        "retained_manifest": {
                            "path": str(retained.resolve()),
                            "bytes": len(encoded),
                            "sha256": hashlib.sha256(encoded).hexdigest(),
                        },
                    }
        if runner.metrics is not None:
            runner.metrics.record["dependency_manifest"] = dependencies
        execution_configuration = {
            "cargo_profile": runner.cargo_profile or "test",
            "nextest_profile": runner.base_env.get("NEXTEST_PROFILE"),
            "target_dir": str(runner.target_dir),
            "platform": runner.platform,
        }
        if args.command == "check-manifest":
            metadata.validate_manifest(runner.manifest)
            print(
                f"validated Rust test manifest version {runner.manifest.version}: {args.manifest}"
            )
        elif args.command == "check-gates":
            runner.check_gates(args.names)
        elif args.command == "plan":
            print(json.dumps(runner.plan(args.name), indent=2))
        elif args.command == "gates-for":
            print(json.dumps(runner.gates_for(args.paths), indent=2))
        elif args.command == "run-target":
            try:
                receipts = runner.run_target(args.name, filter_args, allow_all=allow_all)
            except RunnerError as error:
                if isinstance(error.completed_tests, ExecutionReceipts):
                    emit_execution_receipt(execution_fingerprint, error.completed_tests,
                        [args.name], skipped=None, exit_code=2,
                        selected_packages=[runner.target(args.name).package],
                        dependency_manifest=dependencies,
                        execution_configuration=execution_configuration)
                raise
            emit_execution_receipt(
                execution_fingerprint, receipts, [args.name], skipped=None,
                execution_configuration=execution_configuration,
                selected_packages=[runner.target(args.name).package],
                dependency_manifest=dependencies,
                admission=admission,
                metrics=runner.metrics,
            )
        elif args.command == "run-gate":
            try:
                receipts = runner.run_gates(args.names)
            except RunnerError as error:
                if isinstance(error.completed_gates, ExecutionReceipts):
                    emit_execution_receipt(execution_fingerprint, error.completed_gates,
                        args.names, skipped=0, exit_code=2,
                        selected_packages=sorted({runner.target(step.target).package
                            for name in args.names for step in runner.gate(name).steps}),
                        dependency_manifest=dependencies,
                        execution_configuration=execution_configuration)
                raise
            emit_execution_receipt(
                execution_fingerprint, receipts, args.names, skipped=0,
                execution_configuration=execution_configuration,
                selected_packages=sorted({runner.target(step.target).package
                    for name in args.names for step in runner.gate(name).steps}),
                dependency_manifest=dependencies,
                admission=admission,
                metrics=runner.metrics,
            )
        else:  # pragma: no cover - argparse enforces the command set.
            raise RunnerError(f"unsupported command {args.command!r}")
    return 0


def execution_dependency_manifest(
    metadata: MetadataIndex, manifest: Path,
) -> dict[str, Any]:
    """Record known inputs without claiming arbitrary test effects are hermetic.

    Cargo metadata uses --no-deps; package roots are observations, not a
    transitive dependency proof. Never enable replay from this manifest alone.
    Environment values are hashed as a whole and are never emitted.
    """
    roots = {str(REPO_ROOT.resolve())}
    inputs = {
        Path(__file__).resolve(), manifest.resolve(),
        Path(__file__).with_name("process_owner.py").resolve(),
        Path(__file__).with_name("rust_tool_env.py").resolve(),
        Path(__file__).with_name("rust_build_status.py").resolve(),
        Path(__file__).with_name("rust_build_status_support.py").resolve(),
        Path(__file__).with_name("tool_versions.py").resolve(),
        Path(__file__).with_name("validation_metrics.py").resolve(),
        Path(__file__).with_name("atomic_json.py").resolve(),
        CODEX_RS_ROOT / "Cargo.toml", CODEX_RS_ROOT / "Cargo.lock",
    }
    for package in metadata.packages.values():
        path = package.get("manifest_path")
        if isinstance(path, str):
            path = Path(path).resolve()
            inputs.add(path)
            roots.add(str(path.parent))
    for directory in (CODEX_RS_ROOT, *CODEX_RS_ROOT.parents):
        inputs.update(directory / name for name in (
            ".cargo/config", ".cargo/config.toml",
            "rust-toolchain", "rust-toolchain.toml",
        ))
    records = []
    for path in sorted(inputs, key=str)[:256]:
        record: dict[str, Any] = {"path": str(path)}
        try:
            # Do not let a malformed config turn provenance capture into an
            # unbounded read or discard an otherwise usable execution receipt.
            with path.open("rb") as source:
                content = source.read(1024 * 1024 + 1)
            if len(content) <= 1024 * 1024:
                record["sha256"] = hashlib.sha256(content).hexdigest()
            else:
                record["state"] = "oversized"
        except FileNotFoundError:
            record["state"] = "absent"
        except OSError:
            record["state"] = "unavailable"
        records.append(record)
    context = json.dumps({
        "environment": dict(os.environ),
        "python": str(Path(sys.executable).resolve()),
        "platform": sys.platform,
    }, sort_keys=True, ensure_ascii=True).encode()
    return {
        "version": 1,
        "producer": "rust_test_runner",
        "captured": "before_execution",
        "file_inputs": records,
        "source_roots": sorted(roots)[:256],
        "omitted_file_inputs": max(0, len(inputs) - 256),
        "omitted_source_roots": max(0, len(roots) - 256),
        "execution_context_sha256": hashlib.sha256(context).hexdigest(),
        "coverage": "declared_not_exhaustive",
        "unresolved_dependencies": [
            "transitive_and_generated_inputs", "resolved_toolchain",
            "test_owned_services_network_and_time",
        ],
        "automatic_replay_allowed": False,
    }


def emit_execution_receipt(
    fingerprint: str | None,
    receipts: dict[str, list[str]],
    selected_targets: Sequence[str],
    *,
    skipped: int | None,
    selected_packages: Sequence[str] = (),
    dependency_manifest: dict[str, Any] | None = None,
    admission: dict[str, object] | None = None,
    metrics: ValidationMetrics | None = None,
    exit_code: int = 0,
    execution_configuration: dict[str, Any] | None = None,
) -> None:
    """Publish the runner's completed-test ledger, not a parsed success slogan.

    Source dependency freshness remains the invoking harness's obligation.
    A target run does not count ignored tests; report that count as unknown.
    """
    receipt = {
        "kind": "codex_test_execution_v1",
        "runner": "rust_test_runner",
        "runner_input_fingerprint": fingerprint,
        "selected_targets": list(selected_targets),
        "selected_packages": sorted(set(selected_packages)),
        "workspace_root": str(CODEX_RS_ROOT),
        "completed_tests": receipts.completed_tests() if isinstance(receipts, ExecutionReceipts) else receipts,
        "executed_tests": len(receipts.executed) if isinstance(receipts, ExecutionReceipts)
            else sum(len(set(tests)) for tests in receipts.values()),
        "skipped_tests": skipped,
        "exit_code": exit_code,
    }
    if isinstance(receipts, ExecutionReceipts):
        receipt["executions"] = receipts.identities(receipts.executed)
        receipt["required_executions"] = (
            receipts.identities(receipts.required) if receipts.required is not None else None)
        receipt["satisfied_gates"] = receipts.gates
    if dependency_manifest is not None:
        receipt["dependency_manifest"] = dependency_manifest
    if execution_configuration is not None:
        receipt["execution_configuration"] = execution_configuration
    if admission is not None:
        receipt["admission"] = admission
    if metrics is not None:
        metrics.completed(receipt["completed_tests"], selected_targets)
        metrics.record["proof"]["executions"] = receipt.get("executions")
        metrics.record["proof"]["required_executions"] = receipt.get("required_executions")
        metrics.record["proof"]["satisfied_gates"] = receipt.get("satisfied_gates")
        receipt["validation_run_id"] = metrics.record["run_id"]
    print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    raise SystemExit(main())
