#!/usr/bin/env python3
"""Strict manifest-driven runner for repository-owned Rust test targets."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence


REPO_ROOT = Path(__file__).resolve().parents[1]
CODEX_RS_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_MANIFEST = CODEX_RS_ROOT / ".config" / "kd4-rust-tests.toml"
SCHEMA_VERSION = 1

# Windows test binaries link an 8 MiB main stack and libtest builds its per-test
# worker threads from RUST_MIN_STACK. Keep the runner aligned with the justfile
# and `scripts/rust_build_status.py`.
RUST_MIN_STACK_BYTES = "8388608"
TRUSTED_TEST_FEATURES = frozenset({"codex-core/completion-proof-test-store"})
_QUALIFIED_FEATURE_PATTERN = re.compile(r"^[A-Za-z0-9_-]+/[A-Za-z0-9_-]+$")


class RunnerError(RuntimeError):
    """Raised when a declared test contract cannot be honored."""


@dataclass(frozen=True)
class Helper:
    name: str
    package: str
    binary: str
    platform: str | None
    features: tuple[str, ...] = ()


@dataclass(frozen=True)
class Target:
    name: str
    package: str
    selector_kind: str
    selector_value: str | None
    helpers: tuple[str, ...]
    features: tuple[str, ...] = ()

    def selection_args(self) -> list[str]:
        args = ["-p", self.package]
        if self.selector_kind == "lib":
            args.append("--lib")
        else:
            args.extend([f"--{self.selector_kind}", self.selector_value or ""])
        if self.features:
            args.extend(["--features", ",".join(self.features)])
        return args


@dataclass(frozen=True)
class GateStep:
    target: str
    filterset: str | None
    tests: tuple[str, ...]


@dataclass(frozen=True)
class Gate:
    name: str
    description: str
    steps: tuple[GateStep, ...]


@dataclass(frozen=True)
class NextestTest:
    rust_binary_id: str
    event_binary_alias: str
    semantic_id: str
    ignored: bool

    @property
    def authoritative_id(self) -> str:
        return f"{self.rust_binary_id}${self.semantic_id}"

    @property
    def event_alias(self) -> str:
        return f"{self.event_binary_alias}${self.semantic_id}"


@dataclass(frozen=True)
class NextestTargetIdentity:
    package_name: str
    binary_name: str
    kind: str

    @property
    def rust_binary_id(self) -> str:
        return _nextest_rust_binary_id(
            self.package_name,
            self.kind,
            self.binary_name,
        )

    @property
    def event_binary_alias(self) -> str:
        return f"{self.package_name}::{self.binary_name}"


@dataclass(frozen=True)
class Manifest:
    version: int
    helpers: Mapping[str, Helper]
    targets: Mapping[str, Target]
    gates: Mapping[str, Gate]

    @classmethod
    def load(cls, path: Path) -> "Manifest":
        try:
            with path.open("rb") as manifest_file:
                raw = tomllib.load(manifest_file)
        except (OSError, tomllib.TOMLDecodeError) as exc:
            raise RunnerError(f"cannot read Rust test manifest {path}: {exc}") from exc
        return cls.from_data(raw)

    @classmethod
    def from_data(cls, raw: Any) -> "Manifest":
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
        if not targets_raw:
            raise RunnerError("manifest.targets must be a non-empty table")
        if not gates_raw:
            raise RunnerError("manifest.gates must be a non-empty table")

        helpers: dict[str, Helper] = {}
        for name, value in helpers_raw.items():
            helper_name = _require_name(name, "helper")
            table = _require_table(value, f"helpers.{helper_name}")
            _reject_unknown(
                table,
                {"package", "bin", "platform", "features"},
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
            features = _require_trusted_features(
                table.get("features", []), f"helpers.{helper_name}.features"
            )
            helpers[helper_name] = Helper(
                helper_name, package, binary, platform, features
            )

        targets: dict[str, Target] = {}
        for name, value in targets_raw.items():
            target_name = _require_name(name, "target")
            table = _require_table(value, f"targets.{target_name}")
            _reject_unknown(
                table,
                {"package", "lib", "test", "bin", "helpers", "features"},
                f"targets.{target_name}",
            )
            package = _require_string(
                table.get("package"), f"targets.{target_name}.package"
            )
            selectors = [key for key in ("lib", "test", "bin") if key in table]
            if len(selectors) != 1:
                raise RunnerError(
                    f"targets.{target_name} must declare exactly one of lib, test, or bin"
                )
            selector_kind = selectors[0]
            if selector_kind == "lib":
                if table["lib"] is not True:
                    raise RunnerError(f"targets.{target_name}.lib must be true")
                selector_value = None
            else:
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
            features = _require_trusted_features(
                table.get("features", []), f"targets.{target_name}.features"
            )
            targets[target_name] = Target(
                target_name,
                package,
                selector_kind,
                selector_value,
                tuple(helper_names),
                features,
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
                _reject_unknown(step, {"target", "filter", "tests"}, prefix)
                target_name = _require_string(step.get("target"), f"{prefix}.target")
                if target_name not in targets:
                    raise RunnerError(
                        f"{prefix}.target references unknown target {target_name!r}"
                    )
                filter_value = step.get("filter")
                filterset = None
                if filter_value is not None:
                    filterset = _require_string(filter_value, f"{prefix}.filter")
                tests = _require_string_list(step.get("tests"), f"{prefix}.tests")
                if not tests:
                    raise RunnerError(f"{prefix}.tests must not be empty")
                _reject_duplicates(tests, f"{prefix}.tests")
                steps.append(GateStep(target_name, filterset, tuple(tests)))
            gates[gate_name] = Gate(gate_name, description, tuple(steps))

        return cls(version, helpers, targets, gates)


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


def _require_trusted_features(value: Any, location: str) -> tuple[str, ...]:
    features = _require_string_list(value, location)
    _reject_duplicates(features, location)
    for feature in features:
        if not _QUALIFIED_FEATURE_PATTERN.fullmatch(feature):
            raise RunnerError(
                f"{location} entry {feature!r} must be exactly package/feature"
            )
        if feature not in TRUSTED_TEST_FEATURES:
            raise RunnerError(f"{location} contains untrusted feature {feature!r}")
    return tuple(features)


def _nextest_rust_binary_id(
    package_name: str,
    kind: str,
    binary_name: str,
) -> str:
    """Derive nextest's stable RustBinaryId for the supported target kinds."""
    if kind in {"lib", "proc-macro"}:
        return package_name
    if kind == "test":
        return f"{package_name}::{binary_name}"
    if kind == "bin":
        return f"{package_name}::bin/{binary_name}"
    raise RunnerError(f"unsupported nextest Rust suite kind {kind!r}")


