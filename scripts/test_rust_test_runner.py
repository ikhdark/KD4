#!/usr/bin/env python3
"""Unit tests for the strict, manifest-driven Rust test runner.

Every test drives the runner through a fake executor, so no Cargo command runs
and no test binary is built. The Cargo metadata fixture mirrors the shape the
runner consumes from `cargo metadata --no-deps`.
"""

from __future__ import annotations

import contextlib
import copy
import io
import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from scripts import rust_test_runner
from scripts.build_tooling_test_support import REPO_ROOT
from scripts.rust_test_runner import Manifest
from scripts.rust_test_runner import MetadataIndex
from scripts.rust_test_runner import RunnerError
from scripts.rust_test_runner import RustTestRunner


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
        capture_output: bool,
    ) -> subprocess.CompletedProcess[str]:
        self.calls.append(
            {
                "args": list(args),
                "cwd": cwd,
                "env": dict(env),
                "capture_output": capture_output,
            }
        )
        selector = self._selector_for(args)
        failed = args[:3] == ["cargo", "nextest", "run"] and selector in self.failing_runs
        return subprocess.CompletedProcess(
            list(args),
            1 if failed else 0,
            stdout=self._stdout(args),
            stderr=f"failed {selector}" if failed else "",
        )

    def _stdout(self, args: list[str]) -> str:
        if args[:3] == ["cargo", "nextest", "list"]:
            return nextest_list_payload(self._listing_for(args))
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
        binary = args[args.index("--bin") + 1]
        package = args[args.index("-p") + 1]
        package_id = next(
            entry["id"] for entry in METADATA_PACKAGES if entry["name"] == package
        )
        executable = self.artifacts.get(binary)
        if executable is None:
            return json.dumps({"reason": "build-finished", "success": True})
        return json.dumps(
            {
                "reason": "compiler-artifact",
                "package_id": package_id,
                "target": {"name": binary, "kind": ["bin"]},
                "executable": str(executable),
            }
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


class ManifestSchemaTest(RunnerTestCase):
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
        with self.assertRaisesRegex(RunnerError, "exactly one of lib or test"):
            Manifest.from_data(data)

        data = copy.deepcopy(MANIFEST_DATA)
        del data["targets"]["core_all"]["test"]
        with self.assertRaisesRegex(RunnerError, "exactly one of lib or test"):
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
    def test_helper_binary_must_exist_in_cargo_metadata(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["helpers"]["codex"]["bin"] = "codex-renamed"
        with self.assertRaisesRegex(RunnerError, "declares missing binary"):
            RustTestRunner(Manifest.from_data(data), self.metadata())

    def test_test_target_must_exist_in_cargo_metadata(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["test"] = "gone"
        with self.assertRaisesRegex(RunnerError, "declares missing test target"):
            RustTestRunner(Manifest.from_data(data), self.metadata())

    def test_unknown_package_is_rejected(self) -> None:
        data = copy.deepcopy(MANIFEST_DATA)
        data["targets"]["core_all"]["package"] = "codex-nope"
        with self.assertRaisesRegex(RunnerError, "unknown Cargo package 'codex-nope'"):
            RustTestRunner(Manifest.from_data(data), self.metadata())


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
            with self.subTest(argv=argv):
                with self.assertRaisesRegex(
                    RunnerError, "cannot override a named target"
                ):
                    rust_test_runner.validate_filtering_args(argv)

    def test_no_tests_override_is_rejected(self) -> None:
        for argv in (["--no-tests"], ["--no-tests=pass"], ["--no-tests=fail"]):
            with self.subTest(argv=argv):
                with self.assertRaisesRegex(RunnerError, "--no-tests is runner-owned"):
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
    def test_every_codex_core_package_spelling_is_rejected(self) -> None:
        for argv in (
            ["-p", "codex-core"],
            ["--package", "codex-core"],
            ["--package=codex-core"],
            ["-pcodex-core"],
            ["--no-fail-fast", "-p", "codex-core", "-E", "test(x)"],
        ):
            with self.subTest(argv=argv):
                with self.assertRaisesRegex(RunnerError, "cannot select codex-core"):
                    rust_test_runner.guard_generic_recipe_args(argv)

    def test_other_packages_are_allowed(self) -> None:
        rust_test_runner.guard_generic_recipe_args(["-p", "codex-tui"])
        rust_test_runner.guard_generic_recipe_args(["--package=codex-app-server"])

    def test_guard_is_token_aware(self) -> None:
        # `codex-core` inside another option's value is not a package selection.
        rust_test_runner.guard_generic_recipe_args(["-E", "package(codex-core)"])
        # Everything after `--` is a libtest filter, not a Cargo option.
        rust_test_runner.guard_generic_recipe_args(["--", "-p", "codex-core"])


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
        for command in [plan["list"], plan["run"], *plan["builds"]]:
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
    def build_executor(self, **kwargs: Any) -> FakeExecutor:
        artifacts = {
            name: self.helper_executable(name)
            for name in ("codex", "codex-code-mode-host", "test_stdio_server")
        }
        return FakeExecutor(artifacts=artifacts, **kwargs)

    def test_run_forces_no_tests_fail(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [])
        run_commands = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(run_commands), 1)
        self.assertIn("--no-tests=fail", run_commands[0])

    def test_local_run_can_preserve_no_fail_fast_behavior(self) -> None:
        runner, executor = self.runner(executor=self.build_executor())
        runner.run_target("core_all", [], no_fail_fast=True)
        run_commands = executor.commands(["cargo", "nextest", "run"])
        self.assertEqual(len(run_commands), 1)
        self.assertIn("--no-fail-fast", run_commands[0])

    def test_zero_selected_tests_fails_before_anything_is_built(self) -> None:
        executor = self.build_executor(default_listing={})
        runner, _ = self.runner(executor=executor)
        with self.assertRaisesRegex(RunnerError, "selected zero tests"):
            runner.run_target("core_all", [])
        self.assertEqual(executor.commands(["cargo", "build"]), [])
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

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
            runner.run_gate("demo-gate")
        self.assertEqual(executor.commands(["cargo", "nextest", "run"]), [])

    def test_unexpected_test_id_fails_the_gate(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        runner, _ = self.runner(executor=self.gate_executor(listings))
        with self.assertRaisesRegex(RunnerError, r"unexpected=\['suite::mod::extra'\]"):
            runner.run_gate("demo-gate")

    def test_gate_verifies_every_step_before_running_any(self) -> None:
        listings = self.matching_listings()
        listings["all"] = {"suite::mod::beta": False, "suite::mod::extra": False}
        executor = self.gate_executor(listings)
        runner, _ = self.runner(executor=executor)
        with self.assertRaises(RunnerError):
            runner.run_gate("demo-gate")
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
            with mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root):
                runner.parity("core_all", ["core_shard"])
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
        self.assertEqual(run_calls[0]["env"]["INSTA_UPDATE"], "always")
        self.assertEqual(run_calls[1]["env"]["INSTA_UPDATE"], "always")

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
            with mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root):
                with self.assertRaisesRegex(RunnerError, "content_changes"):
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
            with mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root):
                with self.assertRaisesRegex(
                    RunnerError,
                    r"(?s)every target.*core_all:.*core_shard_two:",
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
            with mock.patch.object(rust_test_runner, "CODEX_RS_ROOT", codex_rs_root):
                with self.assertRaisesRegex(
                    RunnerError, r"missing=\['suite__b__two\.snap'\].*duplicates=\['suite__a__one\.snap'\]"
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
                "auto_review",
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
    PLACEHOLDERS = {
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
                if parsed.command in {"run-target", "plan"}:
                    self.assertIn(parsed.name, manifest.targets)
                elif parsed.command == "run-gate":
                    self.assertIn(parsed.name, manifest.gates)
                elif parsed.command == "parity":
                    self.assertIn(parsed.legacy_target, manifest.targets)


if __name__ == "__main__":
    unittest.main()
