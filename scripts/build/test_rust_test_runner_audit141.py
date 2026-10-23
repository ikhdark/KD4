"""Selection, recovery and accounting contracts identified by audit 141."""

import contextlib
import io
import os
import shlex
import unittest
from unittest import mock

from scripts.build import rust_test_runner as runner_module
from scripts.build.test_rust_test_runner import FakeExecutor
from scripts.build.test_rust_test_runner import RunnerTestCase
from scripts.build.test_rust_test_runner import nextest_run_payload
from scripts.build.validation_metrics import ValidationMetrics


def emitted_argv(output, label):
    command = output.split(label + "\n", 1)[1].splitlines()[0].strip()
    if os.name == "nt":
        command = command.removeprefix("& ")
    return shlex.split(command)[2:]


class OwnershipQueryTest(RunnerTestCase):
    def test_shared_ancestors_are_parsed_once_without_changing_ownership(self):
        crate = self.temp_dir / "core"
        root = crate / "src/lib.rs"
        root.parent.mkdir(parents=True)
        root.write_text("mod parent;", encoding="utf-8")
        parent = root.parent / "parent.rs"
        parent.write_text("mod first; mod second;", encoding="utf-8")
        leaves = []
        for name in ("first", "second"):
            path = root.parent / "parent" / f"{name}.rs"
            path.parent.mkdir(exist_ok=True)
            path.write_text("#[test] fn works() {}", encoding="utf-8")
            leaves.append(str(path))
        runner, executor = self.runner(
            manifest=self.manifest(
                gates={
                    name: {
                        "steps": [
                            {"target": "core_lib", "tests": [f"parent::{name}::works"]}
                        ]
                    }
                    for name in ("first", "second")
                }
            )
        )
        package = runner.metadata.packages["codex-core"]
        package["manifest_path"] = str(crate / "Cargo.toml")
        package["targets"] = [
            {"name": "codex_core", "kind": ["lib"], "src_path": str(root)}
        ]
        with mock.patch.object(
            runner_module, "_rust_scope_items", wraps=runner_module._rust_scope_items
        ) as parse:
            result = runner.gates_for(leaves)
        self.assertEqual(result["paths"], {leaves[0]: ["first"], leaves[1]: ["second"]})
        texts = [call.args[0] for call in parse.call_args_list]
        self.assertEqual(texts.count("mod parent;"), 1)
        self.assertEqual(texts.count("mod first; mod second;"), 1)
        self.assertEqual(executor.calls, [])
        # A new query must observe changed registration, not an earlier result.
        parent.write_text("mod first;", encoding="utf-8")
        self.assertEqual(runner.gates_for(leaves)["target_paths"][leaves[1]], [])

    def test_equal_text_does_not_cache_paths_or_hide_changed_text(self):
        roots = [self.temp_dir / name / "lib.rs" for name in ("one", "two")]
        for root in roots:
            root.parent.mkdir()
            root.write_text("mod child;", encoding="utf-8")
        child = roots[0].parent / "child.rs"
        child.write_text("", encoding="utf-8")
        query = {}
        self.assertEqual(
            runner_module._rust_module_source(
                roots[0], "child", self.temp_dir, declarations=query
            ),
            (child.resolve(), None),
        )
        self.assertIsNone(
            runner_module._rust_module_source(
                roots[1], "child", self.temp_dir, declarations=query
            )
        )
        # Same length, same query: only the full source text is a parse key.
        roots[0].write_text("mod other;", encoding="utf-8")
        self.assertIsNone(
            runner_module._rust_module_source(
                roots[0], "child", self.temp_dir, declarations=query
            )
        )