@dataclass(frozen=True)
class MetadataIndex:
    target_directory: Path
    packages: Mapping[str, Mapping[str, Any]]

    @classmethod
    def from_json(cls, raw: Any) -> "MetadataIndex":
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
            package = self._package(helper.package, f"helper {helper.name!r}")
            if not self._has_target(package, helper.binary, "bin"):
                raise RunnerError(
                    f"helper {helper.name!r} declares missing binary "
                    f"{helper.package}/{helper.binary}"
                )
            self._validate_features(helper.features, f"helper {helper.name!r}")
        for target in manifest.targets.values():
            self.nextest_target_identity(target)
            self._validate_features(target.features, f"target {target.name!r}")

    def _validate_features(self, features: Sequence[str], owner: str) -> None:
        for qualified in features:
            package_name, feature_name = qualified.split("/", 1)
            package = self._package(package_name, owner)
            package_features = package.get("features")
            if not isinstance(package_features, dict):
                raise RunnerError(
                    f"cargo metadata features for package {package_name!r} must be a table"
                )
            if feature_name not in package_features:
                raise RunnerError(
                    f"{owner} declares unknown Cargo feature {qualified!r}"
                )

    def package_id(self, package_name: str) -> str:
        package = self._package(package_name, f"package {package_name!r}")
        return _require_string(package.get("id"), f"cargo package {package_name!r}.id")

    def nextest_target_identity(self, target: Target) -> NextestTargetIdentity:
        package = self._package(target.package, f"target {target.name!r}")
        candidates: list[tuple[str, str]] = []
        if target.selector_kind == "lib":
            for cargo_target in self._targets(package):
                kinds = cargo_target.get("kind")
                if not isinstance(kinds, list):
                    continue
                supported = [kind for kind in ("lib", "proc-macro") if kind in kinds]
                if len(supported) > 1:
                    raise RunnerError(
                        f"target {target.name!r} has ambiguous Cargo library kinds"
                    )
                if supported:
                    candidates.append(
                        (
                            _require_string(
                                cargo_target.get("name"),
                                f"Cargo library target for {target.package!r}.name",
                            ),
                            supported[0],
                        )
                    )
            if not candidates:
                raise RunnerError(
                    f"target {target.name!r} declares --lib for package "
                    f"{target.package!r}, which has no library target"
                )
        else:
            for cargo_target in self._targets(package):
                kinds = cargo_target.get("kind")
                if (
                    cargo_target.get("name") == (target.selector_value or "")
                    and isinstance(kinds, list)
                    and target.selector_kind in kinds
                ):
                    candidates.append(
                        (
                            _require_string(
                                cargo_target.get("name"),
                                f"Cargo {target.selector_kind} target for "
                                f"{target.package!r}.name",
                            ),
                            target.selector_kind,
                        )
                    )
            if not candidates:
                raise RunnerError(
                    f"target {target.name!r} declares missing {target.selector_kind} target "
                    f"{target.package}/{target.selector_value}"
                )
        if len(candidates) != 1:
            raise RunnerError(
                f"target {target.name!r} does not resolve to exactly one Cargo target"
            )
        binary_name, kind = candidates[0]
        return NextestTargetIdentity(target.package, binary_name, kind)

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


Executor = Callable[..., subprocess.CompletedProcess[str]]


def _default_executor(
    args: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str],
    capture_output: bool,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        list(args),
        cwd=cwd,
        env=dict(env),
        text=True,
        capture_output=capture_output,
        check=False,
    )


