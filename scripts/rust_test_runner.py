#!/usr/bin/env python3
"""Strict manifest-driven runner for repository-owned Rust test targets."""

from __future__ import annotations

import argparse
import codecs
import filecmp
import fnmatch
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from contextlib import ExitStack
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import tomllib

try:
    from .process_owner import owned_process, CleanupFailed
    from .rust_tool_env import cargo_package_specs, local_rust_env
except ImportError:
    from process_owner import owned_process, CleanupFailed
    from rust_tool_env import cargo_package_specs, local_rust_env

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


class RunnerError(RuntimeError):
    """Raised when a declared test contract cannot be honored."""

    def __init__(self, message: str, *, outcome: str = "failed") -> None:
        super().__init__(message)
        self.outcome = outcome


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
                if not set(names).issubset(helper_names):
                    raise RunnerError(
                        f"helper prefix {prefix} must be a subset of target helpers"
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
                tests = _require_string_list(step.get("tests"), f"{prefix}.tests")
                if not tests:
                    raise RunnerError(f"{prefix}.tests must not be empty")
                _reject_duplicates(tests, f"{prefix}.tests")
                step_helpers = None
                if "helpers" in step:
                    names = _require_string_list(step["helpers"], f"{prefix}.helpers")
                    _reject_duplicates(names, f"{prefix}.helpers")
                    if not set(names).issubset(targets[target_name].helpers):
                        raise RunnerError(
                            f"{prefix}.helpers must be a subset of target helpers"
                        )
                    step_helpers = tuple(names)
                steps.append(
                    GateStep(target_name, filterset, tuple(tests), step_helpers)
                )
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
    with ExitStack() as stack:
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
        try:
            returncode = process.wait(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
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
        command_timeout_seconds: float | None = None,
        env: Mapping[str, str] | None = None,
        cwd: Path = CODEX_RS_ROOT,
    ) -> None:
        self.manifest = manifest
        self.metadata = metadata
        self.target_dir = (target_dir or metadata.target_directory).resolve()
        self.platform = platform or current_platform()
        self.executor = executor
        self.cwd = cwd
        self.no_fail_fast = no_fail_fast
        self.base_env = {
            key: value
            for key, value in (os.environ if env is None else env).items()
            # Only helpers built for this run may satisfy a helper lookup.
            if not key.upper().startswith("CARGO_BIN_EXE_")
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
        # CARGO_INCREMENTAL stays untouched on purpose. The shared launcher policy
        # points RUSTC_WRAPPER at sccache, and sccache aborts
        # the whole build when that variable asks for incremental compilation
        # while refusing to honor it when it asks for "0". Leaving it unset lets
        # `codex-rs/.cargo/config.toml` give workspace crates the incremental
        # cache -- the only one a narrow edit/test loop can use -- while sccache
        # still serves the registry dependencies it does cache.
        if profile is not None:
            self.base_env["NEXTEST_PROFILE"] = profile

    def target(self, name: str) -> Target:
        try:
            target = self.manifest.targets[name]
            self.metadata.validate_target(target)
            return target
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test target {name!r}") from exc

    def gate(self, name: str) -> Gate:
        try:
            return self.manifest.gates[name]
        except KeyError as exc:
            raise RunnerError(f"unknown named Rust test gate {name!r}") from exc

    def active_helpers(self, target_names: Iterable[str]) -> list[Helper]:
        return self._active_helper_names(
            name for target in target_names for name in self.target(target).helpers
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
        self, names: Sequence[str], *, exact_only: bool = False
    ) -> list[GateStep]:
        groups: dict[
            tuple[str, str, str | None, frozenset[str]],
            tuple[tuple[str, ...], list[GateStep]],
        ] = {}
        for name in dict.fromkeys(names):
            for step in self.gate(name).steps:
                if exact_only and step.filterset is not None:
                    continue
                target = self.target(step.target)
                helpers = tuple(
                    helper.name
                    for helper in self._active_helper_names(
                        target.helpers if step.helpers is None else step.helpers
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
                    tuple(dict.fromkeys(test for step in steps for test in step.tests)),
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
                "run": self._run_command(target, []),
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

    def run_target(
        self,
        name: str,
        filter_args: Sequence[str],
        *,
        no_fail_fast: bool | None = None,
        allow_all: bool = False,
    ) -> None:
        args = validate_filtering_args(filter_args)
        target = self.target(name)
        require_core_lib_filter(target, args, allow_all=allow_all)
        helpers = self.active_helpers([name])
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
            for names in target.helpers_by_test_prefix.values()
        ):
            # Exact IDs already bound the selection, so their helpers need no
            # discovery invocation; an ID the run cannot select only adds helpers.
            selected = _exact_test_ids(args)
            if selected is None:
                # Let nextest interpret filters, exclusions and ignored tests.
                # Listing builds the unit binary without unrelated helpers.
                print(
                    "Selecting tests with nextest list (this compiles the test binary).",
                    file=sys.stderr,
                )
                selected = self._list_tests(target, args)
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
        env = self._helper_environment([target], helpers, self._build_helpers(helpers))
        self._checked(
            self._run_command(target, args, no_fail_fast=no_fail_fast),
            env=env,
            capture=CAPTURE_NONE,
        )

    def check_gates(
        self, names: Sequence[str], *, include_generated: bool = True
    ) -> None:
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
        for step in discovery_steps:
            target = self.target(step.target)
            listed = self._list_tests(target, self._gate_filter_args(step))
            actual = set(listed)
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
                    f"gates {list(names)!r} step {step.target!r} selected the wrong test-ID set: "
                    + ", ".join(details)
                )
            ignored = sorted(test for test, is_ignored in listed.items() if is_ignored)
            if ignored:
                raise RunnerError(
                    f"gate requires ignored tests that would not execute: {ignored}",
                    outcome="skipped",
                )

    def run_gate(self, name: str) -> None:
        self.run_gates([name])

    def run_gates(
        self, names: Sequence[str], *, quiet: bool = False, discover: bool = False
    ) -> dict[str, list[str]]:
        """Verify exact selections, build helpers once, and execute each test once
        per declared helper set with only that set exported."""
        if not names:
            raise RunnerError("at least one gate is required")
        grouped = self._group_gate_steps(names)
        # Exact generated selections are proved by completed results below.
        # Explicit filters must always prove parity before batching: execution
        # of the declared IDs alone cannot detect an over-broad source filter.
        self.check_gates(names, include_generated=discover)

        artifacts = self._build_helpers(
            self._active_helper_names(
                helper for step in grouped for helper in step.helpers or ()
            )
        )
        failures: list[RunnerError] = []
        for batch in self._gate_batches(grouped):
            targets = [self.target(step.target) for step in batch]
            env = self._helper_environment(
                targets, self._active_helper_names(batch[0].helpers or ()), artifacts
            )
            try:
                result = self._checked(
                    self._gate_run_command(
                        targets[0],
                        self._batch_filter_args(targets, batch),
                        batch=targets[1:],
                    ),
                    env=env,
                    capture=CAPTURE_BOTH,
                )
            except RunnerError as error:
                if error.outcome in {"cancelled", "timed_out", "cleanup_failed"}:
                    if not failures:
                        raise
                    # Gate runs capture their output, so an earlier group's
                    # failure is reported nowhere else; the stop keeps its outcome.
                    raise RunnerError(
                        f"{error}\ngate runs that failed before the stop:\n"
                        + "\n".join(str(failure) for failure in failures),
                        outcome=error.outcome,
                    ) from error
                failures.append(error)
                continue
            # Require completed per-test results from this execution, not just
            # a successful exit or discovery. Suppress summary repetitions and
            # unrelated filtered-out skips, and disallow retries for this proof.
            # Each result must come from the binary of the step declaring it.
            expected = {
                _nextest_binary_id(target): set(step.tests)
                for target, step in zip(targets, batch)
            }
            passed: dict[tuple[str, str], int] = {}
            unexpected = False
            for stream in ("stdout", "stderr"):
                for line in _output_lines(result, stream):
                    # Nextest reports a passing test that leaked handles as
                    # LEAK; LEAK-FAIL does not match and fails the run.
                    match = re.fullmatch(
                        r"\s*(?:PASS|LEAK)\s+\[[^]\r\n]+\]\s+(?:\(\d+/\d+\)\s+)?(\S+)\s+(\S+)\s*",
                        line,
                    )
                    if match:
                        binary, test = match.groups()
                        if test in expected.get(binary, ()):
                            key = (binary, test)
                            passed[key] = min(2, passed.get(key, 0) + 1)
                        else:
                            unexpected = True
            required = {
                (binary, test) for binary, tests in expected.items() for test in tests
            }
            if (
                unexpected
                or set(passed) != required
                or any(count != 1 for count in passed.values())
            ):
                if not quiet:
                    print(self._failure_detail(result))
                # `--status-level pass` hides SKIP lines and `--no-tests=fail`
                # fails an empty run before this point, so a missing or ignored
                # test is only known as not executed; `check-gates` names it.
                steps = ", ".join(repr(step.target) for step in batch)
                reported = {f"{binary} {test}": n for (binary, test), n in passed.items()}
                failures.append(
                    RunnerError(
                        f"gate {steps} did not report every required test passed exactly once: "
                        f"expected={sorted(f'{binary} {test}' for binary, test in required)}, "
                        f"passed={reported}, unexpected={unexpected}",
                        outcome="not_executed",
                    )
                )
            elif not quiet:
                for step in batch:
                    print(f"gate {step.target}: {len(step.tests)} passed")
        if failures:
            outcomes = {error.outcome for error in failures}
            raise RunnerError(
                "gate runs failed after executing every selected target:\n"
                + "\n".join(str(error) for error in failures),
                outcome=next(iter(outcomes)) if len(outcomes) == 1 else "failed",
            )
        return {
            name: sorted(
                {test for step in self.gate(name).steps for test in step.tests}
            )
            for name in dict.fromkeys(names)
        }

    def _gate_filter_args(self, step: GateStep) -> list[str]:
        expression = step.filterset or " | ".join(
            f"test(={test})" for test in step.tests
        )
        return ["-E", expression]

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
            return " | ".join(f"test(={test})" for test in step.tests)

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
    ) -> list[str]:
        args = validate_filtering_args(filter_args)
        keep_going = self.no_fail_fast if no_fail_fast is None else no_fail_fast
        return [
            *self._selection_command("run", target, *batch),
            "--no-tests=fail",
            "--show-progress",
            "none",
            "--success-output",
            "never",
            *(["--no-fail-fast"] if keep_going else []),
            *args,
        ]

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
            *(arg for package in packages for arg in ("-p", package)),
            *(arg for helper in helpers for arg in ("--bin", helper.binary)),
        ]

    def _helper_build_commands(self, helpers: Sequence[Helper]) -> list[list[str]]:
        command = self._helper_build(helpers)
        return [] if command is None else [command]

    def _list_tests(
        self, target: Target, filter_args: Sequence[str]
    ) -> dict[str, bool]:
        args = _list_only_args(validate_filtering_args(filter_args))
        result = self._checked(
            self._list_command(target, args), env=self.base_env, capture=CAPTURE_STDOUT
        )
        tests = parse_nextest_list(_stdout_text(result))
        if not tests:
            raise RunnerError(
                f"named target {target.name!r} selected zero tests with args {args!r}",
                outcome="zero_tests",
            )
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
        return {
            helper.name: self._helper_artifact(helper, _output_lines(result, "stdout"))
            for helper in helpers
        }

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
                    self.target_dir / "debug",
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

    def _helper_artifact(self, helper: Helper, output: str | Iterable[str]) -> Path:
        expected_package_id = self.metadata.package_id(helper.package)
        executables: list[Path] = []
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

    def _execute(
        self,
        args: Sequence[str],
        *,
        env: Mapping[str, str],
        capture: str,
    ) -> subprocess.CompletedProcess[str]:
        try:
            return self.executor(
                list(args), cwd=self.cwd, env=dict(env), capture=capture
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
        for attempt in range(2):
            phase = (
                "compile/discover"
                if list(args[:3]) == ["cargo", "nextest", "list"]
                else "helper-build"
                if list(args[:2]) == ["cargo", "build"]
                else "build/test"
            )
            started = time.monotonic()
            result = None
            print(
                f"Rust phase {phase}: starting {subprocess.list2cmdline(list(args))}",
                file=sys.stderr,
            )
            try:
                result = self._execute(args, env=effective_env, capture=capture)
            finally:
                elapsed = time.monotonic() - started
                reported = (
                    self._reported_durations(result) if result is not None else {}
                )
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
            rendered = subprocess.list2cmdline(list(args))
            if result.returncode in (-1, 0xFFFFFFFF):
                detail = (
                    "Process exited 0xFFFFFFFF without a normal Cargo exit code. "
                    "Inspect retained logs and process-owner/OS termination evidence; "
                    "this is not classified as a test failure or retried automatically.\n"
                    + detail
                )
            if detail:
                raise RunnerError(
                    f"command failed ({rendered}), exit code {result.returncode}:\n{detail}"
                )
            raise RunnerError(
                f"command failed ({rendered}), exit code {result.returncode}"
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

    def _failure_detail(
        self,
        result: subprocess.CompletedProcess[str],
        *,
        include_stdout: bool = True,
    ) -> str:
        streams = [
            (name, output)
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
        try:
            log_dir = self.target_dir / "test-runner-logs"
            log_dir.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                newline="",
                suffix=".log",
                prefix="failure-",
                dir=log_dir,
                delete=False,
            ) as log:
                log.write(full_detail)
                log_path = log.name
        except OSError:
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
    configured = (
        value
        or os.environ.get("CODEX_CARGO_LANE_TARGET_DIR")
        or os.environ.get("CARGO_TARGET_DIR")
    )
    if configured is None:
        return metadata.target_directory
    path = Path(configured)
    return path if path.is_absolute() else cwd / path


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
    run_options.add_argument(
        "--command-timeout-seconds",
        type=float,
        help="Deadline for each child command, including process-tree cleanup; unlimited by default.",
    )
    run_options.add_argument("--target-dir", default=argparse.SUPPRESS)

    subparsers.add_parser("check-manifest")
    subparsers.add_parser("list-targets")
    check_gates = subparsers.add_parser("check-gates")
    check_gates.add_argument("names", nargs="+")
    plan = subparsers.add_parser("plan")
    plan.add_argument("name")
    run_target = subparsers.add_parser("run-target", parents=[run_options])
    run_target.add_argument(
        "--all",
        action="store_true",
        help="Explicitly allow an unfiltered core_lib run.",
    )
    run_target.add_argument("name")
    run_target.add_argument("filter_args", nargs=argparse.REMAINDER)
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


def _split_runner_owned_options(
    filter_args: Sequence[str],
) -> tuple[list[str], set[str]]:
    """Separates runner-owned execution flags from caller filtering args.

    Recipes may pass `--no-fail-fast` and the explicit full-library opt-in
    `--all` after the target name. Consume these policy flags instead of
    forwarding them as nextest selection overrides.
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
        allow_all = getattr(args, "all", False)
        if args.command == "run-target":
            filter_args = list(args.filter_args)
            if filter_args[:1] == ["--"]:
                filter_args = filter_args[1:]
            filter_args, owned = _split_runner_owned_options(filter_args)
            no_fail_fast = no_fail_fast or "--no-fail-fast" in owned
            allow_all = allow_all or "--all" in owned
            validate_filtering_args(filter_args)

        manifest = Manifest.load(args.manifest)
        if args.command == "run-target":
            require_core_lib_filter(
                manifest.targets.get(args.name), filter_args, allow_all=allow_all
            )
        if args.command == "list-targets":
            for name in manifest.targets:
                print(f"target\t{name}")
            for name in manifest.gates:
                print(f"gate\t{name}")
            return 0
        timeout = getattr(args, "command_timeout_seconds", None)
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
            no_fail_fast=no_fail_fast,
            command_timeout_seconds=getattr(args, "command_timeout_seconds", None),
        )
        if args.command == "check-manifest":
            metadata.validate_manifest(manifest)
            print(
                f"validated Rust test manifest version {manifest.version}: {args.manifest}"
            )
        elif args.command == "check-gates":
            runner.check_gates(args.names)
        elif args.command == "plan":
            print(json.dumps(runner.plan(args.name), indent=2))
        elif args.command == "run-target":
            runner.run_target(args.name, filter_args, allow_all=allow_all)
        elif args.command == "run-gate":
            runner.run_gates(args.names)
        else:  # pragma: no cover - argparse enforces the command set.
            raise RunnerError(f"unsupported command {args.command!r}")
    except RunnerError as exc:
        print(f"rust_test_runner: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
