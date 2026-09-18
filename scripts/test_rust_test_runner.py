#!/usr/bin/env python3
"""Unit tests for the strict, manifest-driven Rust test runner.

Most tests use a fake executor; process lifecycle tests launch Python children.
No Cargo command runs and no test binary is built. The Cargo metadata fixture
mirrors the shape consumed from `cargo metadata --no-deps`.
"""

from __future__ import annotations

import contextlib
import copy
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, ClassVar
from unittest import mock

from scripts import rust_test_runner
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.rust_test_runner import (
    Manifest,
    MetadataIndex,
    RunnerError,
    RustTestRunner,
)

MANIFEST_DATA: dict[str, Any] = {
    "version": 1,
    "helpers": {
        "codex": {"package": "codex-cli", "bin": "codex"},
        "codex-code-mode-host": {
            "package": "codex-code-mode-host",
            "bin": "codex-code-mode-host",
        },
        "test_stdio_server": {
            "package": "codex-rmcp-client",
            "bin": "test_stdio_server",
        },
        "test_streamable_http_server": {
            "package": "codex-rmcp-client",
            "bin": "test_streamable_http_server",
        },
        "codex-command-runner": {
            "package": "codex-windows-sandbox",
            "bin": "codex-command-runner",
            "platform": "windows",
        },
    },
    "targets": {
        "core_lib": {
            "package": "codex-core",
            "lib": True,
            "helpers": ["codex", "codex-command-runner"],
        },
        "core_all": {
            "package": "codex-core",
            "test": "all",
            "helpers": ["codex", "codex-code-mode-host", "test_stdio_server"],
        },
        "core_shard": {
            "package": "codex-core",
            "test": "core_shard",
            "helpers": ["codex"],
        },
        "core_shard_two": {
            "package": "codex-core",
            "test": "core_shard_two",
            "helpers": ["codex"],
        },
    },
    "gates": {
        "demo-gate": {
            "description": "Two targets so the helper union is observable.",
            "steps": [
                {
                    "target": "core_lib",
                    "filter": "test(alpha)",
                    "tests": ["mod::tests::alpha"],
                },
                {
                    "target": "core_all",
                    "filter": "test(beta)",
                    "tests": ["suite::mod::beta"],
                },
            ],
        },
    },
}

METADATA_PACKAGES: list[dict[str, Any]] = [
    {
        "name": "codex-core",
        "id": "path+file:///codex-core#0.0.0",
        "targets": [
            {"name": "codex_core", "kind": ["lib"]},
            {"name": "all", "kind": ["test"]},
            {"name": "core_shard", "kind": ["test"]},
            {"name": "core_shard_two", "kind": ["test"]},
        ],
    },
    {
        "name": "codex-cli",
        "id": "path+file:///codex-cli#0.0.0",
        "targets": [{"name": "codex", "kind": ["bin"]}],
    },
    {
        "name": "codex-code-mode-host",
        "id": "path+file:///codex-code-mode-host#0.0.0",
        "targets": [{"name": "codex-code-mode-host", "kind": ["bin"]}],
    },
    {
        "name": "codex-rmcp-client",
        "id": "path+file:///codex-rmcp-client#0.0.0",
        "targets": [
            {"name": "codex_rmcp_client", "kind": ["lib"]},
            {"name": "test_stdio_server", "kind": ["bin"]},
            {"name": "test_streamable_http_server", "kind": ["bin"]},
        ],
    },
    {
        "name": "codex-windows-sandbox",
        "id": "path+file:///codex-windows-sandbox#0.0.0",
        "targets": [{"name": "codex-command-runner", "kind": ["bin"]}],
    },
]


def nextest_list_payload(tests: dict[str, bool]) -> str:
    """Renders the `cargo nextest list -T json` shape the runner parses."""
    return json.dumps(
        {
            "test-count": len(tests),
            "rust-suites": {
                "codex-core::fixture": {
                    "package-name": "codex-core",
                    "testcases": {
                        test_id: {"ignored": ignored}
                        for test_id, ignored in tests.items()
                    },
                }
            },
        }
    )


class FakeExecutor:
    """Records every command and answers with canned Cargo output."""

    def __init__(
        self,
        *,
        artifacts: dict[str, Path] | None = None,
        listings: dict[str, dict[str, bool]] | None = None,
        default_listing: dict[str, bool] | None = None,
        failing_runs: set[str] | None = None,
    ) -> None:
        self.artifacts = artifacts or {}
        self.listings = listings or {}
        self.default_listing = (
            default_listing
            if default_listing is not None
            else {"mod::tests::alpha": False}
        )
        self.failing_runs = failing_runs or set()
        self.calls: list[dict[str, Any]] = []

    def __call__(
        self,
        args: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        capture: str,
    ) -> subprocess.CompletedProcess[str]:
        self.calls.append(
            {
                "args": list(args),
                "cwd": cwd,
                "env": dict(env),
                "capture": capture,
            }
        )
        selector = self._selector_for(args)
        failed = (
            args[:3] == ["cargo", "nextest", "run"] and selector in self.failing_runs
        )
        # Mirror the real streams: an uncaptured stream is `None`, never "".
        return subprocess.CompletedProcess(
            list(args),
            1 if failed else 0,
            stdout=self._stdout(args)
            if capture != rust_test_runner.CAPTURE_NONE
            else None,
            stderr=(f"failed {selector}" if failed else "")
            if capture == rust_test_runner.CAPTURE_BOTH
            else None,
        )

    def _stdout(self, args: list[str]) -> str:
        if args[:3] == ["cargo", "nextest", "list"]:
            return nextest_list_payload(self._listing_for(args))
        if args[:3] == ["cargo", "nextest", "run"]:
            return "\n".join(
                f"{'SKIP' if ignored else 'PASS'} [ 0.001s] fixture {test}"
                for test, ignored in self._listing_for(args).items()
            )
        if args[:2] == ["cargo", "build"]:
            return self._artifact_output(args)
        return ""

    def _listing_for(self, args: list[str]) -> dict[str, bool]:
        selector = self._selector_for(args)
        return self.listings.get(selector, self.default_listing)

    @staticmethod
    def _selector_for(args: list[str]) -> str:
        return args[args.index("--test") + 1] if "--test" in args else "--lib"

    def _artifact_output(self, args: list[str]) -> str:
        """Cargo resolves `--bin` across every selected `-p`, so attribute each
        binary to its own owning package rather than to the first one."""
        binaries = [args[index + 1] for index, arg in enumerate(args) if arg == "--bin"]
        packages = {args[index + 1] for index, arg in enumerate(args) if arg == "-p"}
        owners = {
            target["name"]: entry["id"]
            for entry in METADATA_PACKAGES
            if entry["name"] in packages
            for target in entry["targets"]
            if "bin" in target["kind"]
        }
        return "\n".join(
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "package_id": owners[binary],
                    "target": {"name": binary, "kind": ["bin"]},
                    "executable": str(executable),
                }
            )
            for binary in binaries
            if (executable := self.artifacts.get(binary)) is not None
        )

    def commands(self, prefix: list[str]) -> list[list[str]]:
        return [
            call["args"] for call in self.calls if call["args"][: len(prefix)] == prefix
        ]

    def last_env(self) -> dict[str, str]:
        return self.calls[-1]["env"]


# Distinguishes "use this fixture's lane directory" from an explicit `None`,
# which asks the runner to fall back to the Cargo metadata target directory.
_USE_LANE_TARGET_DIR: Any = object()


class RunnerTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = Path(tempfile.mkdtemp(prefix="rust-test-runner-"))
        self.addCleanup(self._cleanup)
        self.target_dir = self.temp_dir / "lanes" / "demo"
        self.target_dir.mkdir(parents=True)

    def _cleanup(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def manifest(self, **overrides: Any) -> Manifest:
        data = copy.deepcopy(MANIFEST_DATA)
        data.update(overrides)
        return Manifest.from_data(data)

    def metadata(self) -> MetadataIndex:
        return MetadataIndex.from_json(
            {
                "target_directory": str(self.temp_dir / "target"),
                "packages": copy.deepcopy(METADATA_PACKAGES),
            }
        )

    def helper_executable(self, name: str) -> Path:
        path = self.target_dir / "debug" / f"{name}.exe"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"")
        return path

    def runner(
        self,
        *,
        executor: FakeExecutor | None = None,
        platform: str = "windows",
        target_dir: Path | None = _USE_LANE_TARGET_DIR,
        manifest: Manifest | None = None,
    ) -> tuple[RustTestRunner, FakeExecutor]:
        executor = executor or FakeExecutor()
        runner = RustTestRunner(
            manifest or self.manifest(),
            self.metadata(),
            target_dir=self.target_dir
            if target_dir is _USE_LANE_TARGET_DIR
            else target_dir,
            platform=platform,
            executor=executor,
        )
        return runner, executor