def current_platform() -> str:
    if os.name == "nt":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


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
        no_fail_fast: bool = False,
        env: Mapping[str, str] | None = None,
    ) -> None:
        metadata.validate_manifest(manifest)
        self.manifest = manifest
        self.metadata = metadata
        self.target_dir = (target_dir or metadata.target_directory).resolve()
        self.platform = platform or current_platform()
        self.executor = executor
        self.no_fail_fast = no_fail_fast
        self.base_env = dict(os.environ if env is None else env)
        self.base_env.setdefault("RUST_MIN_STACK", RUST_MIN_STACK_BYTES)
        if profile is not None:
            self.base_env["NEXTEST_PROFILE"] = profile

    def target(self, name: str) -> Target:
        try:
            return self.manifest.targets[name]
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test target {name!r}") from exc

    def gate(self, name: str) -> Gate:
        try:
            return self.manifest.gates[name]
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test gate {name!r}") from exc

    def active_helpers(self, target_names: Iterable[str]) -> list[Helper]:
        selected: list[Helper] = []
        seen: set[str] = set()
        for target_name in target_names:
            for helper_name in self.target(target_name).helpers:
                if helper_name in seen:
                    continue
                helper = self.manifest.helpers[helper_name]
                if helper.platform is not None and helper.platform != self.platform:
                    continue
                selected.append(helper)
                seen.add(helper_name)
        return selected

    def plan(self, name: str) -> dict[str, Any]:
        if name in self.manifest.targets:
            target = self.target(name)
            helpers = self.active_helpers([name])
            return {
                "kind": "target",
                "name": name,
                "target_dir": str(self.target_dir),
                "selection": target.selection_args(),
                "features": list(target.features),
                "helpers": [helper.name for helper in helpers],
                "helper_features": {
                    helper.name: list(helper.features) for helper in helpers
                },
                "list": self._list_command(target, []),
                "builds": [self._build_command(helper) for helper in helpers],
                "run": self._run_command(target, []),
            }
        if name in self.manifest.gates:
            gate = self.gate(name)
            targets = [step.target for step in gate.steps]
            helpers = self.active_helpers(targets)
            steps = []
            for step in gate.steps:
                target = self.target(step.target)
                filter_args = self._gate_filter_args(step)
                steps.append(
                    {
                        "target": step.target,
                        "tests": list(step.tests),
                        "features": list(target.features),
                        "list": self._list_command(target, filter_args),
                        "run": self._run_command(target, filter_args),
                    }
                )
            return {
                "kind": "gate",
                "name": name,
                "target_dir": str(self.target_dir),
                "helpers": [helper.name for helper in helpers],
                "helper_features": {
                    helper.name: list(helper.features) for helper in helpers
                },
                "builds": [self._build_command(helper) for helper in helpers],
                "steps": steps,
            }
        raise RunnerError(f"unknown named Rust test target or gate {name!r}")

    def run_target(
        self,
        name: str,
        filter_args: Sequence[str],
        *,
        no_fail_fast: bool | None = None,
    ) -> None:
        args = validate_filtering_args(filter_args)
        target = self.target(name)
        self._list_tests(target, args)
        env = self._build_helper_environment(self.active_helpers([name]))
        self._checked(
            self._run_command(target, args, no_fail_fast=no_fail_fast),
            env=env,
            capture_output=False,
        )

    def run_gate(self, name: str) -> None:
        gate = self.gate(name)
        for step in gate.steps:
            target = self.target(step.target)
            actual = set(self._list_tests(target, self._gate_filter_args(step)))
            expected = set(step.tests)
            if actual != expected:
                missing = sorted(expected - actual)
                unexpected = sorted(actual - expected)
                details: list[str] = []
                if missing:
                    details.append(f"missing={missing}")
                if unexpected:
                    details.append(f"unexpected={unexpected}")
                raise RunnerError(
                    f"gate {name!r} step {step.target!r} selected the wrong test-ID set: "
                    + ", ".join(details)
                )

        env = self._build_helper_environment(
            self.active_helpers(step.target for step in gate.steps)
        )
        for step in gate.steps:
            target = self.target(step.target)
            self._checked(
                self._run_command(target, self._gate_filter_args(step)),
                env=env,
                capture_output=False,
            )

    def run_gate_proof(self, name: str, execution_id: str) -> tuple[dict[str, Any], int]:
        """Run a named gate and return fresh per-test execution evidence.

        This path is reserved for the canonical completion-proof runner. It first
        confirms every nonzero manifest selection, then runs every gate step with
        nextest's structured libtest event stream. A confirmed test failure is
        retained even if a later step has a runner error.
        """
        if not execution_id.strip():
            raise RunnerError("proof execution ID must be nonempty")
        gate = self.gate(name)
        intended = [test_id for step in gate.steps for test_id in step.tests]
        _reject_duplicates(intended, f"gate {name!r} proof test IDs")
        if not intended:
            raise RunnerError(f"gate {name!r} proof selected zero tests")

        selected: list[str] = []
        selected_steps: list[tuple[GateStep, dict[str, NextestTest]]] = []
        for step in gate.steps:
            target = self.target(step.target)
            listed = self._list_selection(target, self._gate_filter_args(step))
            actual = set(listed)
            expected = set(step.tests)
            if actual != expected:
                missing = sorted(expected - actual)
                unexpected = sorted(actual - expected)
                raise RunnerError(
                    f"gate {name!r} step {step.target!r} selected the wrong test-ID set: "
                    f"missing={missing}, unexpected={unexpected}"
                )
            selected.extend(step.tests)
            selected_steps.append((step, listed))

        env = self._build_helper_environment(
            self.active_helpers(step.target for step in gate.steps)
        )
        env["NEXTEST_EXPERIMENTAL_LIBTEST_JSON"] = "1"
        executed: list[str] = []
        outcomes: list[dict[str, str]] = []
        diagnostics: list[str] = []
        saw_confirmed_failure = False
        saw_pre_result_error = False

        for step, listed in selected_steps:
            target = self.target(step.target)
            command = [
                *self._run_command(
                    target,
                    self._gate_filter_args(step),
                    no_fail_fast=True,
                ),
                "--retries",
                "0",
                "--message-format",
                "libtest-json-plus",
                "--message-format-version",
                "0.1",
            ]
            result = self.executor(
                command,
                cwd=CODEX_RS_ROOT,
                env=env,
                capture_output=True,
            )
            started, terminal, parse_errors = parse_nextest_events(
                result.stdout or ""
            )
            expected_semantic = set(step.tests)
            event_by_semantic: dict[str, str] = {}
            authoritative_by_event: dict[str, str] = {}
            semantic_by_authoritative: dict[str, str] = {}
            for semantic_id in step.tests:
                listed_test = listed[semantic_id]
                event_alias = listed_test.event_alias
                authoritative_id = listed_test.authoritative_id
                previous_authoritative = authoritative_by_event.get(event_alias)
                if (
                    previous_authoritative is not None
                    and previous_authoritative != authoritative_id
                ):
                    raise RunnerError(
                        f"nextest event alias {event_alias!r} is ambiguous between "
                        f"{previous_authoritative!r} and {authoritative_id!r}"
                    )
                previous_semantic = semantic_by_authoritative.get(authoritative_id)
                if previous_semantic is not None and previous_semantic != semantic_id:
                    raise RunnerError(
                        f"nextest authoritative ID {authoritative_id!r} maps to "
                        "more than one manifest test ID"
                    )
                event_by_semantic[semantic_id] = event_alias
                authoritative_by_event[event_alias] = authoritative_id
                semantic_by_authoritative[authoritative_id] = semantic_id
            expected_events = set(authoritative_by_event)
            observed_starts = started & expected_events
            observed_terminals = {
                event_alias
                for event_alias, outcome in terminal.items()
                if event_alias in expected_events
                and outcome in {"passed", "failed"}
            }
            unexpected_terminal_ids = sorted(
                event_alias
                for event_alias, outcome in terminal.items()
                if event_alias not in expected_events
                and outcome in {"passed", "failed"}
            )
            cross_binary_lookalikes = sorted(
                event_alias
                for event_alias, outcome in terminal.items()
                if event_alias not in expected_events
                and outcome in {"passed", "failed"}
                and _split_qualified_nextest_id(event_alias)[1]
                in expected_semantic
            )
            step_outcomes: list[dict[str, str]] = []
            for semantic_id in step.tests:
                event_alias = event_by_semantic[semantic_id]
                authoritative_id = authoritative_by_event[event_alias]
                mapped_semantic = semantic_by_authoritative[authoritative_id]
                outcome = terminal.get(event_alias)
                if outcome in {"passed", "failed"}:
                    executed.append(mapped_semantic)
                    item = {"id": mapped_semantic, "outcome": outcome}
                    outcomes.append(item)
                    step_outcomes.append(item)
            failed = [item for item in step_outcomes if item["outcome"] == "failed"]
            invalid_outcomes = [
                semantic_id
                for semantic_id, event_alias in event_by_semantic.items()
                if event_alias in terminal
                and terminal[event_alias] not in {"passed", "failed"}
            ]
            confirmed_failure = bool(failed) and result.returncode != 0
            step_passed = (
                result.returncode == 0
                and not parse_errors
                and not invalid_outcomes
                and observed_starts == expected_events
                and observed_terminals == expected_events
                and not unexpected_terminal_ids
                and not cross_binary_lookalikes
                and not failed
            )
            if confirmed_failure:
                saw_confirmed_failure = True
            if not step_passed and not confirmed_failure:
                saw_pre_result_error = True
            elif confirmed_failure and (
                parse_errors
                or invalid_outcomes
                or observed_starts != expected_events
                or observed_terminals != expected_events
                or unexpected_terminal_ids
                or cross_binary_lookalikes
            ):
                # Preserve the confirmed failure while retaining that the rest
                # of the step was not clean evidence.
                saw_pre_result_error = True
            if parse_errors:
                diagnostics.extend(parse_errors)
            if observed_starts != expected_events:
                diagnostics.append(
                    f"step {step.target} start mismatch: "
                    f"missing={sorted(expected_events - observed_starts)}"
                )
            if (
                observed_terminals != expected_events
                or unexpected_terminal_ids
                or cross_binary_lookalikes
            ):
                diagnostics.append(
                    f"step {step.target} terminal mismatch: "
                    f"missing={sorted(expected_events - observed_terminals)}, "
                    f"unexpected={unexpected_terminal_ids}, "
                    f"cross_binary_lookalikes={cross_binary_lookalikes}"
                )
            if invalid_outcomes:
                diagnostics.append(
                    f"step {step.target} invalid terminal outcomes: "
                    f"{sorted(invalid_outcomes)}"
                )
            if failed and result.returncode == 0:
                diagnostics.append(
                    f"step {step.target} emitted a failed terminal with exit 0"
                )
            if result.returncode != 0:
                diagnostics.append(
                    f"step {step.target} exited {result.returncode}: "
                    f"{(result.stderr or '')[-2000:]}"
                )

        if saw_confirmed_failure:
            classification = "confirmed_validation_failure"
            exit_code = 100
        elif saw_pre_result_error:
            classification = "pre_result_error"
            exit_code = 2
        else:
            classification = "confirmed_pass"
            exit_code = 0
        return (
            {
                "schema_version": 1,
                "report_type": "RustNamedGateExecutionReportV1",
                "gate": name,
                "execution_id": execution_id,
                "classification": classification,
                "intended_ids": intended,
                "selected_ids": selected,
                "executed_ids": executed,
                "outcomes": outcomes,
                "diagnostic": "\n".join(diagnostics)[-8000:],
            },
            exit_code,
        )

    def parity(self, legacy_name: str, replacement_names: Sequence[str]) -> None:
        if not replacement_names:
            raise RunnerError("parity requires at least one replacement target")
        legacy = self.target(legacy_name)
        replacements = [self.target(name) for name in replacement_names]
        if legacy_name in replacement_names:
            raise RunnerError("legacy target cannot also be a replacement target")
        _reject_duplicates(list(replacement_names), "parity replacement targets")

        parity_list_args = ["--ignore-default-filter", "--run-ignored", "all"]
        legacy_tests = self._list_tests(legacy, parity_list_args)
        replacement_tests: dict[str, bool] = {}
        duplicates: list[str] = []
        for replacement in replacements:
            for test_id, ignored in self._list_tests(
                replacement, parity_list_args
            ).items():
                if test_id in replacement_tests:
                    duplicates.append(test_id)
                else:
                    replacement_tests[test_id] = ignored
        if duplicates:
            raise RunnerError(
                "replacement targets contain duplicate canonical test IDs: "
                + ", ".join(sorted(set(duplicates)))
            )

        legacy_ids = set(legacy_tests)
        replacement_ids = set(replacement_tests)
        missing = sorted(legacy_ids - replacement_ids)
        added = sorted(replacement_ids - legacy_ids)
        ignored_changes = sorted(
            test_id
            for test_id in legacy_ids & replacement_ids
            if legacy_tests[test_id] != replacement_tests[test_id]
        )
        if missing or added or ignored_changes:
            raise RunnerError(
                "legacy/replacement parity mismatch: "
                f"missing={missing}, additions={added}, ignored_state_changes={ignored_changes}"
            )

        all_names = [legacy_name, *replacement_names]
        env = self._build_helper_environment(self.active_helpers(all_names))
        legacy_env = dict(env)
        legacy_env["INSTA_UPDATE"] = "always"
        replacement_env = dict(env)
        replacement_env["INSTA_UPDATE"] = "always"
        behavior_args = [
            "--ignore-default-filter",
            "--no-fail-fast",
            "--retries",
            "0",
            "--run-ignored",
            "default",
        ]
        behavior_runs = [(legacy, legacy_env), *(
            (replacement, replacement_env) for replacement in replacements
        )]
        failed_runs: list[str] = []
        for target, run_env in behavior_runs:
            command = self._run_command(target, behavior_args, internal_args=True)
            result = self.executor(
                command,
                cwd=CODEX_RS_ROOT,
                env=run_env,
                capture_output=False,
            )
            if result.returncode != 0:
                detail = (result.stderr or result.stdout or "").strip()
                rendered = subprocess.list2cmdline(command)
                failed_runs.append(
                    f"{target.name}: {rendered}" + (f"\n{detail}" if detail else "")
                )
        if failed_runs:
            raise RunnerError(
                "parity behavior runs failed after executing every target:\n"
                + "\n".join(failed_runs)
            )
        self._assert_snapshot_parity(legacy, replacements)

    @staticmethod
    def _assert_snapshot_parity(legacy: Target, replacements: Sequence[Target]) -> None:
        if legacy.package != "codex-core" or legacy.selector_kind != "test":
            return

        snapshots_dir = CODEX_RS_ROOT / "core" / "tests" / "suite" / "snapshots"
        legacy_prefix = f"{legacy.selector_value}__"
        legacy_snapshots = {
            path.name.removeprefix(legacy_prefix): path
            for path in snapshots_dir.glob(f"{legacy_prefix}*.snap")
        }
        if not legacy_snapshots:
            return

        replacements_by_suffix: dict[str, list[Path]] = {}
        for replacement in replacements:
            if replacement.package != legacy.package or replacement.selector_kind != "test":
                continue
            replacement_prefix = f"{replacement.selector_value}__"
            for path in snapshots_dir.glob(f"{replacement_prefix}*.snap"):
                suffix = path.name.removeprefix(replacement_prefix)
                replacements_by_suffix.setdefault(suffix, []).append(path)

        missing = sorted(set(legacy_snapshots) - set(replacements_by_suffix))
        additions = sorted(set(replacements_by_suffix) - set(legacy_snapshots))
        duplicates = sorted(
            suffix
            for suffix, paths in replacements_by_suffix.items()
            if len(paths) > 1
        )
        mismatched = sorted(
            suffix
            for suffix, legacy_path in legacy_snapshots.items()
            if len(replacements_by_suffix.get(suffix, [])) == 1
            and legacy_path.read_bytes()
            != replacements_by_suffix[suffix][0].read_bytes()
        )
        if missing or additions or duplicates or mismatched:
            raise RunnerError(
                "legacy/replacement snapshot parity mismatch: "
                f"missing={missing}, additions={additions}, "
                f"duplicates={duplicates}, content_changes={mismatched}"
            )

    def _gate_filter_args(self, step: GateStep) -> list[str]:
        return ["-E", step.filterset] if step.filterset is not None else []

    def _selection_command(self, verb: str, target: Target) -> list[str]:
        return [
            "cargo",
            "nextest",
            verb,
            "--target-dir",
            str(self.target_dir),
            *target.selection_args(),
        ]

    def _list_command(self, target: Target, filter_args: Sequence[str]) -> list[str]:
        return [*self._selection_command("list", target), "-T", "json", *filter_args]

    def _run_command(
        self,
        target: Target,
        filter_args: Sequence[str],
        *,
        internal_args: bool = False,
        no_fail_fast: bool | None = None,
    ) -> list[str]:
        args = (
            list(filter_args) if internal_args else validate_filtering_args(filter_args)
        )
        keep_going = self.no_fail_fast if no_fail_fast is None else no_fail_fast
        return [
            *self._selection_command("run", target),
            "--no-tests=fail",
            *(["--no-fail-fast"] if keep_going else []),
            *args,
        ]

    def _build_command(self, helper: Helper) -> list[str]:
        return [
            "cargo",
            "build",
            "--message-format=json-render-diagnostics",
            "--target-dir",
            str(self.target_dir),
            "-p",
            helper.package,
            "--bin",
            helper.binary,
            *(
                ["--features", ",".join(helper.features)]
                if helper.features
                else []
            ),
        ]

    def _list_tests(
        self, target: Target, filter_args: Sequence[str]
    ) -> dict[str, bool]:
        return {
            test_id: test.ignored
            for test_id, test in self._list_selection(target, filter_args).items()
        }

    def _list_selection(
        self, target: Target, filter_args: Sequence[str]
    ) -> dict[str, NextestTest]:
        args = _list_only_args(validate_filtering_args(filter_args))
        result = self._checked(
            self._list_command(target, args), env=self.base_env, capture_output=True
        )
        tests = parse_nextest_list(
            result.stdout,
            expected_target=self.metadata.nextest_target_identity(target),
        )
        if not tests:
            raise RunnerError(
                f"named target {target.name!r} selected zero tests with args {args!r}"
            )
        return tests

    def _build_helper_environment(self, helpers: Sequence[Helper]) -> dict[str, str]:
        env = dict(self.base_env)
        for helper in helpers:
            result = self._checked(
                self._build_command(helper), env=env, capture_output=True
            )
            executable = self._helper_artifact(helper, result.stdout)
            dashed = f"CARGO_BIN_EXE_{helper.binary}"
            underscored = f"CARGO_BIN_EXE_{helper.binary.replace('-', '_')}"
            env[dashed] = str(executable)
            env[underscored] = str(executable)
        return env

    def _helper_artifact(self, helper: Helper, output: str) -> Path:
        expected_package_id = self.metadata.package_id(helper.package)
        executables: list[Path] = []
        for line in output.splitlines():
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
            if (
                message.get("package_id") == expected_package_id
                and target.get("name") == helper.binary
                and isinstance(target.get("kind"), list)
                and "bin" in target["kind"]
                and isinstance(message.get("executable"), str)
            ):
                executables.append(Path(message["executable"]))
        if len(executables) != 1 or not executables[0].is_file():
            raise RunnerError(
                f"helper build did not produce exactly one executable artifact for "
                f"{helper.package}/{helper.binary}"
            )
        return executables[0].resolve()

    def _checked(
        self,
        args: Sequence[str],
        *,
        env: Mapping[str, str],
        capture_output: bool,
    ) -> subprocess.CompletedProcess[str]:
        result = self.executor(
            list(args),
            cwd=CODEX_RS_ROOT,
            env=env,
            capture_output=capture_output,
        )
        if result.returncode != 0:
            detail = (result.stderr or result.stdout or "").strip()
            rendered = subprocess.list2cmdline(list(args))
            if detail:
                raise RunnerError(f"command failed ({rendered}):\n{detail}")
            raise RunnerError(f"command failed ({rendered})")
        return result