class RecoveryContractTest(RunnerTestCase):
    def helper_free_runner(self):
        return self.runner(
            executor=FakeExecutor(default_listing={"mod::tests::alpha": False}),
            manifest=self.manifest(
                targets={
                    "core_lib": {"package": "codex-core", "lib": True, "helpers": []},
                    "core_all": {"package": "codex-core", "test": "all", "helpers": []},
                }
            ),
        )

    def test_unknown_and_ignored_recovery_keep_conservative_inventory(self):
        runner, _ = self.helper_free_runner()
        for mode in (None, True, False):
            with self.subTest(mode=mode):
                output = io.StringIO()
                with contextlib.redirect_stderr(output):
                    runner._report_failed_tests(
                        "core_lib",
                        [],
                        ["mod::tests::alpha"],
                        ["mod::tests::alpha"],
                        runs_ignored=mode,
                    )
                args = runner_module.build_parser().parse_args(
                    emitted_argv(
                        output.getvalue(), "Rerun failed only (from repository root):"
                    )
                )
                self.assertEqual("--run-ignored" in args.filter_args, mode is not False)
                self.assertIn("--ignore-default-filter", args.filter_args)
                self.assertEqual(args.filter_args[-1], "test(=mod::tests::alpha)")

    def test_ordinary_failed_command_runs_once_and_rejects_later_ignored_drift(self):
        runner, executor = self.helper_free_runner()
        original = runner.executor

        def fail(args, **kwargs):
            result = original(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"]:
                result.returncode = 100
                result.stdout = nextest_run_payload([("mod::tests::alpha", "failed")])
            return result

        runner.executor = fail
        output = io.StringIO()
        with (
            contextlib.redirect_stderr(output),
            self.assertRaises(runner_module.RunnerError),
        ):
            runner.run_target("core_lib", ["-E", "test(=mod::tests::alpha)"])
        args = runner_module.build_parser().parse_args(
            emitted_argv(output.getvalue(), "Rerun failed only (from repository root):")
        )
        executor.calls.clear()
        runner.executor = original
        with contextlib.redirect_stderr(io.StringIO()):
            receipts = runner.run_target(args.name, args.filter_args)
        self.assertEqual(receipts, {"codex-core": ["mod::tests::alpha"]})
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)
        self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])

        def ignored(args, **kwargs):
            result = original(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"]:
                result.stdout = nextest_run_payload([("mod::tests::alpha", "ignored")])
            return result

        runner.executor = ignored
        with (
            contextlib.redirect_stderr(io.StringIO()),
            self.assertRaises(runner_module.RunnerError) as failure,
        ):
            runner.run_target(args.name, args.filter_args)
        self.assertEqual(failure.exception.outcome, "evidence_incomplete")
        self.assertEqual(failure.exception.completed_tests, {})

    def test_failed_gate_recovery_preserves_helper_bound_identity(self):
        runner, executor = self.runner(
            executor=FakeExecutor(default_listing={"mod::tests::alpha": False}),
            manifest=self.manifest(
                gates={
                    "pure": {
                        "steps": [
                            {
                                "target": "core_lib",
                                "helpers": [],
                                "tests": ["mod::tests::alpha"],
                            }
                        ]
                    }
                }
            ),
        )
        original = runner.executor

        def fail(args, **kwargs):
            result = original(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"]:
                result.returncode = 100
                result.stdout = nextest_run_payload([("mod::tests::alpha", "failed")])
            return result

        runner.executor = fail
        output = io.StringIO()
        with (
            contextlib.redirect_stderr(output),
            self.assertRaises(runner_module.RunnerError),
        ):
            runner.run_gates(["pure"])
        args = runner_module.build_parser().parse_args(
            emitted_argv(
                output.getvalue(),
                "Rerun declaring gates (preserves helper contracts; from repository root):",
            )
        )
        self.assertEqual(args.command, "run-gate")
        self.assertEqual(args.names, ["pure"])
        runner.executor = original
        executor.calls.clear()
        with (
            contextlib.redirect_stderr(io.StringIO()),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            receipts = runner.run_gates(args.names)
        self.assertEqual(receipts, {"pure": ["mod::tests::alpha"]})
        self.assertEqual(
            receipts.executed, {("codex-core", frozenset(), "mod::tests::alpha")}
        )
        self.assertEqual(executor.commands(["cargo", "build"]), [])
        self.assertEqual(executor.commands(["cargo", "nextest", "list"]), [])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)

    def test_recovery_matches_helper_sets_independent_of_declaration_order(self):
        runner, executor = self.runner(
            executor=FakeExecutor(
                artifacts={
                    name: self.helper_executable(name)
                    for name in ("codex", "test_stdio_server")
                },
                default_listing={"alpha": False, "beta": False},
            ),
            manifest=self.manifest(
                gates={
                    "first": {
                        "steps": [
                            {
                                "target": "core_all",
                                "helpers": ["codex", "test_stdio_server"],
                                "tests": ["alpha"],
                            }
                        ]
                    },
                    "second": {
                        "steps": [
                            {
                                "target": "core_all",
                                "helpers": ["test_stdio_server", "codex"],
                                "tests": ["beta"],
                            }
                        ]
                    },
                }
            ),
        )
        original = runner.executor

        def fail(args, **kwargs):
            result = original(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"]:
                result.returncode = 100
                result.stdout = nextest_run_payload(
                    [("alpha", "ok"), ("beta", "failed")], binary="all", kind="test"
                )
            return result

        runner.executor = fail
        output = io.StringIO()
        with (
            contextlib.redirect_stderr(output),
            self.assertRaises(runner_module.RunnerError),
        ):
            runner.run_gates(["first", "second"])
        args = runner_module.build_parser().parse_args(
            emitted_argv(
                output.getvalue(),
                "Rerun declaring gates (preserves helper contracts; from repository root):",
            )
        )
        # Repair must cover the original gate obligation in one compatible
        # execution, including the gate whose case already passed.
        self.assertEqual(args.names, ["first", "second"])
        self.assertEqual(len(executor.commands(["cargo", "nextest", "run"])), 1)

    def test_reporter_parsing_is_inside_reconciliation_not_child_wall(self):
        runner, _ = self.helper_free_runner()
        clock = [0.0]
        runner.metrics = ValidationMetrics([], clock=lambda: clock[0])
        original = runner_module._nextest_results

        def parsed(*args, **kwargs):
            clock[0] += 7
            return original(*args, **kwargs)

        with (
            mock.patch.object(runner_module, "_nextest_results", side_effect=parsed),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            receipts = runner.run_target("core_lib", ["-E", "test(=mod::tests::alpha)"])
        self.assertEqual(receipts, {"codex-core": ["mod::tests::alpha"]})
        self.assertEqual(runner.metrics.record["phases"]["reconciliation"], 7)


class ExplicitConsumerContractTest(unittest.TestCase):
    def test_reported_public_contracts_require_their_real_oracles(self):
        manifest = runner_module.Manifest.load(runner_module.DEFAULT_MANIFEST)
        for path, gate in (
            (
                "codex-rs/app-server-protocol/src/protocol/v2/command_exec.rs",
                "app-server-command-exec",
            ),
            ("codex-rs/core/config.schema.json", "config-schema-protocol"),
            ("codex-rs/protocol/src/validation.rs", "validation-protocol"),
        ):
            with self.subTest(path=path):
                result = manifest.required_contracts(
                    [path], cwd=runner_module.REPO_ROOT
                )
                self.assertIn(gate, result["required_gates"])
        self.assertEqual(manifest.targets["protocol_lib"].package, "codex-protocol")
        self.assertTrue(
            all(step.tests for step in manifest.gates["validation-protocol"].steps)
        )