class RealExecutorTest(RunnerTestCase):
    def test_metadata_failure_preserves_full_log_recovery(self):
        results = []

        def execute_metadata(*args, **kwargs):
            result = self.execute(
                "import sys; sys.stderr.write('metadata diagnostic' * 100000); sys.exit(7)"
            )
            results.append(result)
            return result

        with self.assertRaises(RunnerError) as raised:
            rust_test_runner.load_metadata(execute_metadata)
        self.assertIn(str(results[0].stderr_path), str(raised.exception))
        self.assertEqual(
            results[0].stderr_path.read_text(), "metadata diagnostic" * 100000
        )
        self.assertLess(len(str(raised.exception)), 5000)

    def execute(self, script: str, **extra_env: str):
        return rust_test_runner._default_executor(
            [sys.executable, "-c", script],
            cwd=self.temp_dir,
            env={
                **os.environ,
                "CODEX_RUST_TEST_LOG_DIR": str(self.temp_dir),
                **extra_env,
            },
            capture=rust_test_runner.CAPTURE_BOTH,
        )

    def test_large_logs_are_file_backed_with_bounded_diagnostics(self):
        result = self.execute(
            "import sys; print('start'); print('x' * 2000000); print('end'); sys.stderr.write('failure' * 300000)"
        )
        self.assertEqual(result.returncode, 0)
        self.assertLessEqual(
            len(result.stdout), rust_test_runner.MAX_FAILURE_STREAM_CHARS
        )
        self.assertLessEqual(
            len(result.stderr), rust_test_runner.MAX_FAILURE_STREAM_CHARS
        )
        self.assertEqual(
            result.stdout_path.stat().st_size, 2_000_011 + (3 if os.name == "nt" else 0)
        )
        self.assertEqual(result.stderr_path.read_text(), "failure" * 300000)
        self.assertTrue(result.stdout.endswith("end\n"))
        self.assertEqual(
            list(rust_test_runner._output_lines(result, "stdout")), ["start\n", "end\n"]
        )
        runner, _ = self.runner()
        detail = runner._failure_detail(result)
        self.assertIn(str(result.stdout_path), detail)
        self.assertIn(str(result.stderr_path), detail)
        self.assertLess(len(detail), 10000)

    def test_gate_counts_early_statuses_from_log_and_rejects_duplicates(self):
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"] = [
            {"target": "core_lib", "tests": ["mod::tests::alpha"], "helpers": []}
        ]
        runner = RustTestRunner(
            Manifest.from_data(data), self.metadata(), target_dir=self.target_dir
        )
        for count in (1, 2):
            with self.subTest(count=count):
                command = [
                    sys.executable,
                    "-c",
                    f"print('PASS [0.1s] core mod::tests::alpha\\n' * {count}); print('x' * 2000000)",
                ]
                with mock.patch.object(
                    runner, "_gate_run_command", return_value=command
                ):
                    if count == 1:
                        self.assertEqual(
                            runner.run_gates(["demo-gate"], quiet=True),
                            {"demo-gate": ["mod::tests::alpha"]},
                        )
                    else:
                        with self.assertRaises(RunnerError) as failure:
                            runner.run_gates(["demo-gate"], quiet=True)
                        self.assertEqual(failure.exception.outcome, "not_executed")

    def test_deadline_stops_parent_and_descendant(self):
        pid_file = self.temp_dir / "child.pid"
        child = f"import os,time; from pathlib import Path; Path({str(pid_file)!r}).write_text(str(os.getpid())); time.sleep(60)"
        parent = f"import subprocess,sys,time; subprocess.Popen([sys.executable, '-c', {child!r}]); time.sleep(60)"
        with self.assertRaises(RunnerError) as failure:
            self.execute(parent, CODEX_RUST_TEST_TIMEOUT_SECS="2")
        self.assertEqual(failure.exception.outcome, "timed_out")
        self.assertTrue(
            pid_file.exists(), "descendant must have started before the deadline"
        )
        child_pid = int(pid_file.read_text())
        if os.name == "nt":
            status = subprocess.run(
                ["tasklist", "/FI", f"PID eq {child_pid}", "/FO", "CSV", "/NH"],
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertNotIn(f'"{child_pid}"', status.stdout)
        else:
            with self.assertRaises(ProcessLookupError):
                os.kill(child_pid, 0)
        self.assertIn("Full stdout:", str(failure.exception))

    def test_interrupt_is_cancelled_and_reaps_the_child(self):
        real_wait = subprocess.Popen.wait
        observed = []

        def interrupt_once(process, *args, **kwargs):
            if not observed:
                observed.append(process)
                raise KeyboardInterrupt
            return real_wait(process, *args, **kwargs)

        with mock.patch.object(subprocess.Popen, "wait", interrupt_once):
            with self.assertRaises(RunnerError) as failure:
                self.execute("import time; time.sleep(60)")
        self.assertEqual(failure.exception.outcome, "cancelled")
        self.assertIsNotNone(observed[0].returncode)


class ManifestSchemaTest(RunnerTestCase):
    def test_list_targets_does_not_load_cargo_metadata(self):
        output = io.StringIO()
        with (
            mock.patch.object(
                rust_test_runner,
                "load_metadata",
                side_effect=AssertionError("metadata discovery"),
            ),
            contextlib.redirect_stdout(output),
        ):
            self.assertEqual(rust_test_runner.main(["list-targets"]), 0)
        self.assertIn("target\t", output.getvalue())
        self.assertIn("gate\t", output.getvalue())

    def test_unknown_top_level_key_is_rejected(self) -> None:
        with self.assertRaisesRegex(RunnerError, "unknown keys: profiles"):
            self.manifest(profiles={})

    def test_unknown_helper_key_is_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["binary"] = "codex"
        with self.assertRaisesRegex(
            RunnerError, r"helpers\.codex contains unknown keys"
        ):
            Manifest.from_data(data)

    def test_unknown_target_key_is_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["profile"] = "fast"
        with self.assertRaisesRegex(
            RunnerError, r"targets\.core_lib contains unknown keys: profile"
        ):
            Manifest.from_data(data)

    def test_unknown_gate_step_key_is_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["retries"] = 0
        with self.assertRaisesRegex(
            RunnerError, r"steps\[0\] contains unknown keys: retries"
        ):
            Manifest.from_data(data)

    def test_version_must_match_the_supported_schema(self) -> None:
        with self.assertRaisesRegex(RunnerError, "manifest.version must be 1"):
            self.manifest(version=2)

    def test_target_must_declare_exactly_one_selector(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["test"] = "all"
        with self.assertRaisesRegex(RunnerError, "exactly one of lib, test or bin"):
            Manifest.from_data(data)

        data = copy.deepcopy(MANIFEST_DATA)
        del data["targets"]["core_all"]["test"]
        with self.assertRaisesRegex(RunnerError, "exactly one of lib, test or bin"):
            Manifest.from_data(data)

    def test_target_helper_reference_must_exist(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_lib"]["helpers"] = ["not-a-helper"]
        with self.assertRaisesRegex(RunnerError, "unknown helper 'not-a-helper'"):
            Manifest.from_data(data)

    def test_gate_step_target_reference_must_exist(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["target"] = "core_missing"
        with self.assertRaisesRegex(RunnerError, "unknown target 'core_missing'"):
            Manifest.from_data(data)

    def test_gate_step_requires_expected_test_ids(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["tests"] = []
        with self.assertRaisesRegex(RunnerError, "must not be empty"):
            Manifest.from_data(data)

    def test_duplicate_expected_test_ids_are_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"]["demo-gate"]["steps"][0]["tests"] = ["a::b", "a::b"]
        with self.assertRaisesRegex(RunnerError, "contains duplicates: a::b"):
            Manifest.from_data(data)

    def test_helper_platform_must_be_known(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["platform"] = "solaris"
        with self.assertRaisesRegex(RunnerError, "must be windows, linux, or macos"):
            Manifest.from_data(data)


class MetadataValidationTest(RunnerTestCase):
    def test_metadata_deadline_reaches_executor_and_rejects_invalid_values(
        self,
    ) -> None:
        executor = mock.Mock(
            return_value=subprocess.CompletedProcess(
                [],
                0,
                json.dumps(
                    {
                        "target_directory": str(self.target_dir),
                        "packages": METADATA_PACKAGES,
                    }
                ),
                "",
            )
        )
        metadata = rust_test_runner.load_metadata(
            executor, command_timeout_seconds=123.5
        )
        self.assertEqual(metadata.target_directory, self.target_dir)
        self.assertEqual(
            executor.call_args.kwargs["env"]["CODEX_RUST_TEST_TIMEOUT_SECS"], "123.5"
        )
        for invalid in (0, -1, float("nan"), float("inf")):
            executor.reset_mock()
            with (
                self.subTest(invalid=invalid),
                self.assertRaisesRegex(RunnerError, "finite positive"),
            ):
                rust_test_runner.load_metadata(
                    executor, command_timeout_seconds=invalid
                )
            executor.assert_not_called()

    def test_helper_binary_must_exist_in_cargo_metadata(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["bin"] = "codex-renamed"
        runner, executor = self.runner(manifest=Manifest.from_data(data))
        with self.assertRaisesRegex(RunnerError, "declares missing binary"):
            runner.run_target("core_all", [])
        self.assertEqual(executor.calls, [])

    def test_test_target_must_exist_in_cargo_metadata(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["test"] = "gone"
        runner, executor = self.runner(manifest=Manifest.from_data(data))
        with self.assertRaisesRegex(RunnerError, "declares missing test target"):
            runner.run_target("core_all", [])
        self.assertEqual(executor.calls, [])

    def test_unknown_package_is_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["package"] = "codex-nope"
        runner, executor = self.runner(manifest=Manifest.from_data(data))
        with self.assertRaisesRegex(RunnerError, "unknown Cargo package 'codex-nope'"):
            runner.run_target("core_all", [])
        self.assertEqual(executor.calls, [])


class SelectedMetadataValidationTest(RunnerTestCase):
    def test_unrelated_stale_target_does_not_block_selected_run(self):
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["test"] = "gone"
        data["targets"]["core_lib"]["helpers"] = []
        runner, executor = self.runner(manifest=Manifest.from_data(data))
        runner.run_target("core_lib", ["alpha"])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)
        with self.assertRaisesRegex(RunnerError, "missing test target"):
            runner.run_target("core_all", [])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)

    def test_stale_required_helper_fails_before_build(self):
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["bin"] = "gone"
        runner, executor = self.runner(manifest=Manifest.from_data(data))
        with self.assertRaisesRegex(RunnerError, "missing binary"):
            runner.run_target("core_lib", ["alpha"])
        self.assertEqual(executor.calls, [])


class NamedSelectionTest(RunnerTestCase):
    def test_unknown_target_name_fails(self) -> None:
        runner, _ = self.runner()
        with self.assertRaisesRegex(
            RunnerError, "unknown named Rust test target 'nope'"
        ):
            runner.target("nope")

    def test_unknown_plan_name_fails(self) -> None:
        runner, _ = self.runner()
        with self.assertRaisesRegex(
            RunnerError, "unknown named Rust test target or gate"
        ):
            runner.plan("nope")

    def test_selection_is_taken_from_the_manifest(self) -> None:
        runner, _ = self.runner()
        self.assertEqual(
            runner.plan("core_all")["selection"], ["-p", "codex-core", "--test", "all"]
        )
        self.assertEqual(
            runner.plan("core_lib")["selection"], ["-p", "codex-core", "--lib"]
        )


class FilteringArgumentPolicyTest(unittest.TestCase):
    def test_package_and_target_overrides_are_rejected(self) -> None:
        for argv in (
            ["-p", "codex-tui"],
            ["--package=codex-tui"],
            ["--workspace"],
            ["--test", "all"],
            ["--test=all"],
            ["--lib"],
            ["--all-targets"],
            ["--manifest-path", "Cargo.toml"],
            ["--target-dir", "target"],
            ["--target-dir=target"],
        ):
            with (
                self.subTest(argv=argv),
                self.assertRaisesRegex(RunnerError, "cannot override a named target"),
            ):
                rust_test_runner.validate_filtering_args(argv)

    def test_no_tests_override_is_rejected(self) -> None:
        for argv in (["--no-tests"], ["--no-tests=pass"], ["--no-tests=fail"]):
            with (
                self.subTest(argv=argv),
                self.assertRaisesRegex(RunnerError, "--no-tests is runner-owned"),
            ):
                rust_test_runner.validate_filtering_args(argv)

    def test_filtering_and_ignored_options_are_permitted(self) -> None:
        argv = [
            "-E",
            "test(alpha)",
            "--run-ignored",
            "only",
            "suite::live_cli",
            "--",
            "--exact",
            "--skip",
            "slow",
        ]
        self.assertEqual(rust_test_runner.validate_filtering_args(argv), argv)

    def test_run_ignored_value_is_validated(self) -> None:
        with self.assertRaisesRegex(RunnerError, "must be default, only, or all"):
            rust_test_runner.validate_filtering_args(["--run-ignored", "sometimes"])


class GenericRecipeGuardTest(unittest.TestCase):
    def test_cli_guard_normalizes_attached_equals_before_package_ownership_check(self):
        for package, expected_code in (("codex-core", 2), ("codex-tui", 0)):
            with (
                self.subTest(package=package),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(
                    rust_test_runner.main(["_guard-generic", "--", f"-p={package}"]),
                    expected_code,
                )
                if expected_code:
                    self.assertIn("cannot select codex-core", stderr.getvalue())
                else:
                    self.assertEqual(stderr.getvalue(), "")

    def test_every_codex_core_package_spelling_is_rejected(self) -> None:
        for argv in (
            ["-p", "codex-core"],
            ["--package", "codex-core"],
            ["--package=codex-core"],
            ["-pcodex-core"],
            ["-p=codex-core"],
            ["-p=codex-core@0.0.0"],
            ["-p=codex-*"],
            ["--workspace"],
            ["--all"],
            ["-p", "codex-*"],
            ["--package=codex-c?re"],
            ["-p", "codex-core@0.0.0"],
            ["--no-fail-fast", "-p", "codex-core", "-E", "test(x)"],
        ):
            with (
                self.subTest(argv=argv),
                self.assertRaisesRegex(RunnerError, "cannot select codex-core"),
            ):
                rust_test_runner.guard_generic_recipe_args(argv)

    def test_other_packages_are_allowed(self) -> None:
        rust_test_runner.guard_generic_recipe_args(["-p", "codex-tui"])
        rust_test_runner.guard_generic_recipe_args(["--package=codex-app-server"])
        rust_test_runner.guard_generic_recipe_args(["-p=codex-tui@0.0.0"])

    def test_qualified_package_ids_require_plain_names(self) -> None:
        for spec in (
            "path+file:///repo/core#codex-core@0.0.0",
            "path+file:///repo/core#0.0.0",
            "https://github.com/openai/codex#codex-core@0.0.0",
            "codex-core:0.0.0",
        ):
            with (
                self.subTest(spec=spec),
                self.assertRaisesRegex(RunnerError, "unsupported package selection"),
            ):
                rust_test_runner.guard_generic_recipe_args(["-p", spec])

    def test_guard_is_token_aware(self) -> None:
        # `codex-core` inside another option's value is not a package selection.
        rust_test_runner.guard_generic_recipe_args(
            ["-p", "codex-tui", "-E", "package(codex-core)"]
        )
        # Everything after `--` is a libtest filter, not a Cargo option.
        rust_test_runner.guard_generic_recipe_args(
            ["-p", "codex-tui", "--", "-p", "codex-core"]
        )

    def test_implicit_workspace_selection_is_rejected(self) -> None:
        for argv in ([], ["--", "-p", "codex-tui"], ["-E", "package(codex-tui)"]):
            with (
                self.subTest(argv=argv),
                self.assertRaisesRegex(RunnerError, "explicit -p/--package"),
            ):
                rust_test_runner.guard_generic_recipe_args(argv)


class NextestListParsingTest(unittest.TestCase):
    def test_ignored_state_is_preserved(self) -> None:
        payload = nextest_list_payload({"a::b": False, "a::c": True})
        self.assertEqual(
            rust_test_runner.parse_nextest_list(payload), {"a::b": False, "a::c": True}
        )

    def test_invalid_json_is_rejected(self) -> None:
        with self.assertRaisesRegex(RunnerError, "invalid JSON"):
            rust_test_runner.parse_nextest_list("not json")

    def test_declared_count_mismatch_is_rejected(self) -> None:
        payload = json.loads(nextest_list_payload({"a::b": False}))
        payload["test-count"] = 7
        with self.assertRaisesRegex(RunnerError, "does not match parsed count"):
            rust_test_runner.parse_nextest_list(json.dumps(payload))

    def test_only_filter_matches_are_returned_as_selected(self) -> None:
        payload = json.loads(
            nextest_list_payload({"a::selected": False, "a::other": True})
        )
        testcases = payload["rust-suites"]["codex-core::fixture"]["testcases"]
        testcases["a::selected"]["filter-match"] = {"status": "matches"}
        testcases["a::other"]["filter-match"] = {
            "status": "mismatch",
            "reason": "string",
        }

        self.assertEqual(
            rust_test_runner.parse_nextest_list(json.dumps(payload)),
            {"a::selected": False},
        )


class HelperUnionTest(RunnerTestCase):
    def test_target_helpers_are_exactly_the_declared_set(self) -> None:
        runner, _ = self.runner()
        self.assertEqual(
            runner.plan("core_all")["helpers"],
            ["codex", "codex-code-mode-host", "test_stdio_server"],
        )

    def test_gate_helpers_are_the_deduplicated_union_of_its_steps(self) -> None:
        runner, _ = self.runner()
        self.assertEqual(
            runner.plan("demo-gate")["helpers"],
            [
                "codex",
                "codex-command-runner",
                "codex-code-mode-host",
                "test_stdio_server",
            ],
        )

    def test_platform_scoped_helpers_are_dropped_off_platform(self) -> None:
        runner, _ = self.runner(platform="linux")
        self.assertEqual(runner.plan("core_lib")["helpers"], ["codex"])


class TargetDirectoryPropagationTest(RunnerTestCase):
    def test_every_cargo_command_targets_the_active_lane(self) -> None:
        runner, _ = self.runner()
        plan = runner.plan("core_all")
        expected = str(self.target_dir.resolve())

        self.assertEqual(plan["target_dir"], expected)
        for command in [plan["run"], *plan["builds"]]:
            with self.subTest(command=command):
                self.assertIn("--target-dir", command)
                self.assertEqual(command[command.index("--target-dir") + 1], expected)

    def test_metadata_target_directory_is_the_default(self) -> None:
        runner, _ = self.runner(target_dir=None)
        self.assertEqual(
            runner.plan("core_all")["target_dir"],
            str((self.temp_dir / "target").resolve()),
        )


class RunTargetTest(RunnerTestCase):
    def test_cross_package_helpers_build_once_before_exact_run_without_discovery(
        self,
    ) -> None:
        executor = self.build_executor()
        runner, _ = self.runner(executor=executor)
        expression = "test(=suite::one) | test(=suite::two)"

        runner.run_target("core_all", ["-E", expression], no_fail_fast=True)

        self.assertEqual(len(executor.calls), 2)
        build, run = executor.calls
        self.assertEqual(build["args"][:2], ["cargo", "build"])
        self.assertEqual(run["args"][:3], ["cargo", "nextest", "run"])
        self.assertEqual(
            [
                build["args"][i + 1]
                for i, arg in enumerate(build["args"])
                if arg == "-p"
            ],
            ["codex-cli", "codex-code-mode-host", "codex-rmcp-client"],
        )
        self.assertEqual(run["args"][run["args"].index("-E") + 1], expression)
        self.assertEqual(run["args"][run["args"].index("--test") + 1], "all")
        self.assertIn("--no-tests=fail", run["args"])
        self.assertIn("--no-fail-fast", run["args"])
        for name, executable in executor.artifacts.items():
            for alias in (name, name.replace("-", "_")):
                self.assertEqual(
                    run["env"][f"CARGO_BIN_EXE_{alias}"], str(executable.resolve())
                )
        for call in executor.calls:
            self.assertEqual(
                call["args"][call["args"].index("--target-dir") + 1],
                str(self.target_dir.resolve()),
            )

    def test_no_tests_run_failure_is_propagated_without_discovery(self) -> None:
        executor = self.build_executor(default_listing={})
        runner, _ = self.runner(executor=executor)

        def execute(args: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
            result = executor(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"]:
                # Model Nextest rejecting an empty selection only when the
                # runner supplied its mandatory failure policy.
                result.returncode = 4 if "--no-tests=fail" in args else 0
            return result

        runner.executor = execute
        with self.assertRaisesRegex(RunnerError, "command failed"):
            runner.run_target("core_all", ["-E", "test(=missing::test)"])
        self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
        self.assertEqual(len(executor.commands(["cargo", "build"])), 1)
        runs = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(runs), 1)
        self.assertEqual(runs[0][runs[0].index("-E") + 1], "test(=missing::test)")

    def test_same_package_helpers_share_a_build_and_export_every_artifact(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        names = ["test_stdio_server", "test_streamable_http_server"]
        data["targets"]["core_shard"]["helpers"] = names
        executor = FakeExecutor(
            artifacts={name: self.helper_executable(name) for name in names}
        )
        runner, _ = self.runner(executor=executor, manifest=Manifest.from_data(data))
        plan = runner.plan("core_shard")
        runner.run_target("core_shard", [])
        builds = executor.commands(["cargo", "build"])
        self.assertEqual(len(builds), 1)
        self.assertEqual(plan["builds"], builds)
        self.assertEqual(builds[0][builds[0].index("-p") + 1], "codex-rmcp-client")
        self.assertEqual(
            {
                builds[0][index + 1]
                for index, arg in enumerate(builds[0])
                if arg == "--bin"
            },
            set(names),
        )
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)
        for name in names:
            self.assertEqual(
                executor.last_env()[f"CARGO_BIN_EXE_{name}"],
                str(executor.artifacts[name].resolve()),
            )

    def test_grouped_helper_build_rejects_a_missing_second_artifact(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_shard"]["helpers"] = [
            "test_stdio_server",
            "test_streamable_http_server",
        ]
        executor = FakeExecutor(
            artifacts={"test_stdio_server": self.helper_executable("test_stdio_server")}
        )
        runner, _ = self.runner(executor=executor, manifest=Manifest.from_data(data))
        with self.assertRaisesRegex(
            RunnerError, "codex-rmcp-client/test_streamable_http_server"
        ):
            runner.run_target("core_shard", [])
        self.assertEqual(len(executor.commands(["cargo", "build"])), 1)
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_unfiltered_core_lib_is_rejected_before_metadata_or_builds(self) -> None:
        for arguments in [
            [],
            ["--no-fail-fast"],
            ["--run-ignored", "all"],
            ["--run-ignored", "all", "--", "--skip", "slow"],
        ]:
            with (
                self.subTest(arguments=arguments),
                mock.patch.object(rust_test_runner, "load_metadata") as metadata,
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(
                    rust_test_runner.main(["run-target", "core_lib", *arguments]), 2
                )
                metadata.assert_not_called()
                self.assertIn(
                    "core_lib requires an explicit test filter", stderr.getvalue()
                )

    def test_core_lib_filter_and_explicit_all_reach_the_run(self) -> None:
        for arguments in [
            ["-E", "test(alpha)"],
            ["tests::alpha"],
            ["--all"],
            ["--all", "--no-fail-fast"],
        ]:
            with (
                self.subTest(arguments=arguments),
                mock.patch.object(rust_test_runner, "load_metadata"),
                mock.patch.object(rust_test_runner, "RustTestRunner") as runner,
            ):
                self.assertEqual(
                    rust_test_runner.main(["run-target", "core_lib", *arguments]), 0
                )
                forwarded = [
                    arg for arg in arguments if arg not in {"--all", "--no-fail-fast"}
                ]
                runner.return_value.run_target.assert_called_once_with(
                    "core_lib", forwarded, allow_all="--all" in arguments
                )

    def test_direct_runner_also_rejects_unfiltered_core_lib(self) -> None:
        runner, executor = self.runner()
        with self.assertRaisesRegex(
            RunnerError, "core_lib requires an explicit test filter"
        ):
            runner.run_target("core_lib", [])
        self.assertEqual(executor.calls, [])

    def build_executor(self, **kwargs: Any) -> FakeExecutor:
        artifacts = {
            name: self.helper_executable(name)
            for name in ("codex", "codex-code-mode-host", "test_stdio_server")
        }
        return FakeExecutor(artifacts=artifacts, **kwargs)

    def test_run_forces_no_tests_fail(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", ["-E", "test(=tests::alpha)"])
        for command in executor.commands(["cargo", "nextest"]):
            self.assertEqual(command[command.index("-E") + 1], "test(=tests::alpha)")
        run_commands = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(run_commands), 1)
        self.assertIn("--no-tests=fail", run_commands[0])
        self.assertEqual(
            run_commands[0][run_commands[0].index("--show-progress") + 1], "none"
        )
        self.assertEqual(
            run_commands[0][run_commands[0].index("--success-output") + 1], "never"
        )

    def test_local_run_can_preserve_no_fail_fast_behavior(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [], no_fail_fast=True)
        run_commands = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(run_commands), 1)
        self.assertIn("--no-fail-fast", run_commands[0])

    def test_run_streams_its_output_instead_of_capturing_it(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [])
        captures = {
            call["args"][1]: call["capture"]
            for call in executor.calls
            if call["args"][0] == "cargo"
        }
        # Only the helper build's machine-readable stdout is parsed; its
        # progress and diagnostics stay on the terminal.
        self.assertEqual(
            captures,
            {
                "build": rust_test_runner.CAPTURE_STDOUT,
                "nextest": rust_test_runner.CAPTURE_NONE,
            },
        )

    def test_one_cargo_invocation_selects_every_declared_helper_binary(self) -> None:
        # The merged build must still name each declared binary, so widening
        # the package list cannot quietly pull in a package's other binaries.
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [])
        builds = executor.commands(["cargo", "build"])
        self.assertEqual(len(builds), 1)
        self.assertEqual(
            [
                builds[0][index + 1]
                for index, arg in enumerate(builds[0])
                if arg == "--bin"
            ],
            ["codex", "codex-code-mode-host", "test_stdio_server"],
        )

    def test_helper_environment_exports_dashed_and_underscored_aliases(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [])
        env = executor.last_env()

        host = str(self.helper_executable("codex-code-mode-host").resolve())
        self.assertEqual(env["CARGO_BIN_EXE_codex-code-mode-host"], host)
        self.assertEqual(env["CARGO_BIN_EXE_codex_code_mode_host"], host)
        self.assertEqual(
            env["CARGO_BIN_EXE_test_stdio_server"],
            str(self.helper_executable("test_stdio_server").resolve()),
        )

    def test_only_declared_helpers_are_built(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_shard", [])
        built = [
            command[command.index("--bin") + 1]
            for command in executor.commands(["cargo", "build"])
        ]
        self.assertEqual(built, ["codex"])

    def test_missing_helper_artifact_fails(self) -> None:
        # `codex-code-mode-host` produces no `compiler-artifact` message.
        artifacts = {
            name: self.helper_executable(name)
            for name in ("codex", "test_stdio_server")
        }
        runner, _ = self.runner(executor=FakeExecutor(artifacts=artifacts))
        with self.assertRaisesRegex(
            RunnerError, "did not produce exactly one executable"
        ):
            runner.run_target("core_all", [])

    def test_caller_cannot_widen_the_named_selection(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        with self.assertRaisesRegex(RunnerError, "cannot override a named target"):
            runner.run_target("core_all", ["-p", "codex-tui"])
        self.assertEqual(executor.calls, [])


class RunGateTest(RunnerTestCase):
    def test_cli_deadline_covers_metadata_and_gate_commands(self) -> None:
        executor = self.gate_executor(self.matching_listings())
        runners = []

        def construct(manifest, metadata, **kwargs):
            runner = RustTestRunner(manifest, metadata, executor=executor, **kwargs)
            runners.append(runner)
            return runner

        with (
            mock.patch.object(
                rust_test_runner.Manifest, "load", return_value=self.manifest()
            ),
            mock.patch.object(
                rust_test_runner, "load_metadata", return_value=self.metadata()
            ) as metadata,
            mock.patch.object(
                rust_test_runner, "RustTestRunner", side_effect=construct
            ),
        ):
            self.assertEqual(
                rust_test_runner.main(
                    ["run-gate", "--command-timeout-seconds", "23.5", "demo-gate"]
                ),
                0,
            )
        metadata.assert_called_once_with(command_timeout_seconds=23.5)
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 2)
        self.assertEqual(executor.last_env()["CODEX_RUST_TEST_TIMEOUT_SECS"], "23.5")
        self.assertEqual(runners[0].base_env["CODEX_RUST_TEST_TIMEOUT_SECS"], "23.5")

    def test_cancellation_and_deadline_do_not_launch_later_groups(self) -> None:
        for outcome in ("cancelled", "timed_out"):
            with self.subTest(outcome=outcome):
                executor = self.gate_executor(self.matching_listings())
                runner, _ = self.runner(executor=executor)
                original = runner._checked
                commands = []

                def stop_first_run(args, **kwargs):
                    if args[:3] == ["cargo", "nextest", "run"]:
                        commands.append(args)
                        raise RunnerError("requested stop", outcome=outcome)
                    return original(args, **kwargs)

                with mock.patch.object(runner, "_checked", side_effect=stop_first_run):
                    with self.assertRaises(RunnerError) as raised:
                        runner.run_gates(["demo-gate"], quiet=True)
                self.assertEqual(raised.exception.outcome, outcome)
                self.assertEqual(len(commands), 1)
                self.assertIn("--lib", commands[0])

    def test_repository_core_gates_build_only_their_required_helpers(self) -> None:
        manifest = Manifest.load(
            REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        )
        cases = {
            "windows-sandbox-core-exec": [],
            "core-stdio-helper-regressions": ["test_stdio_server"],
            "capability-known-delta-store": [],
            "capability-command-output-artifacts": [],
        }
        for gate_name, expected_helpers in cases.items():
            with self.subTest(gate=gate_name):
                tests = {
                    test: False
                    for step in manifest.gates[gate_name].steps
                    for test in step.tests
                }
                executor = FakeExecutor(
                    artifacts={
                        name: self.helper_executable(name) for name in expected_helpers
                    },
                    default_listing=tests,
                )
                runner, _ = self.runner(manifest=manifest, executor=executor)
                runner.run_gates([gate_name])
                builds = executor.commands(["cargo", "build"])
                built_helpers = [
                    command[index + 1]
                    for command in builds
                    for index, arg in enumerate(command)
                    if arg == "--bin"
                ]
                self.assertEqual(built_helpers, expected_helpers)
                self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)
                self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])

    def test_batch_cannot_hide_an_explicit_filter_selecting_another_steps_test(self):
        for same_gate in (False, True):
            for explicit_second in (False, True):
                with self.subTest(same_gate=same_gate, explicit_second=explicit_second):
                    data = copy.deepcopy(MANIFEST_DATA)
                    first = {
                        "target": "core_lib",
                        "filter": "test(alpha) | test(beta)",
                        "tests": ["alpha"],
                        "helpers": [],
                    }
                    second = {"target": "core_lib", "tests": ["beta"], "helpers": []}
                    if explicit_second:
                        second["filter"] = "test(beta)"
                    data["gates"] = {"a": {"steps": [first]}}
                    if same_gate:
                        data["gates"]["a"]["steps"].append(second)
                    else:
                        data["gates"]["b"] = {"steps": [second]}
                    executor = FakeExecutor()

                    def listing(args):
                        expression = args[args.index("-E") + 1]
                        return {
                            name: False
                            for name in ("alpha", "beta")
                            if name in expression
                        }

                    executor._listing_for = listing
                    runner, _ = self.runner(
                        manifest=Manifest.from_data(data), executor=executor
                    )
                    with self.assertRaisesRegex(RunnerError, r"unexpected=\['beta'\]"):
                        runner.run_gates(list(data["gates"]), quiet=True)
                    # The normal CLI must enforce the same contract without
                    # requiring a separate check-gates command.
                    stderr = io.StringIO()
                    with (
                        mock.patch.object(
                            Manifest, "load", return_value=runner.manifest
                        ),
                        mock.patch.object(
                            rust_test_runner,
                            "load_metadata",
                            return_value=self.metadata(),
                        ),
                        mock.patch.object(
                            rust_test_runner, "RustTestRunner", return_value=runner
                        ),
                        contextlib.redirect_stderr(stderr),
                    ):
                        self.assertEqual(
                            rust_test_runner.main(["run-gate", *data["gates"]]), 2
                        )
                    self.assertIn("unexpected=['beta']", stderr.getvalue())
                    self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])
                    self.assertEqual(executor.commands(["cargo", "build"]), [])

                    # Once the explicit contract matches its filter, discovery
                    # stays separate while execution still runs the union once.
                    first["tests"] = ["alpha", "beta"]
                    executor.calls.clear()
                    runner, _ = self.runner(
                        manifest=Manifest.from_data(data), executor=executor
                    )
                    expected = {"a": ["alpha", "beta"]}
                    if not same_gate:
                        expected["b"] = ["beta"]
                    self.assertEqual(
                        runner.run_gates(list(data["gates"]), quiet=True), expected
                    )
                    self.assertEqual(
                        len(executor.commands(["cargo", "nextest", "list"])),
                        2 if explicit_second else 1,
                    )
                    self.assertEqual(
                        len(executor.commands(["cargo", "nextest", "run"])), 1
                    )

    def test_failure_preserves_both_streams_and_recovers_large_output(self) -> None:
        for large in (False, True):
            with self.subTest(large=large):
                stdout = (
                    "stdout start\n"
                    + ("output\n" * 2000 if large else "")
                    + "stdout end\n"
                )
                stderr = (
                    "stderr start\n"
                    + ("errors\n" * 2000 if large else "")
                    + "stderr end\n"
                )
                executor = self.gate_executor(self.matching_listings())
                executor.failing_runs = {"all"}
                original = executor.__call__

                def execute(
                    args: list[str],
                    *,
                    invoke: Any = original,
                    streams: tuple[str, str] = (stdout, stderr),
                    **kwargs: Any,
                ) -> subprocess.CompletedProcess[str]:
                    result = invoke(args, **kwargs)
                    if args[:3] == ["cargo", "nextest", "run"] and "all" in args:
                        result.stdout, result.stderr = streams
                    return result

                runner, _ = self.runner(executor=executor)
                runner.executor = execute
                with self.assertRaises(RunnerError) as raised:
                    runner.run_gate("demo-gate")
                detail = str(raised.exception)
                for marker in (
                    "stdout start",
                    "stdout end",
                    "stderr start",
                    "stderr end",
                ):
                    self.assertIn(marker, detail)
                logs = list((runner.target_dir / "test-runner-logs").glob("*.log"))
                if large:
                    self.assertLess(len(detail), 10000)
                    self.assertEqual(len(logs), 1)
                    self.assertIn(str(logs[0]), detail)
                    self.assertEqual(
                        logs[0].read_text(encoding="utf-8"),
                        f"stdout:\n{stdout}\nstderr:\n{stderr}",
                    )
                else:
                    self.assertEqual(logs, [])

    def test_zero_selected_tests_fails_before_anything_is_built(self) -> None:
        executor = self.gate_executor({"--lib": {}})
        runner, _ = self.runner(executor=executor)
        with self.assertRaisesRegex(RunnerError, "selected zero tests") as raised:
            runner.check_gates(["demo-gate"])
        self.assertEqual(raised.exception.outcome, "zero_tests")
        self.assertEqual(executor.commands(["cargo", "build"]), [])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_discovery_streams_build_progress_while_parsing_its_listing(self) -> None:
        runner, executor = self.runner(
            executor=self.gate_executor(self.matching_listings())
        )
        runner.run_gates(["demo-gate"], quiet=True, discover=True)
        captures = {
            tuple(call["args"][1:3]): call["capture"]
            for call in executor.calls
            if call["args"][0] == "cargo"
        }
        self.assertEqual(
            captures,
            {
                ("nextest", "list"): rust_test_runner.CAPTURE_STDOUT,
                ("build", "--message-format=json-render-diagnostics"): (
                    rust_test_runner.CAPTURE_STDOUT
                ),
                # Gate completion evidence is parsed out of the run's own
                # status lines, so this one stream stays captured.
                ("nextest", "run"): rust_test_runner.CAPTURE_BOTH,
            },
        )

    def test_successful_gate_reports_counts_after_validating_completed_tests(
        self,
    ) -> None:
        runner, executor = self.runner(
            executor=self.gate_executor(self.matching_listings())
        )
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            runner.run_gates(["demo-gate"])
        self.assertEqual(
            output.getvalue(), "gate core_lib: 1 passed\ngate core_all: 1 passed\n"
        )
        for command in executor.commands(["cargo", "nextest", "run"]):
            self.assertEqual(command[command.index("--status-level") + 1], "pass")

    def test_failed_gate_targets_finish_the_batch_and_report_every_failure(
        self,
    ) -> None:
        for failed in ({"--lib"}, {"--lib", "all"}):
            with self.subTest(failed=failed):
                executor = self.gate_executor(self.matching_listings())
                executor.failing_runs = failed
                runner, _ = self.runner(executor=executor)
                with self.assertRaises(RunnerError) as raised:
                    runner.run_gate("demo-gate")
                runs = executor.commands(["cargo", "nextest", "run"])
                self.assertEqual(
                    [executor._selector_for(run) for run in runs], ["--lib", "all"]
                )
                for selector in failed:
                    self.assertIn(f"failed {selector}", str(raised.exception))
                self.assertEqual(raised.exception.outcome, "failed")

    def test_missing_completion_evidence_still_runs_later_targets(self) -> None:
        executor = self.gate_executor(self.matching_listings())
        original = executor._stdout
        executor._stdout = lambda args: (
            ""
            if args[:3] == ["cargo", "nextest", "run"] and "--lib" in args
            else original(args)
        )
        runner, _ = self.runner(executor=executor)
        with self.assertRaisesRegex(RunnerError, "passed exactly once") as raised:
            runner.run_gate("demo-gate")
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 2)
        self.assertEqual(raised.exception.outcome, "not_executed")

    def test_failed_execution_groups_are_collected_before_returning_failure(
        self,
    ) -> None:
        for failing in ({"--lib"}, {"--lib", "all"}):
            with self.subTest(failing=failing):
                executor = self.gate_executor(self.matching_listings())
                executor.failing_runs = failing
                runner, _ = self.runner(executor=executor)
                with self.assertRaises(RunnerError) as caught:
                    runner.run_gates(["demo-gate"], quiet=True)
                runs = executor.commands(["cargo", "nextest", "run"])
                self.assertEqual(
                    [executor._selector_for(command) for command in runs],
                    ["--lib", "all"],
                )
                for selector in failing:
                    self.assertIn(f"failed {selector}", str(caught.exception))

    def test_binary_gate_runs_the_named_binary(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["cli"] = {"package": "codex-cli", "bin": "codex", "helpers": []}
        data["gates"] = {
            "cli-proof": {"steps": [{"target": "cli", "tests": ["tests::parse"]}]}
        }
        executor = FakeExecutor(default_listing={"tests::parse": False})
        runner = RustTestRunner(
            Manifest.from_data(data), self.metadata(), executor=executor
        )
        self.assertEqual(
            runner.run_gates(["cli-proof"], quiet=True), {"cli-proof": ["tests::parse"]}
        )
        command = executor.commands(["cargo", "nextest", "run"])[0]
        self.assertEqual(command[command.index("--bin") + 1], "codex")
        self.assertNotIn("--lib", command)
        self.assertEqual(executor.commands(["cargo", "build"]), [])

    def test_cli_batch_deduplicates_tests_and_prepares_only_required_helpers(
        self,
    ) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"] = {
            "pure": {
                "steps": [
                    {"target": "core_lib", "tests": ["tests::alpha"], "helpers": []}
                ]
            },
            "runtime": {
                "steps": [
                    {
                        "target": "core_lib",
                        "tests": ["tests::alpha", "tests::beta"],
                        "helpers": ["codex"],
                    }
                ]
            },
        }
        executor = self.gate_executor(
            {"--lib": {"tests::alpha": False, "tests::beta": False}}
        )
        runner = RustTestRunner(
            Manifest.from_data(data),
            self.metadata(),
            executor=executor,
            env={"INSTA_UPDATE": "always"},
            platform="windows",
        )
        with (
            mock.patch.object(
                rust_test_runner.Manifest, "load", return_value=runner.manifest
            ),
            mock.patch.object(
                rust_test_runner, "load_metadata", return_value=self.metadata()
            ) as metadata,
            mock.patch.object(rust_test_runner, "RustTestRunner", return_value=runner),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(
                rust_test_runner.main(["run-gate", "pure", "runtime", "pure"]), 0
            )
        metadata.assert_called_once_with()
        self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
        runs = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(runs), 1)
        self.assertIn("test(=tests::alpha)", runs[0][runs[0].index("-E") + 1])
        self.assertIn("test(=tests::beta)", runs[0][runs[0].index("-E") + 1])
        self.assertEqual(
            [
                cmd[cmd.index("--bin") + 1]
                for cmd in executor.commands(["cargo", "build"])
            ],
            ["codex"],
        )
        self.assertEqual(executor.last_env()["INSTA_UPDATE"], "no")

    def test_pure_gate_needs_no_helper_artifacts(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"] = {
            "pure": {
                "steps": [
                    {"target": "core_lib", "tests": ["tests::alpha"], "helpers": []}
                ]
            }
        }
        executor = FakeExecutor(default_listing={"tests::alpha": False})
        runner = RustTestRunner(
            Manifest.from_data(data), self.metadata(), executor=executor
        )
        self.assertEqual(
            runner.run_gates(["pure"], quiet=True), {"pure": ["tests::alpha"]}
        )
        self.assertEqual(executor.commands(["cargo", "build"]), [])

    def test_schema_gate_runs_declared_tests_without_building_helpers(self) -> None:
        manifest = Manifest.load(
            REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        )
        data = copy.deepcopy(MANIFEST_DATA)
        data["gates"] = {}
        focused = Manifest.from_data(data)
        focused.gates["config-schema-protocol"] = manifest.gates[
            "config-schema-protocol"
        ]
        expected = {
            "config::schema::tests::config_schema_matches_fixture": False,
            "config::schema::tests::config_schema_hides_unsupported_inline_mcp_bearer_token": False,
        }
        executor = FakeExecutor(default_listing=expected)
        runner, _ = self.runner(manifest=focused, executor=executor)
        self.assertEqual(
            runner.run_gates(["config-schema-protocol"], quiet=True),
            {
                "config-schema-protocol": sorted(expected),
            },
        )
        self.assertEqual(executor.commands(["cargo", "build"]), [])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)

    def test_ignored_required_test_fails_before_build_or_execution(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": True}
        executor = self.gate_executor(listings)
        runner, _ = self.runner(executor=executor)
        with self.assertRaisesRegex(RunnerError, "ignored tests"):
            runner.check_gates(["demo-gate"])
        self.assertEqual(executor.commands(["cargo", "build"]), [])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_normal_gate_uses_exact_ids_and_rejects_missing_test(self) -> None:
        # A successful binary with one remaining test must not hide a deleted
        # test; nor may a whole target disappear behind another target's PASS.
        for partial in (False, True):
            with self.subTest(partial=partial):
                data = copy.deepcopy(MANIFEST_DATA)
                for step in data["gates"]["demo-gate"]["steps"]:
                    step.pop("filter")
                executor = self.gate_executor(self.matching_listings())
                expected_filter = "test(=mod::tests::alpha)"
                if partial:
                    data["gates"]["demo-gate"]["steps"][0]["tests"].append(
                        "mod::tests::deleted"
                    )
                    expected_filter += " | test(=mod::tests::deleted)"
                else:
                    executor.listings["all"] = {}
                runner, _ = self.runner(
                    manifest=Manifest.from_data(data), executor=executor
                )
                with self.assertRaisesRegex(RunnerError, "passed exactly once"):
                    runner.run_gates(["demo-gate"], quiet=True)
                self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
                runs = executor.commands(["cargo", "nextest", "run"])
                self.assertEqual(len(runs), 2)
                self.assertEqual(runs[0][runs[0].index("-E") + 1], expected_filter)
                self.assertEqual(
                    runs[1][runs[1].index("-E") + 1], "test(=suite::mod::beta)"
                )

    def test_success_exit_without_exact_completed_results_is_rejected(self) -> None:
        for output in (
            "",
            "PASS [0.001s] fixture wrong::test",
            "SKIP [0.001s] fixture mod::tests::alpha",
            "PASS [0.001s] fixture mod::tests::alpha\n" * 2,
        ):
            with self.subTest(output=output):
                executor = self.gate_executor(self.matching_listings())
                original = executor._stdout
                executor._stdout = lambda args, output=output, original=original: (
                    output
                    if args[:3] == ["cargo", "nextest", "run"]
                    else original(args)
                )
                runner, _ = self.runner(executor=executor)
                with self.assertRaisesRegex(RunnerError, "passed exactly once"):
                    runner.run_gates(["demo-gate"], quiet=True)

    def test_missing_required_helper_prevents_test_execution(self) -> None:
        executor = FakeExecutor(listings=self.matching_listings())
        runner, _ = self.runner(executor=executor)
        with self.assertRaisesRegex(RunnerError, "exactly one executable artifact"):
            runner.run_gates(["demo-gate"])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def gate_executor(self, listings: dict[str, dict[str, bool]]) -> FakeExecutor:
        artifacts = {
            name: self.helper_executable(name)
            for name in (
                "codex",
                "codex-code-mode-host",
                "test_stdio_server",
                "codex-command-runner",
            )
        }
        return FakeExecutor(artifacts=artifacts, listings=listings)

    def matching_listings(self) -> dict[str, dict[str, bool]]:
        return {
            "--lib": {"mod::tests::alpha": False},
            "all": {"suite::mod::beta": False},
        }

    def test_matching_test_ids_run_every_step(self) -> None:
        executor = self.gate_executor(self.matching_listings())
        runner, _ = self.runner(executor=executor)
        runner.run_gate("demo-gate")
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 2)

    def test_missing_expected_test_id_fails_the_gate(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::unrelated": False}
        runner, executor = self.runner(executor=self.gate_executor(listings))
        with self.assertRaisesRegex(RunnerError, "wrong test-ID set"):
            runner.check_gates(["demo-gate"])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_unexpected_test_id_fails_the_gate(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        runner, _ = self.runner(executor=self.gate_executor(listings))
        with self.assertRaisesRegex(RunnerError, r"unexpected=\['suite::mod::extra'\]"):
            runner.check_gates(["demo-gate"])

    def test_gate_verifies_every_step_before_running_any(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        executor = self.gate_executor(listings)
        runner, _ = self.runner(executor=executor)
        with self.assertRaises(RunnerError):
            runner.check_gates(["demo-gate"])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])
        self.assertEqual(executor.commands(["cargo", "build"]), [])


class ParityTest(RunnerTestCase):
    def parity_executor(
        self,
        listings: dict[str, dict[str, bool]],
        *,
        failing_runs: set[str] | None = None,
    ) -> FakeExecutor:
        artifacts = {
            name: self.helper_executable(name)
            for name in ("codex", "codex-code-mode-host", "test_stdio_server")
        }
        return FakeExecutor(
            artifacts=artifacts,
            listings=listings,
            failing_runs=failing_runs,
        )

    def test_identical_inventories_pass_and_run_both_sides(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": True},
            "core_shard": {"suite::a::one": False, "suite::b::two": True},
        }
        executor = self.parity_executor(listings)
        runner, _ = self.runner(executor=executor)
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs_root = Path(temp_dir) / "codex-rs"
            snapshots = codex_rs_root / "core" / "tests" / "suite" / "snapshots"
            snapshots.mkdir(parents=True)
            approved = {
                snapshots
                / "all__suite__a__one.snap": "---\nsource: old.rs\nexpression: report\n---\nTOTAL: 5\n",
                snapshots
                / "core_shard__suite__a__one.snap": "---\nsource: new.rs\nexpression: report\n---\nTOTAL: 5\n",
            }
            for path, content in approved.items():
                path.write_text(content, encoding="utf-8")
            with mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root):
                runner.parity("core_all", ["core_shard"])
            for path, content in approved.items():
                self.assertEqual(path.read_text(encoding="utf-8"), content)
        list_commands = executor.commands(["cargo", "nextest", "list"])
        self.assertEqual(len(list_commands), 2)
        for command in list_commands:
            self.assertIn("--ignore-default-filter", command)
            self.assertEqual(command[command.index("--run-ignored") + 1], "all")

        run_commands = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(run_commands), 2)
        for command in run_commands:
            self.assertIn("--no-fail-fast", command)
            self.assertEqual(command[command.index("--retries") + 1], "0")
            self.assertEqual(command[command.index("--run-ignored") + 1], "default")
        run_calls = [
            call
            for call in executor.calls
            if call["args"][:3] == ["cargo", "nextest", "run"]
        ]
        self.assertEqual(run_calls[0]["env"]["INSTA_UPDATE"], "no")
        self.assertEqual(run_calls[1]["env"]["INSTA_UPDATE"], "no")

    def test_snapshot_content_change_fails_after_behavior_runs(self) -> None:
        listings = {
            "all": {"suite::a::one": False},
            "core_shard": {"suite::a::one": False},
        }
        executor = self.parity_executor(listings)
        runner, _ = self.runner(executor=executor)
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs_root = Path(temp_dir) / "codex-rs"
            snapshots = codex_rs_root / "core" / "tests" / "suite" / "snapshots"
            snapshots.mkdir(parents=True)
            (snapshots / "all__suite__a__one.snap").write_text("legacy")
            (snapshots / "core_shard__suite__a__one.snap").write_text("replacement")
            with (
                mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root),
                self.assertRaisesRegex(RunnerError, "content_changes"),
            ):
                runner.parity("core_all", ["core_shard"])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 2)

    def test_behavior_failures_are_reported_after_every_target_runs(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
            "core_shard_two": {"suite::b::two": False},
        }
        executor = self.parity_executor(
            listings,
            failing_runs={"all", "core_shard_two"},
        )
        runner, _ = self.runner(executor=executor)
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs_root = Path(temp_dir) / "codex-rs"
            with (
                mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root),
                self.assertRaisesRegex(
                    RunnerError, "(?s)every target.*core_all:.*core_shard_two:"
                ),
            ):
                runner.parity(
                    "core_all",
                    ["core_shard", "core_shard_two"],
                )
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 3)

    def test_missing_or_duplicate_snapshot_counterpart_fails(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
            "core_shard_two": {"suite::b::two": False},
        }
        runner, _ = self.runner(executor=self.parity_executor(listings))
        with tempfile.TemporaryDirectory() as temp_dir:
            codex_rs_root = Path(temp_dir) / "codex-rs"
            snapshots = codex_rs_root / "core" / "tests" / "suite" / "snapshots"
            snapshots.mkdir(parents=True)
            (snapshots / "all__suite__a__one.snap").write_text("same")
            (snapshots / "all__suite__b__two.snap").write_text("same")
            (snapshots / "core_shard__suite__a__one.snap").write_text("same")
            (snapshots / "core_shard_two__suite__a__one.snap").write_text("same")
            with (
                mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root),
                self.assertRaisesRegex(
                    RunnerError,
                    "missing=\\['suite__b__two\\.snap'\\].*duplicates=\\['suite__a__one\\.snap'\\]",
                ),
            ):
                runner.parity("core_all", ["core_shard", "core_shard_two"])

    def test_missing_test_fails_parity(self) -> None:
        listings = {
            "all": {"suite::a::one": False, "suite::b::two": False},
            "core_shard": {"suite::a::one": False},
        }
        runner, executor = self.runner(executor=self.parity_executor(listings))
        with self.assertRaisesRegex(RunnerError, r"missing=\['suite::b::two'\]"):
            runner.parity("core_all", ["core_shard"])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_ignored_state_change_fails_parity(self) -> None:
        listings = {
            "all": {"suite::a::one": True},
            "core_shard": {"suite::a::one": False},
        }
        runner, _ = self.runner(executor=self.parity_executor(listings))
        with self.assertRaisesRegex(RunnerError, "ignored_state_changes"):
            runner.parity("core_all", ["core_shard"])

    def test_legacy_target_cannot_also_be_a_replacement(self) -> None:
        runner, _ = self.runner(executor=self.parity_executor({}))
        with self.assertRaisesRegex(RunnerError, "cannot also be a replacement"):
            runner.parity("core_all", ["core_all"])