def parse_nextest_list(
    output: str,
    *,
    expected_target: NextestTargetIdentity,
) -> dict[str, NextestTest]:
    try:
        payload = json.loads(output)
    except json.JSONDecodeError as exc:
        raise RunnerError(f"cargo nextest list returned invalid JSON: {exc}") from exc
    root = _require_table(payload, "cargo nextest list output")
    if "test-count" not in root:
        raise RunnerError("cargo nextest list output.test-count is required")
    declared_count = root["test-count"]
    if type(declared_count) is not int:
        raise RunnerError("cargo nextest list output.test-count must be an integer")
    if declared_count < 0:
        raise RunnerError(
            "cargo nextest list output.test-count must be nonnegative"
        )
    suites = _require_table(
        root.get("rust-suites"), "cargo nextest list output.rust-suites"
    )
    tests: dict[str, NextestTest] = {}
    listed_ids: set[str] = set()
    binary_alias_owners: dict[str, str] = {}
    event_alias_owners: dict[str, str] = {}
    found_targets: list[tuple[NextestTargetIdentity, str]] = []
    listed_count = 0
    for suite_name, suite_value in suites.items():
        suite_name = _require_string(suite_name, "nextest rust-suite ID")
        if "$" in suite_name:
            raise RunnerError(
                f"nextest rust-suite ID {suite_name!r} must not contain '$'"
            )
        suite_location = f"rust-suites.{suite_name}"
        suite = _require_table(suite_value, suite_location)
        binary_id = _require_string(
            suite.get("binary-id"), f"{suite_location}.binary-id"
        )
        package_name = _require_string(
            suite.get("package-name"), f"{suite_location}.package-name"
        )
        binary_name = _require_string(
            suite.get("binary-name"), f"{suite_location}.binary-name"
        )
        kind = _require_string(suite.get("kind"), f"{suite_location}.kind")
        status = _require_string(suite.get("status"), f"{suite_location}.status")
        for label, value in (
            ("binary-id", binary_id),
            ("package-name", package_name),
            ("binary-name", binary_name),
        ):
            if "$" in value:
                raise RunnerError(
                    f"{suite_location}.{label} {value!r} must not contain '$'"
                )
        if suite_name != binary_id:
            raise RunnerError(
                f"nextest rust-suite map key {suite_name!r} does not match "
                f"binary-id {binary_id!r}"
            )
        if status != "listed":
            raise RunnerError(
                f"{suite_location}.status must be 'listed', found {status!r}"
            )
        derived_binary_id = _nextest_rust_binary_id(
            package_name,
            kind,
            binary_name,
        )
        if binary_id != derived_binary_id:
            raise RunnerError(
                f"{suite_location}.binary-id {binary_id!r} does not match "
                f"the stable {kind} identity {derived_binary_id!r}"
            )
        event_binary_alias = f"{package_name}::{binary_name}"
        previous_owner = binary_alias_owners.get(event_binary_alias)
        if previous_owner is not None and previous_owner != binary_id:
            raise RunnerError(
                f"nextest event binary alias {event_binary_alias!r} is ambiguous "
                f"between {previous_owner!r} and {binary_id!r}"
            )
        binary_alias_owners[event_binary_alias] = binary_id
        actual_target = NextestTargetIdentity(package_name, binary_name, kind)
        found_targets.append((actual_target, binary_id))
        testcases = _require_table(
            suite.get("testcases"), f"{suite_location}.testcases"
        )
        for test_id, testcase_value in testcases.items():
            listed_count += 1
            test_id = _require_string(test_id, "nextest test ID")
            if "$" in test_id:
                raise RunnerError(
                    f"nextest test ID {test_id!r} must not contain '$'"
                )
            _reject_nextest_attempt_identity(test_id, owner="nextest test ID")
            testcase = _require_table(
                testcase_value, f"rust-suites.{suite_name}.testcases.{test_id}"
            )
            if "ignored" not in testcase:
                raise RunnerError(
                    f"nextest ignored state for {test_id!r} is required"
                )
            ignored = testcase["ignored"]
            if type(ignored) is not bool:
                raise RunnerError(
                    f"nextest ignored state for {test_id!r} must be boolean"
                )
            if test_id in listed_ids:
                raise RunnerError(f"nextest listed duplicate test ID {test_id!r}")
            listed_ids.add(test_id)
            if not _testcase_matches_filter(testcase):
                continue
            test = NextestTest(
                rust_binary_id=binary_id,
                event_binary_alias=event_binary_alias,
                semantic_id=test_id,
                ignored=ignored,
            )
            prior_authoritative = event_alias_owners.get(test.event_alias)
            if (
                prior_authoritative is not None
                and prior_authoritative != test.authoritative_id
            ):
                raise RunnerError(
                    f"nextest event alias {test.event_alias!r} is ambiguous between "
                    f"{prior_authoritative!r} and {test.authoritative_id!r}"
                )
            event_alias_owners[test.event_alias] = test.authoritative_id
            tests[test_id] = test
    # `test-count` covers every listed case, including the ones a filterset
    # excluded, so compare it against the full listing rather than the selection.
    if declared_count != listed_count:
        raise RunnerError(
            f"nextest test-count {declared_count} does not match parsed count {listed_count}"
        )
    mismatched_targets = [
        (target, binary_id)
        for target, binary_id in found_targets
        if target != expected_target
    ]
    if mismatched_targets:
        target, binary_id = mismatched_targets[0]
        raise RunnerError(
            "nextest rust-suite does not match the requested Cargo target: "
            f"expected package={expected_target.package_name!r}, "
            f"binary={expected_target.binary_name!r}, kind={expected_target.kind!r}, "
            f"binary-id={expected_target.rust_binary_id!r}; "
            f"found package={target.package_name!r}, "
            f"binary={target.binary_name!r}, kind={target.kind!r}, "
            f"binary-id={binary_id!r}"
        )
    return tests


