#!/usr/bin/env python3

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


TOOL_ROOT = Path(__file__).resolve().parent
RUNNER = TOOL_ROOT / "native_test_runner.py"


class NativeTestRunnerIntegrationTest(unittest.TestCase):
    def run_runner(
        self,
        *args: str,
        env: dict[str, str] | None = None,
        runner: Path = RUNNER,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(runner), *args],
            cwd=runner.parent,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def fake_cargo(self, root: Path) -> tuple[list[str], dict[str, str]]:
        log_path = root / "cargo-log.jsonl"
        script_path = root / "fake_cargo.py"
        script_path.write_text(
            textwrap.dedent(
                """
                import json
                import os
                import sys
                from pathlib import Path

                args = sys.argv[1:]
                log = Path(os.environ["FAKE_CARGO_LOG"])
                with log.open("a", encoding="utf-8") as handle:
                    handle.write(json.dumps({
                        "args": args,
                        "ui_case": os.environ.get("KD4_ARGUMENT_COMMENT_LINT_UI_CASE"),
                        "preserved_env": os.environ.get("KD4_TEST_PRESERVED_ENV"),
                        "path": os.environ.get("PATH"),
                    }) + "\\n")

                if "--list" in args:
                    mode = os.environ.get("FAKE_CARGO_LIST_MODE", "complete")
                    if mode == "failure":
                        print("list failed", file=sys.stderr)
                        raise SystemExit(2)
                    if "--lib" in args:
                        tests = [
                            "comment_parser::tests::parses_prefix_comment",
                            "comment_parser::tests::parses_trailing_comment",
                            "comment_parser::tests::rejects_non_matching_shapes",
                            "ui",
                            "workspace_crate_filter_accepts_first_party_names_only",
                        ]
                        if mode == "zero":
                            tests = []
                        elif mode == "missing":
                            tests.remove("ui")
                        elif mode == "extra":
                            tests.append("unexpected_native_test")
                        elif mode == "duplicate":
                            tests.append(tests[0])
                        elif mode == "duplicate_ui":
                            tests.append("ui")
                    elif "--bin" in args:
                        tests = [
                            "tests::uses_windows_cargo_dylint_binary_name",
                            "tests::strips_host_triple_from_nightly_filename",
                            "tests::leaves_unqualified_nightly_filename_alone",
                            "tests::strict_rustflags_promotes_both_enforced_lints",
                        ]
                    elif "--doc" in args:
                        tests = [
                            "src/lib.rs - ARGUMENT_COMMENT_MISMATCH (line 64)",
                            "src/lib.rs - ARGUMENT_COMMENT_MISMATCH (line 74)",
                            "src/lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT (line 105)",
                            "src/lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT (line 115)",
                        ]
                        if mode == "windows_doc_paths":
                            tests = [
                                test.replace("src/lib.rs", "src" + chr(92) + "lib.rs")
                                for test in tests
                            ]
                        elif mode == "ambiguous":
                            tests[2] = (
                                "src/lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT "
                                "(line 64)"
                            )
                        elif mode == "missing_doc":
                            tests.pop()
                        elif mode == "extra_doc":
                            tests.append("src/lib.rs - UNMAPPED_ITEM (line 150)")
                    else:
                        print("unexpected list target", file=sys.stderr)
                        raise SystemExit(2)
                    for test in tests:
                        print(f"{test}: test")
                    raise SystemExit(0)

                separator = args.index("--")
                native_id = args[separator + 1]
                if "--doc" in args:
                    native_id = {
                        "64": "src/lib.rs - ARGUMENT_COMMENT_MISMATCH (line 64)",
                        "74": "src/lib.rs - ARGUMENT_COMMENT_MISMATCH (line 74)",
                        "105": "src/lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT (line 105)",
                        "115": "src/lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT (line 115)",
                    }[native_id]
                mode = os.environ.get("FAKE_CARGO_MODE", "pass")
                if mode == "pre_result" or (
                    mode == "mixed_failure_pre_result"
                    and native_id.endswith("strips_host_triple_from_nightly_filename")
                ):
                    print("runner failed before a test result", file=sys.stderr)
                    raise SystemExit(2)

                print(json.dumps({"type": "test", "event": "started", "name": native_id}))
                if mode == "after_start_infra":
                    print("runner failed after launch but before a result", file=sys.stderr)
                    raise SystemExit(2)
                event = (
                    "failed"
                    if mode in ("failure", "mixed_failure_pre_result")
                    else "ok"
                )
                result = {"type": "test", "event": event, "name": native_id}
                if mode == "ui_load_error":
                    result["event"] = "failed"
                    result["stdout"] = "error: could not load library selected.dll"
                print(json.dumps(result))
                raise SystemExit(1 if event == "failed" else 0)
                """
            ).strip()
            + "\n",
            encoding="utf-8",
        )
        linker_path = root / ("lld-link.exe" if os.name == "nt" else "cc")
        linker_path.write_text("", encoding="utf-8")
        if os.name != "nt":
            linker_path.chmod(0o755)
        env = os.environ.copy()
        env["FAKE_CARGO_LOG"] = str(log_path)
        env["FAKE_DISCOVERY_LINKER"] = str(linker_path.resolve())
        env["PATH"] = os.pathsep.join(
            part for part in (str(root), env.get("PATH", "")) if part
        )
        return [
            "--cargo",
            sys.executable,
            "--cargo-arg",
            str(script_path),
        ], env

    def temporary_runner_tree(self, root: Path) -> Path:
        runner = root / "native_test_runner.py"
        shutil.copyfile(RUNNER, runner)
        ui_root = root / "ui"
        ui_root.mkdir()
        for source in (TOOL_ROOT / "ui").iterdir():
            if source.suffix in {".rs", ".stderr"}:
                shutil.copyfile(source, ui_root / source.name)
        return runner

    def read_log(self, path: Path) -> list[dict[str, object]]:
        if not path.exists():
            return []
        return [
            json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()
        ]

    def assert_list_pre_result(self, mode: str) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["FAKE_CARGO_LIST_MODE"] = mode
            completed = self.run_runner(*cargo_args, "list", env=env)
            self.assertEqual(completed.returncode, 2)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "pre_result_error")
            self.assertEqual(report["actually_executed_validation_ids"], [])

    def test_list_uses_selected_cargo_and_reconciles_native_topology(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(*cargo_args, "list", env=env)

            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(
                report["report_type"], "ArgumentCommentLintNativeTestInventoryV1"
            )
            self.assertEqual(report["count"], 21)
            ids = [test["id"] for test in report["tests"]]
            self.assertEqual(len(ids), len(set(ids)))
            self.assertEqual(
                {test["kind"] for test in report["tests"]},
                {"rust-lib", "rust-bin", "rust-doctest", "dylint-ui"},
            )
            self.assertTrue(
                all(isinstance(test["native_id"], str) for test in report["tests"])
            )
            native_counts = {
                "rust-lib": 4,
                "dylint-ui": 1,
                "rust-bin": 4,
                "rust-doctest": 4,
            }
            self.assertEqual(
                {
                    kind: len(
                        {
                            test["native_id"]
                            for test in report["tests"]
                            if test["kind"] == kind
                        }
                    )
                    for kind in native_counts
                },
                native_counts,
            )
            log = self.read_log(root / "cargo-log.jsonl")
            linker_override = (
                "target.'cfg(all())'.linker="
                + json.dumps(env["FAKE_DISCOVERY_LINKER"], ensure_ascii=False)
            )
            self.assertEqual(
                [entry["args"] for entry in log],
                [
                    [
                        "--config",
                        linker_override,
                        "test",
                        "--offline",
                        "--lib",
                        "--",
                        "--list",
                    ],
                    [
                        "--config",
                        linker_override,
                        "test",
                        "--offline",
                        "--bin",
                        "argument-comment-lint",
                        "--",
                        "--list",
                    ],
                    [
                        "--config",
                        linker_override,
                        "test",
                        "--offline",
                        "--doc",
                        "--",
                        "--list",
                    ],
                ],
            )

    def test_list_accepts_windows_rustdoc_paths_and_preserves_native_ids(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["FAKE_CARGO_LIST_MODE"] = "windows_doc_paths"
            completed = self.run_runner(*cargo_args, "list", env=env)

            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(report["count"], 21)
            self.assertEqual(
                [
                    test["native_id"]
                    for test in report["tests"]
                    if test["kind"] == "rust-doctest"
                ],
                [
                    "src\\lib.rs - ARGUMENT_COMMENT_MISMATCH (line 64)",
                    "src\\lib.rs - ARGUMENT_COMMENT_MISMATCH (line 74)",
                    (
                        "src\\lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT "
                        "(line 105)"
                    ),
                    (
                        "src\\lib.rs - UNCOMMENTED_ANONYMOUS_LITERAL_ARGUMENT "
                        "(line 115)"
                    ),
                ],
            )

    def test_discovery_preserves_the_selected_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["KD4_TEST_PRESERVED_ENV"] = "preserved-through-discovery"
            expected_path = env["PATH"]
            completed = self.run_runner(*cargo_args, "list", env=env)

            self.assertEqual(completed.returncode, 0, completed.stderr)
            log = self.read_log(root / "cargo-log.jsonl")
            self.assertEqual(len(log), 3)
            self.assertTrue(
                all(
                    entry["preserved_env"] == "preserved-through-discovery"
                    and entry["path"] == expected_path
                    for entry in log
                )
            )

    def test_missing_discovery_linker_is_pre_result_without_launching_cargo(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            empty_path = root / "empty-path"
            empty_path.mkdir()
            env["PATH"] = str(empty_path)
            completed = self.run_runner(*cargo_args, "list", env=env)

            self.assertEqual(completed.returncode, 2)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "pre_result_error")
            self.assertIn("ordinary linker", report["error"])
            self.assertEqual(report["actually_executed_validation_ids"], [])
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_list_command_failure_is_pre_result(self) -> None:
        self.assert_list_pre_result("failure")

    def test_zero_native_discovery_is_pre_result(self) -> None:
        self.assert_list_pre_result("zero")

    def test_missing_native_identity_is_pre_result(self) -> None:
        self.assert_list_pre_result("missing")

    def test_extra_native_identity_is_pre_result(self) -> None:
        self.assert_list_pre_result("extra")

    def test_duplicate_native_identity_is_pre_result(self) -> None:
        self.assert_list_pre_result("duplicate")

    def test_multiple_ui_native_identities_are_pre_result(self) -> None:
        self.assert_list_pre_result("duplicate_ui")

    def test_ambiguous_doctest_filter_is_pre_result(self) -> None:
        self.assert_list_pre_result("ambiguous")

    def test_missing_doctest_identity_is_pre_result(self) -> None:
        self.assert_list_pre_result("missing_doc")

    def test_extra_doctest_identity_is_pre_result(self) -> None:
        self.assert_list_pre_result("extra_doc")

    def test_missing_ui_rs_companion_is_pre_result(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            runner = self.temporary_runner_tree(root)
            (root / "ui" / "comment_matches.rs").unlink()
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args, "list", env=env, runner=runner
            )
            self.assertEqual(completed.returncode, 2)
            self.assertEqual(json.loads(completed.stdout)["result"], "pre_result_error")
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_extra_ui_rs_companion_is_pre_result(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            runner = self.temporary_runner_tree(root)
            (root / "ui" / "unexpected.rs").write_text("fn main() {}\n", encoding="utf-8")
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args, "list", env=env, runner=runner
            )
            self.assertEqual(completed.returncode, 2)
            self.assertEqual(json.loads(completed.stdout)["result"], "pre_result_error")
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_missing_ui_stderr_companion_is_pre_result(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            runner = self.temporary_runner_tree(root)
            (root / "ui" / "comment_mismatch.stderr").unlink()
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args, "list", env=env, runner=runner
            )
            self.assertEqual(completed.returncode, 2)
            self.assertEqual(json.loads(completed.stdout)["result"], "pre_result_error")
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_extra_ui_stderr_companion_is_pre_result(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            runner = self.temporary_runner_tree(root)
            (root / "ui" / "comment_matches.stderr").write_text(
                "unexpected\n", encoding="utf-8"
            )
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args, "list", env=env, runner=runner
            )
            self.assertEqual(completed.returncode, 2)
            self.assertEqual(json.loads(completed.stdout)["result"], "pre_result_error")
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_exact_selected_leaves_execute_through_the_cli(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            selected = [
                "argument-comment-lint::rust-lib::comment_parser.parses_prefix_comment",
                "argument-comment-lint::rust-doctest::argument_comment_mismatch.use_instead",
                "argument-comment-lint::dylint-ui::comment_mismatch",
            ]
            completed = self.run_runner(
                *cargo_args,
                "run",
                *(part for test_id in selected for part in ("--test", test_id)),
                env=env,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "confirmed_pass")
            self.assertEqual(report["selected_validation_ids"], selected)
            self.assertEqual(report["actually_executed_validation_ids"], selected)
            self.assertEqual(
                [outcome["classification"] for outcome in report["outcomes"]],
                ["confirmed_pass", "confirmed_pass", "confirmed_pass"],
            )

            log = self.read_log(root / "cargo-log.jsonl")
            self.assertEqual(len(log), 6)
            self.assertTrue(all("--list" in entry["args"] for entry in log[:3]))
            run_entries = log[3:]
            self.assertIn("--exact", run_entries[0]["args"])
            self.assertNotIn("--exact", run_entries[1]["args"])
            self.assertIn("--exact", run_entries[2]["args"])
            self.assertEqual(
                run_entries[1]["args"][run_entries[1]["args"].index("--") + 1],
                "74",
            )
            self.assertEqual(
                [entry["ui_case"] for entry in run_entries],
                [None, None, "comment_mismatch"],
            )
            for index, entry in enumerate(run_entries):
                args = entry["args"]
                separator = args.index("--")
                self.assertEqual(args.count("--"), 1)
                if index != 1:
                    self.assertEqual(args[separator + 2], "--exact")

    def test_execution_omits_discovery_override_and_preserves_environment(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["KD4_TEST_PRESERVED_ENV"] = "preserved-through-execution"
            expected_path = env["PATH"]
            completed = self.run_runner(
                *cargo_args,
                "run",
                "--test",
                "argument-comment-lint::rust-lib::comment_parser.parses_prefix_comment",
                env=env,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            log = self.read_log(root / "cargo-log.jsonl")
            self.assertEqual(len(log), 4)
            self.assertTrue(all("--config" in entry["args"] for entry in log[:3]))
            execution = log[3]
            self.assertNotIn("--config", execution["args"])
            self.assertEqual(
                execution["preserved_env"], "preserved-through-execution"
            )
            self.assertEqual(execution["path"], expected_path)

    def test_zero_selection_is_pre_result_without_launching_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(*cargo_args, "run", env=env)
            self.assertEqual(completed.returncode, 2)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "pre_result_error")
            self.assertEqual(report["actually_executed_validation_ids"], [])
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_unknown_selection_is_pre_result_without_launching_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args,
                "run",
                "--test",
                "argument-comment-lint::rust-lib::unknown",
                env=env,
            )
            self.assertEqual(completed.returncode, 2)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "pre_result_error")
            self.assertEqual(report["actually_executed_validation_ids"], [])
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def test_duplicate_selection_is_pre_result_without_launching_cargo(self) -> None:
        test_id = (
            "argument-comment-lint::rust-bin::uses_windows_cargo_dylint_binary_name"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            completed = self.run_runner(
                *cargo_args,
                "run",
                "--test",
                test_id,
                "--test",
                test_id,
                env=env,
            )
            self.assertEqual(completed.returncode, 2)
            self.assertEqual(json.loads(completed.stdout)["result"], "pre_result_error")
            self.assertEqual(self.read_log(root / "cargo-log.jsonl"), [])

    def assert_outcome_classification(
        self,
        *,
        mode: str,
        test_id: str,
        expected_code: int,
        expected_result: str,
        expected_executed: bool,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["FAKE_CARGO_MODE"] = mode
            completed = self.run_runner(
                *cargo_args,
                "run",
                "--test",
                test_id,
                env=env,
            )
            self.assertEqual(completed.returncode, expected_code)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], expected_result)
            self.assertEqual(
                bool(report["actually_executed_validation_ids"]),
                expected_executed,
            )

    def test_confirmed_failure_requires_an_observed_result(self) -> None:
        self.assert_outcome_classification(
            mode="failure",
            test_id=(
                "argument-comment-lint::rust-bin::uses_windows_cargo_dylint_binary_name"
            ),
            expected_code=1,
            expected_result="confirmed_validation_failure",
            expected_executed=True,
        )

    def test_pre_result_error_before_start_has_no_execution_evidence(self) -> None:
        self.assert_outcome_classification(
            mode="pre_result",
            test_id=(
                "argument-comment-lint::rust-bin::uses_windows_cargo_dylint_binary_name"
            ),
            expected_code=2,
            expected_result="pre_result_error",
            expected_executed=False,
        )

    def test_infrastructure_error_after_start_has_no_validation_result(self) -> None:
        self.assert_outcome_classification(
            mode="after_start_infra",
            test_id=(
                "argument-comment-lint::rust-bin::uses_windows_cargo_dylint_binary_name"
            ),
            expected_code=2,
            expected_result="pre_result_error",
            expected_executed=True,
        )

    def test_ui_library_load_error_is_pre_result(self) -> None:
        self.assert_outcome_classification(
            mode="ui_load_error",
            test_id="argument-comment-lint::dylint-ui::comment_mismatch",
            expected_code=2,
            expected_result="pre_result_error",
            expected_executed=True,
        )

    def test_confirmed_failure_is_preserved_when_a_later_leaf_has_no_result(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cargo_args, env = self.fake_cargo(root)
            env["FAKE_CARGO_MODE"] = "mixed_failure_pre_result"
            selected = [
                "argument-comment-lint::rust-bin::uses_windows_cargo_dylint_binary_name",
                "argument-comment-lint::rust-bin::strips_host_triple_from_nightly_filename",
            ]
            completed = self.run_runner(
                *cargo_args,
                "run",
                *(part for test_id in selected for part in ("--test", test_id)),
                env=env,
            )

            self.assertEqual(completed.returncode, 1, completed.stderr)
            report = json.loads(completed.stdout)
            self.assertEqual(report["result"], "confirmed_validation_failure")
            self.assertEqual(report["selected_validation_ids"], selected)
            self.assertEqual(report["actually_executed_validation_ids"], selected[:1])
            self.assertEqual(
                [outcome["classification"] for outcome in report["outcomes"]],
                ["confirmed_validation_failure", "pre_result_error"],
            )


if __name__ == "__main__":
    unittest.main()