class RepositoryManifestTest(unittest.TestCase):
    """The checked-in manifest must satisfy the runner's own schema."""

    def setUp(self) -> None:
        self.manifest = Manifest.load(
            REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        )

    def test_repository_gates_use_exact_ids_and_explicit_core_helpers(self) -> None:
        for gate in self.manifest.gates.values():
            for step in gate.steps:
                with self.subTest(gate=gate.name, target=step.target):
                    self.assertIsNone(
                        step.filterset,
                        "redundant filters force discovery before normal gate execution",
                    )
                    if step.target == "core_lib":
                        self.assertIsNotNone(
                            step.helpers,
                            "fixed core gates must not inherit every helper binary",
                        )

    def test_manifest_parses_strictly(self) -> None:
        self.assertEqual(self.manifest.version, rust_test_runner.SCHEMA_VERSION)
        self.assertIn("core_lib", self.manifest.targets)
        self.assertNotIn("core_all", self.manifest.targets)
        self.assertTrue(
            {
                "core_cli_workspace",
                "core_code_mode_mcp",
                "core_exec_permissions",
                "core_thread_state",
                "core_transport_telemetry",
                "core_agents_review",
                "core_model_prompt_runtime",
                "core_windows",
            }.issubset(self.manifest.targets)
        )

    def test_every_declared_test_target_has_a_source_file(self) -> None:
        tests_dir = REPO_ROOT / "codex-rs" / "core" / "tests"
        for target in self.manifest.targets.values():
            if target.package != "codex-core" or target.selector_kind != "test":
                continue
            with self.subTest(target=target.name):
                self.assertTrue(
                    (tests_dir / f"{target.selector_value}.rs").is_file(),
                    f"{target.selector_value}.rs is declared but missing",
                )

    def test_every_gate_step_names_a_declared_target(self) -> None:
        for gate in self.manifest.gates.values():
            for step in gate.steps:
                with self.subTest(gate=gate.name, target=step.target):
                    self.assertIn(step.target, self.manifest.targets)

    def test_shard_modules_and_helpers_match_migration_contract(self) -> None:
        expected_modules = {
            "core_cli_workspace": [
                "agents_md",
                "cli_stream",
                "deprecation_notice",
                "live_cli",
                "remote_env",
                "user_shell_cmd",
            ],
            "core_code_mode_mcp": [
                "code_mode",
                "code_mode_elicitation",
                "mcp_auth_elicitation",
                "mcp_auth_refresh",
                "mcp_refresh_cleanup",
                "mcp_tool_exposure",
                "rmcp_client",
            ],
            "core_exec_permissions": [
                "apply_patch_cli",
                "approvals",
                "exec_policy",
                "extension_sandbox",
                "permissions_messages",
                "request_permissions",
                "safety_check_downgrade",
                "shell_command",
                "shell_snapshot",
                "unified_exec",
                "unified_exec_process_events",
            ],
            "core_thread_state": [
                "compact",
                "compact_remote",
                "compact_resume_fork",
                "fork_thread",
                "pending_input",
                "resume",
                "resume_warning",
                "rollout_list_find",
                "sqlite_state",
                "stream_error_allows_next_turn",
                "stream_no_completed",
                "turn_state",
                "window_headers",
            ],
            "core_transport_telemetry": [
                "client",
                "client_websockets",
                "external_auth",
                "otel",
                "responses_api_proxy_headers",
                "responses_lite",
                "websocket_fallback",
            ],
            "core_agents_review": [
                "agent_execution",
                "agent_jobs",
                "agent_websocket",
                "codex_delegate",
                "collaboration_instructions",
                "investigation_evidence_schema",
                "multi_agent_mode",
                "request_user_input",
                "review",
                "subagent_notifications",
            ],
            "core_model_prompt_runtime": [
                "additional_context",
                "current_time_reminder",
                "image_rollout",
                "model_overrides",
                "model_runtime_selectors",
                "model_switching",
                "model_visible_layout",
                "models_cache_ttl",
                "override_updates",
                "personality",
                "prompt_caching",
                "prompt_debug_tests",
                "quota_exceeded",
                "safety_buffering",
                "web_search",
            ],
            "core_windows": ["hooks_windows", "windows_sandbox"],
        }
        expected_helpers = {
            target: ["codex", "codex-code-mode-host"] for target in expected_modules
        }
        expected_helpers["core_code_mode_mcp"] += [
            "test_stdio_server",
            "test_streamable_http_server",
        ]
        expected_helpers["core_thread_state"].append("test_stdio_server")
        expected_helpers["core_exec_permissions"] += [
            "codex-windows-sandbox-setup",
            "codex-command-runner",
        ]
        expected_helpers["core_windows"] += [
            "codex-windows-sandbox-setup",
            "codex-command-runner",
        ]

        tests_dir = REPO_ROOT / "codex-rs" / "core" / "tests"
        for target_name, modules in expected_modules.items():
            with self.subTest(target=target_name):
                source = (tests_dir / f"{target_name}.rs").read_text(encoding="utf-8")
                declared_modules = [
                    line.strip().removeprefix("mod ").removesuffix(";")
                    for line in source.splitlines()
                    if line.startswith("    mod ")
                ]
                self.assertEqual(declared_modules, modules)
                self.assertIn('include!("suite/prelude.rs");', source)
                self.assertEqual(
                    list(self.manifest.targets[target_name].helpers),
                    expected_helpers[target_name],
                )