def _reject_nextest_attempt_identity(value: str, *, owner: str) -> None:
    retry_prefix, retry_separator, retry_index = value.rpartition("#")
    if retry_separator and retry_prefix and retry_index.isdecimal():
        raise RunnerError(f"{owner} {value!r} contains a retry-attempt suffix")
    stress_prefix, stress_separator, stress_index = value.rpartition("@stress-")
    if stress_separator and stress_prefix and stress_index.isdecimal():
        raise RunnerError(f"{owner} {value!r} contains a stress-attempt suffix")


def _split_qualified_nextest_id(value: str) -> tuple[str, str]:
    if value.count("$") != 1:
        raise RunnerError(
            f"nextest test event name {value!r} must be exactly "
            "<package-name>::<binary-name>$<semantic-test-id>"
        )
    event_binary_alias, semantic_id = value.split("$", 1)
    if event_binary_alias.count("::") != 1 or not semantic_id:
        raise RunnerError(
            f"nextest test event name {value!r} must be exactly "
            "<package-name>::<binary-name>$<semantic-test-id>"
        )
    package_name, binary_name = event_binary_alias.split("::", 1)
    if not package_name.strip() or not binary_name.strip():
        raise RunnerError(
            f"nextest test event name {value!r} must be exactly "
            "<package-name>::<binary-name>$<semantic-test-id>"
        )
    _reject_nextest_attempt_identity(
        semantic_id,
        owner="nextest test event semantic ID",
    )
    return event_binary_alias, semantic_id


