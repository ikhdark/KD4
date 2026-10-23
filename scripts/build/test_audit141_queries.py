"""Audit 141 source-query and explicit consumer-selection regressions."""

import contextlib
import io
import os
import shlex
from pathlib import Path
from unittest import mock

from scripts.build import rust_test_runner as runner_module
from scripts.build import test_rust_test_runner as runner_tests
from scripts.maintenance import root_maintenance


class Audit141QueryTest(runner_tests.RunnerTestCase):
    def test_cleanup_oracle_rejects_live_or_unknown_and_requires_pid_absence(self):
        oracle = runner_tests.RealExecutorTest(
            "test_deadline_stops_parent_and_descendant_and_keeps_live_logs"
        )
        owner = "scripts.build.test_rust_test_runner"
        with mock.patch(f"{owner}.sys.platform", "linux"):
            for state in ("Z", "S", "R", "T", "D", "?"):
                with self.subTest(state=state), mock.patch(f"{owner}.os.kill"), mock.patch(
                    f"{owner}.Path.read_text", return_value=f"123 (child with spaces) {state} 1 123"
                ):
                    if state == "Z":
                        oracle.assert_posix_descendant_terminated(123)
                    else:
                        with self.assertRaises(AssertionError):
                            oracle.assert_posix_descendant_terminated(123)
            with mock.patch(f"{owner}.os.kill", side_effect=ProcessLookupError):
                oracle.assert_posix_descendant_terminated(123)
            with mock.patch(f"{owner}.Path.read_text", side_effect=FileNotFoundError):
                with mock.patch(f"{owner}.os.kill"):
                    with self.assertRaises(AssertionError):
                        oracle.assert_posix_descendant_terminated(123)
                with mock.patch(f"{owner}.os.kill", side_effect=[None, ProcessLookupError]):
                    oracle.assert_posix_descendant_terminated(123)

    def test_exact_source_cache_reuses_parsing_but_observes_same_size_edits(self):
        root = self.temp_dir / "lib.rs"
        source = "mod tests { fn alpha() {} fn bravo() {} }"
        root.write_text(source, encoding="utf-8")
        context = {}
        with mock.patch.object(
            runner_module, "_rust_scope_items", wraps=runner_module._rust_scope_items
        ) as parse:
            for name in ("alpha", "bravo", "alpha"):
                self.assertEqual(
                    runner_module._rust_test_source(
                        root, f"tests::{name}", self.temp_dir, declarations=context
                    ),
                    root,
                )
            # One crate scope and one inline module, not two parses per ID.
            self.assertEqual(parse.call_count, 2)
            stat = root.stat()
            root.write_text(source.replace("alpha", "omega"), encoding="utf-8")
            os.utime(root, ns=(stat.st_atime_ns, stat.st_mtime_ns))
            self.assertIsNone(
                runner_module._rust_test_source(
                    root, "tests::alpha", self.temp_dir, declarations=context
                )
            )
            self.assertEqual(
                runner_module._rust_test_source(
                    root, "tests::omega", self.temp_dir, declarations=context
                ),
                root,
            )
            self.assertEqual(parse.call_count, 4)

    def test_source_parse_cache_is_bounded_and_not_shared_between_queries(self):
        context = {}
        for index in range(140):
            runner_module._rust_scope_items_for_query(f"fn case_{index}() {{}}", context)
        self.assertLessEqual(len(context["scope_items"]), 128)
        self.assertLessEqual(context["scope_items_chars"], 4_194_304)
        with mock.patch.object(
            runner_module, "_rust_scope_items", wraps=runner_module._rust_scope_items
        ) as parse:
            runner_module._rust_scope_items_for_query("fn isolated() {}", {})
            runner_module._rust_scope_items_for_query("fn isolated() {}", {})
            self.assertEqual(parse.call_count, 2)

    def test_qualified_filter_advice_follows_registered_renamed_module(self):
        crate = self.temp_dir / "core"
        src = crate / "src"
        src.mkdir(parents=True)
        (src / "lib.rs").write_text("mod plan_store;", encoding="utf-8")
        (src / "plan_store.rs").write_text(
            '#[path = "task_acceptance.rs"] mod acceptance;', encoding="utf-8"
        )
        changed = src / "task_acceptance.rs"
        changed.write_text("mod tests { fn proves_completion() {} }", encoding="utf-8")
        manifest = self.manifest(
            gates={
                "acceptance": {
                    "steps": [{"target": "core_lib", "filter":
                        "test(plan_store::acceptance::tests::) | test(other::tests::)"}]
                },
                "unsupported": {
                    "steps": [{"target": "core_lib", "filter": "not test(/opaque/)"}]
                },
                "unrelated": {
                    "steps": [{"target": "core_lib", "filter": "test(unrelated::tests::)"}]
                },
            }
        )
        runner, executor = self.runner(manifest=manifest)
        package = runner.metadata.packages["codex-core"]
        package["manifest_path"] = str(crate / "Cargo.toml")
        package["targets"] = [{
            "name": "codex_core", "kind": ["lib"], "src_path": str(src / "lib.rs")
        }]
        advice = runner.gates_for([str(changed)])
        self.assertEqual(advice["advisory_gates"], ["acceptance"])
        self.assertIn("unsupported", advice["filter_only_gates_not_evaluated"])
        self.assertIn("unrelated", advice["filter_only_gates_not_evaluated"])
        self.assertEqual(executor.calls, [])

    def test_runner_changes_select_the_effective_receipt_boundary_consumer(self):
        selected = root_maintenance.python_test_targets(
            [], ["scripts/build/rust_test_runner.py"]
        )
        self.assertIn("scripts.build.test_rust_test_runner_receipt_boundaries", selected)
        self.assertEqual(len(selected), len(set(selected)))

    def test_public_producers_require_distinct_existing_consumer_gates(self):
        manifest = runner_module.Manifest.load(runner_module.DEFAULT_MANIFEST)
        cases = {
            "codex-rs/command-analysis/src/validation.rs": "validation-bloat-contracts",
            "codex-rs/app-server-protocol/src/protocol/v2/thread.rs":
                "app-server-schema-fixtures",
        }
        for path, gate in cases.items():
            with self.subTest(path=path):
                result = manifest.required_contracts([path], cwd=runner_module.REPO_ROOT)
                self.assertIn(gate, result["required_gates"])

    def test_raw_workspace_guard_explains_the_helper_aware_route_boundary(self):
        with self.assertRaises(runner_module.RunnerError) as failure:
            runner_module.guard_generic_recipe_args(["--workspace", "--no-fail-fast"])
        message = str(failure.exception)
        self.assertIn("no helper-aware aggregate route", message)
        self.assertIn("core_lib --all --no-fail-fast", message)
        self.assertIn("Neither replaces all workspace packages", message)

    def test_metadata_execution_keeps_package_seed_and_cleans_it_on_failure(self):
        runner, _ = self.runner()
        seeds = []
        parent_env = {"UNCHANGED": "value"}

        def execute(args, **kwargs):
            self.assertIn("--binaries-metadata", args)
            seed = Path(kwargs["env"]["KD4_AGENT_TASK_FIXTURE_SEED"])
            self.assertTrue(seed.is_dir())
            self.assertEqual(list(seed.iterdir()), [])
            seeds.append(seed)
            raise runner_module.RunnerError("fixture failure")

        runner.executor = execute
        with self.assertRaisesRegex(runner_module.RunnerError, "fixture failure"):
            runner._checked(
                ["cargo", "nextest", "run", "--binaries-metadata", "build.json"],
                env=parent_env,
                capture=runner_module.CAPTURE_NONE,
                selected_packages=("codex-agent-task-store",),
            )
        self.assertEqual(len(seeds), 1)
        self.assertFalse(seeds[0].exists())
        self.assertEqual(parent_env, {"UNCHANGED": "value"})

    def test_multi_gate_repair_preserves_the_entire_original_requirement(self):
        manifest = self.manifest(gates={
            "first": {"steps": [{"target": "core_lib", "tests": ["alpha"], "helpers": []}]},
            "second": {"steps": [{"target": "core_all", "tests": ["beta"], "helpers": ["codex"]}]},
        })
        executor = runner_tests.RunGateTest.gate_executor(
            self, {"--lib": {"alpha": False}, "all": {"beta": False}}
        )
        runner, _ = self.runner(manifest=manifest, executor=executor)
        original = runner.executor

        def execute(args, **kwargs):
            result = original(args, **kwargs)
            if args[:3] == ["cargo", "nextest", "run"] and "--lib" in args:
                result.returncode = 100
                result.stdout = runner_tests.nextest_run_payload([("alpha", "failed")])
            return result

        runner.executor = execute
        output = io.StringIO()
        with contextlib.redirect_stderr(output), self.assertRaises(runner_module.RunnerError):
            runner.run_gates(["first", "second"], quiet=True)
        command = output.getvalue().split(
            "Rerun declaring gates (preserves helper contracts; from repository root):\n"
        )[1].splitlines()[0].removeprefix("& ")
        parsed = runner_module.build_parser().parse_args(shlex.split(command)[2:])
        self.assertEqual(parsed.command, "run-gate")
        self.assertEqual(parsed.names, ["first", "second"])