class RunEnvironmentTest(RunnerTestCase):
    """The runner owns the child environment every Cargo command inherits."""

    def build_runner(self, **kwargs: Any) -> tuple[RustTestRunner, FakeExecutor]:
        executor = FakeExecutor(artifacts={"codex": self.helper_executable("codex")})
        runner = RustTestRunner(
            self.manifest(),
            self.metadata(),
            target_dir=self.target_dir,
            platform="windows",
            executor=executor,
            env={},
            **kwargs,
        )
        return runner, executor

    def test_cargo_incremental_is_never_introduced_by_the_runner(self) -> None:
        # `just-shell.py` points RUSTC_WRAPPER at sccache for every recipe, and
        # sccache aborts the build when CARGO_INCREMENTAL asks for incremental
        # compilation while refusing to honor it when it asks for "0". Only an
        # unset variable lets `.cargo/config.toml` grant workspace crates the
        # incremental cache these narrow runs depend on.
        cases: list[tuple[dict[str, str], str | None]] = [
            ({}, None),
            ({"RUSTC_WRAPPER": "sccache"}, None),
            ({"RUSTC_WORKSPACE_WRAPPER": "sccache"}, None),
            ({"RUSTC_WRAPPER": "sccache", "CARGO_INCREMENTAL": "0"}, "0"),
        ]
        for env, expected in cases:
            with self.subTest(env=env):
                executor = FakeExecutor(
                    artifacts={"codex": self.helper_executable("codex")}
                )
                runner = RustTestRunner(
                    self.manifest(),
                    self.metadata(),
                    target_dir=self.target_dir,
                    platform="windows",
                    executor=executor,
                    env=env,
                )
                runner.run_target("core_shard", [])
                for call in executor.calls:
                    self.assertEqual(
                        call["env"].get("CARGO_INCREMENTAL"),
                        expected,
                        msg=call["args"],
                    )

    def test_profile_is_exported_to_every_cargo_command(self) -> None:
        runner, executor = self.build_runner(profile="fast")
        runner.run_target("core_shard", [])
        for call in executor.calls:
            with self.subTest(args=call["args"]):
                self.assertEqual(call["env"]["NEXTEST_PROFILE"], "fast")

    def test_stack_size_matches_the_windows_test_binary_contract(self) -> None:
        runner, executor = self.build_runner()
        runner.run_target("core_shard", [])
        self.assertEqual(
            executor.last_env()["RUST_MIN_STACK"], rust_test_runner.RUST_MIN_STACK_BYTES
        )

    def test_inherited_profile_is_preserved_when_none_is_requested(self) -> None:
        executor = FakeExecutor(artifacts={"codex": self.helper_executable("codex")})
        runner = RustTestRunner(
            self.manifest(),
            self.metadata(),
            target_dir=self.target_dir,
            platform="windows",
            executor=executor,
            env={"NEXTEST_PROFILE": "local", "RUST_MIN_STACK": "42"},
        )
        runner.run_target("core_shard", [])
        self.assertEqual(executor.last_env()["NEXTEST_PROFILE"], "local")
        self.assertEqual(executor.last_env()["RUST_MIN_STACK"], "42")