def parse_nextest_events(
    output: str,
) -> tuple[set[str], dict[str, str], list[str]]:
    """Parse nextest's libtest-json-plus stream without inferring execution."""
    started: set[str] = set()
    terminal: dict[str, str] = {}
    errors: list[str] = []
    for line in output.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            errors.append(f"invalid nextest event JSON: {line[:500]}")
            continue
        if not isinstance(event, dict) or event.get("type") != "test":
            continue
        test_id = event.get("name")
        event_name = event.get("event")
        if not isinstance(test_id, str) or not test_id:
            errors.append("nextest test event omitted its name")
            continue
        try:
            _split_qualified_nextest_id(test_id)
        except RunnerError as exc:
            errors.append(str(exc))
            continue
        if event_name == "started":
            if test_id in started:
                errors.append(f"nextest emitted duplicate start for {test_id}")
                continue
            started.add(test_id)
        elif event_name in {"ok", "failed", "ignored"}:
            if test_id not in started:
                errors.append(
                    f"nextest emitted a terminal result before its start for {test_id}"
                )
                continue
            if event_name == "ok":
                outcome = "passed"
            elif event_name == "failed":
                outcome = "failed"
            else:
                outcome = "skipped"
            if test_id in terminal:
                errors.append(f"nextest emitted duplicate terminal result for {test_id}")
                continue
            terminal[test_id] = outcome
        else:
            errors.append(
                f"nextest emitted unsupported test event {event_name!r} for {test_id}"
            )
    return started, terminal, errors


