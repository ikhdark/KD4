#!/usr/bin/env python3
"""Strict manifest-driven runner for repository-owned Rust test targets."""

from __future__ import annotations

import argparse
import json
import os
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


class RunnerError(RuntimeError):
    """Raised when a declared test contract cannot be honored."""


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

    def selection_args(self) -> list[str]:
        args = ["-p", self.package]
        if self.selector_kind == "lib":
            args.append("--lib")
        else:
            args.extend(["--test", self.selector_value or ""])
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
                {"package", "lib", "test", "helpers"},
                f"targets.{target_name}",
            )
            package = _require_string(
                table.get("package"), f"targets.{target_name}.package"
            )
            has_lib = "lib" in table
            has_test = "test" in table
            if has_lib == has_test:
                raise RunnerError(
                    f"targets.{target_name} must declare exactly one of lib or test"
                )
            if has_lib:
                if table["lib"] is not True:
                    raise RunnerError(f"targets.{target_name}.lib must be true")
                selector_kind = "lib"
                selector_value = None
            else:
                selector_kind = "test"
                selector_value = _require_string(
                    table["test"], f"targets.{target_name}.test"
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
            targets[target_name] = Target(
                target_name,
                package,
                selector_kind,
                selector_value,
                tuple(helper_names),
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
        for target in manifest.targets.values():
            package = self._package(target.package, f"target {target.name!r}")
            if target.selector_kind == "lib":
                if not self._has_kind(package, "lib"):
                    raise RunnerError(
                        f"target {target.name!r} declares --lib for package "
                        f"{target.package!r}, which has no library target"
                    )
            elif not self._has_target(package, target.selector_value or "", "test"):
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
                "helpers": [helper.name for helper in helpers],
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
                        "list": self._list_command(target, filter_args),
                        "run": self._run_command(target, filter_args),
                    }
                )
            return {
                "kind": "gate",
                "name": name,
                "target_dir": str(self.target_dir),
                "helpers": [helper.name for helper in helpers],
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
        ]

    def _list_tests(
        self, target: Target, filter_args: Sequence[str]
    ) -> dict[str, bool]:
        args = _list_only_args(validate_filtering_args(filter_args))
        result = self._checked(
            self._list_command(target, args), env=self.base_env, capture_output=True
        )
        tests = parse_nextest_list(result.stdout)
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


def parse_nextest_list(output: str) -> dict[str, bool]:
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


def _resolve_target_dir(value: str | None, metadata: MetadataIndex) -> Path:
    configured = (
        value
        or os.environ.get("CODEX_CARGO_LANE_TARGET_DIR")
        or os.environ.get("CARGO_TARGET_DIR")
    )
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
            for name in manifest.targets:
                print(f"target\t{name}")
            for name in manifest.gates:
                print(f"gate\t{name}")
        elif args.command == "plan":
            print(json.dumps(runner.plan(args.name), indent=2))
        elif args.command == "run-target":
            runner.run_target(args.name, filter_args)
        elif args.command == "run-gate":
            runner.run_gate(args.name)
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