class CommandLineTest(unittest.TestCase):
    @staticmethod
    def run_main(argv: list[str]) -> tuple[int, str]:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            code = rust_test_runner.main(argv)
        return code, stderr.getvalue()

    def test_guard_accepts_both_recipe_spellings(self) -> None:
        for command in ("_guard-generic", "guard-args"):
            with self.subTest(command=command):
                self.assertEqual(
                    self.run_main([command, "--", "-p", "codex-tui"]), (0, "")
                )
                code, message = self.run_main([command, "--", "-p", "codex-core"])
                self.assertEqual(code, 2)
                self.assertIn("just core-test", message)

    def test_guard_names_the_calling_recipe(self) -> None:
        _, message = self.run_main(
            ["guard-args", "--recipe", "just test-fast", "--", "-p", "codex-core"]
        )
        self.assertIn("just test-fast cannot select codex-core", message)

    def test_guard_does_not_read_the_manifest(self) -> None:
        # The guard runs on every generic recipe invocation, so it must not
        # depend on the manifest being readable.
        self.assertEqual(
            self.run_main(
                [
                    "--manifest",
                    "does-not-exist.toml",
                    "guard-args",
                    "--",
                    "-p",
                    "codex-tui",
                ]
            ),
            (0, ""),
        )


class JustfileContractTest(unittest.TestCase):
    """Every justfile invocation of the runner must parse against its CLI.

    The justfile and the runner are separate files that are edited
    independently; this keeps a renamed subcommand or a moved option from
    breaking `just test` and the `core-*` recipes silently.
    """

    # Just and PowerShell placeholders standing in for real runtime values.
    PLACEHOLDERS: ClassVar[dict[str, str]] = {
        "{{ target }}": "core_windows",
        "{{ gate }}": "config-schema-protocol",
        "{{ name }}": "core_windows",
        "{{ legacy }}": "core_windows",
        "{{ package }}": "codex-tui",
        "$target_dir": "target",
        "@forwarded_args": "core_windows",
    }

    @staticmethod
    def tokenize(argv: str) -> list[str]:
        tokens: list[str] = []
        current: list[str] = []
        quote: str | None = None
        for char in argv:
            if quote is not None:
                if char == quote:
                    quote = None
                else:
                    current.append(char)
            elif char in "\"'":
                quote = char
            elif char.isspace():
                if current:
                    tokens.append("".join(current))
                    current = []
            else:
                current.append(char)
        if current:
            tokens.append("".join(current))
        return tokens

    def invocations(self) -> list[list[str]]:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        found: list[list[str]] = []
        for line in justfile.splitlines():
            _, separator, rest = line.partition("rust_test_runner.py")
            if not separator:
                continue
            argv = rest.lstrip('"').split(";", 1)[0]
            if "run-gate" in argv:
                argv = argv.replace(
                    "@forwarded_args",
                    "config-schema-protocol capability-command-preflight",
                )
            tokens = [
                self.PLACEHOLDERS.get(token, token) for token in self.tokenize(argv)
            ]
            found.append([token for token in tokens if token])
        return found

    def test_every_justfile_invocation_parses(self) -> None:
        invocations = self.invocations()
        self.assertGreaterEqual(
            len(invocations), 10, "runner invocations were not found"
        )
        parser = rust_test_runner.build_parser()
        for argv in invocations:
            with self.subTest(argv=argv):
                try:
                    parsed = parser.parse_args(argv)
                except SystemExit as exit_error:  # argparse rejects the shape
                    self.fail(
                        f"justfile invocation is not accepted: {argv} ({exit_error})"
                    )
                self.assertIsNotNone(parsed.command)

    def test_named_selections_in_the_justfile_exist_in_the_manifest(self) -> None:
        manifest = Manifest.load(
            REPO_ROOT / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        )
        parser = rust_test_runner.build_parser()
        for argv in self.invocations():
            parsed = parser.parse_args(argv)
            with self.subTest(argv=argv):
                if parsed.command == "run-target":
                    self.assertIn(parsed.name, manifest.targets)
                elif parsed.command == "plan":
                    self.assertIn(
                        parsed.name, manifest.targets.keys() | manifest.gates.keys()
                    )
                elif parsed.command == "run-gate":
                    for name in parsed.names:
                        self.assertIn(name, manifest.gates)
                elif parsed.command == "parity":
                    self.assertIn(parsed.legacy_target, manifest.targets)


if __name__ == "__main__":
    unittest.main()