def _testcase_matches_filter(testcase: Mapping[str, Any]) -> bool:
    """Nextest lists non-matching cases with a `filter-match` mismatch status."""
    if "filter-match" not in testcase:
        raise RunnerError("nextest filter-match is required")
    filter_match = testcase["filter-match"]
    if not isinstance(filter_match, dict):
        raise RunnerError("nextest filter-match must be a table")
    status = _require_string(filter_match.get("status"), "nextest filter-match.status")
    if status not in {"matches", "mismatch"}:
        raise RunnerError(
            f"nextest filter-match.status has unsupported value {status!r}"
        )
    return status == "matches"


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
            or token.startswith("--package=")
            or token.startswith("--exclude=")
            or token.startswith("--test=")
            or token.startswith("--bin=")
            or token.startswith("--bench=")
            or token.startswith("--example=")
            or token.startswith("--manifest-path=")
            or token.startswith("--target=")
            or token.startswith("--target-dir=")
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
            raise RunnerError(f"unsupported test filtering option {token!r}")
        if token in {"-E", "--filterset", "--run-ignored"}:
            if index + 1 >= len(args):
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
            if token == "--filterset=":
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
        raise RunnerError(f"unsupported test filtering option {token!r}")
    return args


def guard_generic_recipe_args(
    raw_args: Sequence[str], *, recipe: str | None = None
) -> None:
    args = list(raw_args)
    index = 0
    while index < len(args):
        token = args[index]
        if token == "--":
            return
        package_spec: str | None = None
        if token in {"-p", "--package"}:
            if index + 1 < len(args):
                package_spec = args[index + 1]
                index += 1
        elif token.startswith("--package="):
            package_spec = token.split("=", 1)[1]
        elif token.startswith("-p") and token != "-p":
            package_spec = token[2:]
        if package_spec == "codex-core" or (
            package_spec is not None and package_spec.startswith("codex-core@")
        ):
            owner = f"{recipe} cannot" if recipe else "generic Rust test recipes cannot"
            raise RunnerError(
                f"{owner} select codex-core; the package is owned by named targets. "
                "Use just core-test <target>, just core-test-fast <target>, or "
                "just core-gate <gate>; just core-test-list prints the names."
            )
        index += 1


def load_metadata(executor: Executor = _default_executor) -> MetadataIndex:
    result = executor(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=CODEX_RS_ROOT,
        env=os.environ,
        capture_output=True,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "").strip()
        raise RunnerError(f"cargo metadata --no-deps failed:\n{detail}")
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RunnerError(f"cargo metadata returned invalid JSON: {exc}") from exc
    return MetadataIndex.from_json(payload)


