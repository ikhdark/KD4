#!/usr/bin/env python3
"""Strict manifest-driven runner for repository-owned Rust test targets."""

from __future__ import annotations

import argparse
import codecs
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
from collections.abc import Callable, Iterable, Mapping, Sequence
from contextlib import ExitStack
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import tomllib

try:
    from .process_owner import owned_process, CleanupFailed
    from .rust_tool_env import cargo_package_specs
except ImportError:
    from process_owner import owned_process, CleanupFailed
    from rust_tool_env import cargo_package_specs

REPO_ROOT = Path(__file__).resolve().parents[1]
CODEX_RS_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_MANIFEST = CODEX_RS_ROOT / ".config" / "kd4-rust-tests.toml"
SCHEMA_VERSION = 1

# Windows test binaries link an 8 MiB main stack and libtest builds its per-test
# worker threads from RUST_MIN_STACK. Keep the runner aligned with the justfile
# and `scripts/rust_build_status.py`.
RUST_MIN_STACK_BYTES = "8388608"
MAX_FAILURE_STREAM_CHARS = 4096


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
        import time

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
        self.base_env = dict(os.environ if env is None else env)
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
        # Ordinary acceptance must never update its expected outputs implicitly.
        self.base_env["INSTA_UPDATE"] = "no"
        self.base_env.setdefault("RUST_MIN_STACK", RUST_MIN_STACK_BYTES)
        self._sccache_disabled = False
        # CARGO_INCREMENTAL stays untouched on purpose. `scripts/just-shell.py`
        # points RUSTC_WRAPPER at sccache for every recipe, and sccache aborts
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
        groups: dict[tuple[str, str, str | None], list[GateStep]] = {}
        for name in dict.fromkeys(names):
            for step in self.gate(name).steps:
                if exact_only and step.filterset is not None:
                    continue
                target = self.target(step.target)
                key = (target.package, target.selector_kind, target.selector_value)
                groups.setdefault(key, []).append(step)
        grouped = []
        for steps in groups.values():
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
                    tuple(
                        dict.fromkeys(
                            helper
                            for step in steps
                            for helper in (
                                self.target(step.target).helpers
                                if step.helpers is None
                                else step.helpers
                            )
                        )
                    ),
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
                "builds": self._helper_build_commands(helpers),
                "run": self._run_command(target, []),
            }
        if name in self.manifest.gates:
            grouped = self._group_gate_steps([name])
            helpers = self._active_helper_names(
                helper for step in grouped for helper in step.helpers or ()
            )
            steps = []
            for step in grouped:
                target = self.target(step.target)
                filter_args = self._gate_filter_args(step)
                steps.append(
                    {
                        "target": step.target,
                        "tests": list(step.tests),
                        "list": self._list_command(target, filter_args),
                        "run": self._gate_run_command(
                            target,
                            ["-E", " | ".join(f"test(={test})" for test in step.tests)],
                        ),
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
        require_core_lib_filter(name, args, allow_all=allow_all)
        target = self.target(name)
        helpers = self.active_helpers([name])
        # Discovery is worthwhile only if some prefix can omit an active helper.
        # Otherwise the direct run already rejects an empty selection.
        if any(
            len(self._active_helper_names(names)) < len(helpers)
            for names in target.helpers_by_test_prefix.values()
        ):
            # Let nextest interpret filters, exclusions and ignored tests. Listing
            # builds the unit binary without building unrelated helper binaries.
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
        env = self._build_helper_environment(helpers)
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
        """Verify exact selections, prepare helpers once, and execute each test once."""
        if not names:
            raise RunnerError("at least one gate is required")
        grouped = self._group_gate_steps(names)
        # Exact generated selections are proved by completed results below.
        # Explicit filters must always prove parity before batching: execution
        # of the declared IDs alone cannot detect an over-broad source filter.
        self.check_gates(names, include_generated=discover)

        env = self._build_helper_environment(
            self._active_helper_names(
                helper for step in grouped for helper in step.helpers or ()
            )
        )
        failures: list[RunnerError] = []
        for step in grouped:
            target = self.target(step.target)
            filter_args = ["-E", " | ".join(f"test(={test})" for test in step.tests)]
            try:
                result = self._checked(
                    self._gate_run_command(target, filter_args),
                    env=env,
                    capture=CAPTURE_BOTH,
                )
            except RunnerError as error:
                if error.outcome in {"cancelled", "timed_out", "cleanup_failed"}:
                    raise
                failures.append(error)
                continue
            # Require completed per-test results from this execution, not just
            # a successful exit or discovery. Suppress summary repetitions and
            # unrelated filtered-out skips, and disallow retries for this proof.
            passed: dict[str, int] = {}
            unexpected = False
            skipped = False
            zero_tests = False
            expected = set(step.tests)
            suffix = (
                f"bin/{target.selector_value}"
                if target.selector_kind == "bin"
                else target.selector_value
            )
            expected_binary = (
                target.package
                if target.selector_kind == "lib"
                else f"{target.package}::{suffix}"
            )
            for stream in ("stdout", "stderr"):
                for line in _output_lines(result, stream):
                    match = re.fullmatch(
                        r"\s*PASS\s+\[[^]\r\n]+\]\s+(?:\(\d+/\d+\)\s+)?(\S+)\s+(\S+)\s*",
                        line,
                    )
                    if match:
                        binary, test = match.groups()
                        if binary == expected_binary and test in expected:
                            passed[test] = min(2, passed.get(test, 0) + 1)
                        else:
                            unexpected = True
                    skipped |= re.match(r"\s*SKIP\s+\[", line) is not None
                    zero_tests |= re.search(r"\b0 tests run\b", line) is not None
            if (
                unexpected
                or set(passed) != expected
                or any(count != 1 for count in passed.values())
            ):
                if not quiet:
                    print(self._failure_detail(result))
                outcome = "not_executed"
                if skipped:
                    outcome = "skipped"
                elif zero_tests:
                    outcome = "zero_tests"
                failures.append(
                    RunnerError(
                        f"gate {step.target!r} did not report every required test passed exactly once: "
                        f"expected={sorted(step.tests)}, passed={passed}, unexpected={unexpected}",
                        outcome=outcome,
                    )
                )
            elif not quiet:
                print(f"gate {step.target}: {len(passed)} passed")
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
        legacy_env["INSTA_UPDATE"] = "no"
        replacement_env = dict(env)
        replacement_env["INSTA_UPDATE"] = "no"
        behavior_args = [
            "--ignore-default-filter",
            "--no-fail-fast",
            "--retries",
            "0",
            "--run-ignored",
            "default",
        ]
        behavior_runs = [
            (legacy, legacy_env),
            *((replacement, replacement_env) for replacement in replacements),
        ]
        failed_runs: list[str] = []
        for target, run_env in behavior_runs:
            command = self._run_command(target, behavior_args, internal_args=True)
            result = self.executor(
                command,
                cwd=CODEX_RS_ROOT,
                env=run_env,
                capture=CAPTURE_NONE,
            )
            if result.returncode != 0:
                detail = self._failure_detail(result)
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
            if (
                replacement.package != legacy.package
                or replacement.selector_kind != "test"
            ):
                continue
            replacement_prefix = f"{replacement.selector_value}__"
            for path in snapshots_dir.glob(f"{replacement_prefix}*.snap"):
                suffix = path.name.removeprefix(replacement_prefix)
                replacements_by_suffix.setdefault(suffix, []).append(path)

        missing = sorted(set(legacy_snapshots) - set(replacements_by_suffix))
        additions = sorted(set(replacements_by_suffix) - set(legacy_snapshots))
        duplicates = sorted(
            suffix for suffix, paths in replacements_by_suffix.items() if len(paths) > 1
        )
        mismatched = sorted(
            suffix
            for suffix, legacy_path in legacy_snapshots.items()
            if len(replacements_by_suffix.get(suffix, [])) == 1
            and RustTestRunner._snapshot_semantics(legacy_path)
            != RustTestRunner._snapshot_semantics(replacements_by_suffix[suffix][0])
        )
        if missing or additions or duplicates or mismatched:
            raise RunnerError(
                "legacy/replacement snapshot parity mismatch: "
                f"missing={missing}, additions={additions}, "
                f"duplicates={duplicates}, content_changes={mismatched}"
            )

    @staticmethod
    def _snapshot_semantics(path: Path) -> str:
        text = path.read_text(encoding="utf-8")
        if not text.startswith("---\n"):
            return text
        header, separator, body = text[4:].partition("\n---\n")
        if not separator:
            return text
        # A shard can move its source file. Retain expression, info, and the
        # complete approved payload; only source-location metadata is incidental.
        header = "\n".join(
            line for line in header.splitlines() if not line.startswith("source:")
        )
        return f"---\n{header}\n---\n{body}"

    def _gate_filter_args(self, step: GateStep) -> list[str]:
        expression = step.filterset or " | ".join(
            f"test(={test})" for test in step.tests
        )
        return ["-E", expression]

    def _gate_run_command(
        self, target: Target, filter_args: Sequence[str]
    ) -> list[str]:
        return [
            *self._run_command(target, filter_args),
            "--color",
            "never",
            "--status-level",
            "pass",
            "--final-status-level",
            "none",
            "--retries",
            "0",
        ]

    def _selection_command(self, verb: str, target: Target) -> list[str]:
        return [
            "cargo",
            "nextest",
            verb,
            "--locked",
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
            "--show-progress",
            "none",
            "--success-output",
            "never",
            *(["--no-fail-fast"] if keep_going else []),
            *args,
        ]

    def _helper_build(self, helpers: Sequence[Helper]) -> list[str] | None:
        # Active helpers share this runner's profile, features and target lane,
        # and Cargo resolves `--bin` across every selected package. One
        # invocation keeps the exact selected binary set while paying a single
        # dependency-graph resolution instead of one per helper package.
        if not helpers:
            return None
        packages = list(dict.fromkeys(helper.package for helper in helpers))
        return [
            "cargo",
            "build",
            "--locked",
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

    def _build_helper_environment(self, helpers: Sequence[Helper]) -> dict[str, str]:
        env = dict(self.base_env)
        command = self._helper_build(helpers)
        if command is None:
            return env
        result = self._checked(command, env=env, capture=CAPTURE_STDOUT)
        helper_dirs: list[str] = []
        for helper in helpers:
            executable = self._helper_artifact(helper, _output_lines(result, "stdout"))
            if str(executable.parent) not in helper_dirs:
                helper_dirs.append(str(executable.parent))
            dashed = f"CARGO_BIN_EXE_{helper.binary}"
            underscored = f"CARGO_BIN_EXE_{helper.binary.replace('-', '_')}"
            env[dashed] = str(executable)
            env[underscored] = str(executable)
            # Test executables resolve bundled helpers before consulting PATH.
            # Refresh that generated layout after every helper build as well.
            if os.name == "nt" and helper.binary in {
                "codex-windows-sandbox-setup",
                "codex-command-runner",
            }:
                resources = executable.parent / "deps" / "codex-resources"
                resources.mkdir(parents=True, exist_ok=True)
                shutil.copy2(executable, resources / executable.name)
        # Native sandbox setup also locates helpers by executable name. Prefer
        # this build over an older installed executable inherited through PATH.
        env["PATH"] = os.pathsep.join([*helper_dirs, env.get("PATH", "")])
        return env

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
            try:
                result = self.executor(
                    list(args),
                    cwd=self.cwd,
                    env=dict(effective_env),
                    capture=capture,
                )
            except CleanupFailed as error:
                (self.target_dir / ".lane-cleanup-unconfirmed").write_text(str(error))
                raise RunnerError(str(error), outcome="cleanup_failed") from error
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
            if detail:
                raise RunnerError(
                    f"command failed ({rendered}), exit code {result.returncode}:\n{detail}"
                )
            raise RunnerError(
                f"command failed ({rendered}), exit code {result.returncode}"
            )
        return result

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
        return transport and compile_failed

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


def require_core_lib_filter(name: str, args: Sequence[str], *, allow_all: bool) -> None:
    if name != "core_lib" or allow_all:
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
        "core_lib requires an explicit test filter (-E <filterset> or a test name). "
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
    if command_timeout_seconds is not None:
        if not math.isfinite(command_timeout_seconds) or command_timeout_seconds <= 0:
            raise RunnerError("command timeout must be a finite positive number")
        env["CODEX_RUST_TEST_TIMEOUT_SECS"] = str(command_timeout_seconds)
    result = executor(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=cwd,
        env=env,
        capture=CAPTURE_BOTH,
    )
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
            require_core_lib_filter(
                args.name, validate_filtering_args(filter_args), allow_all=allow_all
            )

        manifest = Manifest.load(args.manifest)
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