def _configured_target_dir(value: str | None) -> str | None:
    return (
        value
        or os.environ.get("CODEX_CARGO_LANE_TARGET_DIR")
        or os.environ.get("CARGO_TARGET_DIR")
    )


def _validate_explicit_target_dir(value: str | None) -> None:
    configured = _configured_target_dir(value)
    if configured is None:
        return
    path = Path(configured)
    if path.is_absolute():
        return
    normalized = Path(os.path.normpath(configured))
    if normalized.parts and normalized.parts[0].casefold() == "codex-rs":
        raise RunnerError(
            "relative --target-dir paths are anchored under codex-rs and must not "
            "start with codex-rs; use an absolute path or a workspace-relative "
            "target-* path"
        )


def _resolve_target_dir(value: str | None, metadata: MetadataIndex) -> Path:
    configured = _configured_target_dir(value)
    if configured is None:
        return metadata.target_directory
    path = Path(configured)
    return path if path.is_absolute() else CODEX_RS_ROOT / path


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--target-dir")
    subparsers = parser.add_subparsers(dest="command", required=True)

    # Execution policy the calling recipe owns. `--target-dir` uses SUPPRESS so a
    # subcommand that omits it keeps the value parsed before the subcommand.
    run_options = argparse.ArgumentParser(add_help=False)
    run_options.add_argument("--profile")
    run_options.add_argument("--no-fail-fast", action="store_true")
    run_options.add_argument("--target-dir", default=argparse.SUPPRESS)

    subparsers.add_parser("check-manifest")
    subparsers.add_parser("list-targets")
    plan = subparsers.add_parser("plan")
    plan.add_argument("name")
    run_target = subparsers.add_parser("run-target", parents=[run_options])
    run_target.add_argument("name")
    run_target.add_argument("filter_args", nargs=argparse.REMAINDER)
    run_gate = subparsers.add_parser("run-gate", parents=[run_options])
    run_gate.add_argument("name")
    proof_gate = subparsers.add_parser("run-gate-proof", parents=[run_options])
    proof_gate.add_argument("name")
    proof_gate.add_argument("--proof-report", type=Path, required=True)
    proof_gate.add_argument("--proof-execution-id", required=True)
    parity = subparsers.add_parser("parity", parents=[run_options])
    parity.add_argument("legacy_target")
    parity.add_argument("replacement_targets", nargs="+")
    guard = subparsers.add_parser(
        "_guard-generic", aliases=["guard-args"], help=argparse.SUPPRESS
    )
    guard.add_argument("--recipe")
    guard.add_argument("guarded_args", nargs=argparse.REMAINDER)
    return parser


# Execution policy the runner owns even when a recipe forwards it positionally.
_RUNNER_OWNED_RUN_OPTIONS = {"--no-fail-fast"}


def _split_runner_owned_options(
    filter_args: Sequence[str],
) -> tuple[list[str], set[str]]:
    """Separates runner-owned execution flags from caller filtering args.

    Recipes may pass `--no-fail-fast` after the target name; the runner assembles
    the nextest command, so it consumes the flag instead of forwarding it.
    """
    remaining: list[str] = []
    owned: set[str] = set()
    after_separator = False
    for token in filter_args:
        if token == "--":
            after_separator = True
        if not after_separator and token in _RUNNER_OWNED_RUN_OPTIONS:
            owned.add(token)
            continue
        remaining.append(token)
    return remaining, owned


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command in {"_guard-generic", "guard-args"}:
            # This runs on every generic recipe invocation: never read the
            # manifest or shell out to Cargo here.
            guarded_args = list(args.guarded_args)
            if guarded_args[:1] == ["--"]:
                guarded_args = guarded_args[1:]
            guard_generic_recipe_args(guarded_args, recipe=args.recipe)
            return 0

        filter_args: list[str] = []
        no_fail_fast = getattr(args, "no_fail_fast", False)
        if args.command == "run-target":
            filter_args = list(args.filter_args)
            if filter_args[:1] == ["--"]:
                filter_args = filter_args[1:]
            filter_args, owned = _split_runner_owned_options(filter_args)
            no_fail_fast = no_fail_fast or "--no-fail-fast" in owned

        _validate_explicit_target_dir(args.target_dir)
        manifest = Manifest.load(args.manifest)
        metadata = load_metadata()
        runner = RustTestRunner(
            manifest,
            metadata,
            target_dir=_resolve_target_dir(args.target_dir, metadata),
            profile=getattr(args, "profile", None),
            no_fail_fast=no_fail_fast,
        )
        if args.command == "check-manifest":
            print(
                f"validated Rust test manifest version {manifest.version}: {args.manifest}"
            )
        elif args.command == "list-targets":
            for name, target in manifest.targets.items():
                print(f"target\t{name}\tfeatures={','.join(target.features)}")
            for name in manifest.gates:
                gate_features = sorted(
                    {
                        feature
                        for step in manifest.gates[name].steps
                        for feature in manifest.targets[step.target].features
                    }
                )
                print(f"gate\t{name}\tfeatures={','.join(gate_features)}")
        elif args.command == "plan":
            print(json.dumps(runner.plan(args.name), indent=2))
        elif args.command == "run-target":
            runner.run_target(args.name, filter_args)
        elif args.command == "run-gate":
            runner.run_gate(args.name)
        elif args.command == "run-gate-proof":
            report, exit_code = runner.run_gate_proof(
                args.name,
                args.proof_execution_id,
            )
            args.proof_report.parent.mkdir(parents=True, exist_ok=True)
            try:
                with args.proof_report.open("x", encoding="utf-8", newline="\n") as output:
                    json.dump(report, output, indent=2, sort_keys=True)
                    output.write("\n")
            except FileExistsError as exc:
                raise RunnerError(
                    f"proof report already exists: {args.proof_report}"
                ) from exc
            return exit_code
        elif args.command == "parity":
            runner.parity(args.legacy_target, args.replacement_targets)
        else:  # pragma: no cover - argparse enforces the command set.
            raise RunnerError(f"unsupported command {args.command!r}")
    except RunnerError as exc:
        print(f"rust_test_runner: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
