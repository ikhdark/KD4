import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

from scripts import source_inventory as inventory


class SourceInventoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.git("init", "-q")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True,
                              capture_output=True).stdout

    def file(self, path, text="prompt evidence", tracked=True):
        dest = self.root / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text, encoding="utf-8")
        if tracked:
            self.git("add", "--", path)
        return dest

    def test_rejects_f1_invented_paths_and_derives_unique_counts(self):
        actual = ["codex-rs/ext/image-generation/imagegen_description.md",
                  "codex-rs/ext/web-search/web_run_description.md",
                  "codex-rs/tui/prompt_for_init_command.md",
                  "codex-rs/prompts/templates/compact/incremental_prompt.md"]
        invented = ["codex-rs/ext/image-generation/src/imagegen_description.md",
                    "codex-rs/ext/web-search/src/web_run_description.md",
                    "codex-rs/tui/src/prompt_for_init.md",
                    "codex-rs/prompts/templates/compact/with_tool_output.md"]
        for path in actual:
            self.file(path)
        query = {"categories": [{"name": "templates", "paths": ["codex-rs/**/*.md"]}],
                 "candidates": [{"path": p, "category": "templates"} for p in invented + actual + actual]}
        output, state = inventory.inventory(self.root, query)
        self.assertEqual(output["paths"], sorted(actual))
        self.assertEqual(output["count"], 4)
        self.assertEqual(output["category_counts"], {"templates": 4})
        self.assertEqual({r["path"] for r in output["unresolved"]}, set(invented))
        for record in state["records"]:
            if record["unresolved"] is None:
                self.assertEqual(record["tracking"], "tracked")
                self.assertTrue(record["exists"])
                self.assertEqual(record["evidence"]["path"], record["path"])

    def test_git_states_prune_before_descent_and_never_open_pruned_candidates(self):
        self.file("src/gone.rs").unlink()
        self.file("src/new name.rs", tracked=False)
        self.file("src/tracked.rs")
        self.file("nested/target/deep/build.rs", tracked=False)
        query = {"categories": [{"name": "sources", "paths": ["**/*.rs"]}],
                 "candidates": [{"path": "nested/target/deep/build.rs", "category": "sources"}]}
        real_run = inventory.repository_source_records.__globals__["subprocess"].run
        with mock.patch("scripts.source_map_check.subprocess.run", wraps=real_run) as run:
            output, state = inventory.inventory(self.root, query)
        argv = run.call_args.args[0]
        self.assertIn("--exclude=target/", argv)
        self.assertIn("--exclude=node_modules/", argv)
        self.assertEqual(run.call_count, 1)
        self.assertEqual(output["paths"], ["src/tracked.rs"])
        self.assertEqual(output["untracked_paths"], ["src/new name.rs"])
        records = {r["path"]: r for r in state["records"]}
        self.assertEqual(records["src/gone.rs"]["tracking"], "deleted")
        self.assertFalse(records["src/gone.rs"]["exists"])
        self.assertIsNone(records["nested/target/deep/build.rs"]["exists"])
        self.assertNotIn("nested/target/deep/build.rs", state["files"])

    def test_reuses_coverage_and_invalidates_only_changed_inputs(self):
        first = self.file("src/a.rs", "first prompt")
        self.file("src/b.rs", "second prompt")
        query = {"categories": [{"name": "prompts", "paths": ["src/*.rs"], "contains": "prompt"}]}
        output, state = inventory.inventory(self.root, query)
        self.assertEqual(output["searched_records"], 2)
        output, state = inventory.inventory(self.root, query, state)
        self.assertEqual(output["searched_records"], 0)
        self.assertEqual(output["reused_records"], 2)
        self.file("unrelated.txt", "irrelevant")
        output, state = inventory.inventory(self.root, query, state)
        self.assertEqual(output["searched_records"], 0)
        first.write_text("no match", encoding="utf-8")
        output, state = inventory.inventory(self.root, query, state)
        self.assertEqual(output["searched_records"], 1)
        self.assertEqual(output["reused_records"], 1)
        self.assertEqual(output["paths"], ["src/b.rs"])

    def test_candidate_needs_category_evidence_and_ambiguity_is_not_verified(self):
        self.file("src/a.rs", "not a match")
        query = {"categories": [{"name": "prompts", "paths": ["src/*.rs"], "contains": "PROMPT"}],
                 "candidates": [{"path": "src/a.rs", "category": "prompts"}]}
        output, _ = inventory.inventory(self.root, query)
        self.assertEqual(output["count"], 0)
        self.assertEqual(output["unresolved"][0]["unresolved"], "category rule did not match")
        query["categories"][0] = {"name": "prompts", "paths": ["src/*.rs"], "unresolved": True}
        output, _ = inventory.inventory(self.root, query)
        self.assertEqual(output["count"], 0)
        self.assertEqual(output["unresolved"][0]["unresolved"], "runtime consumer requires inspection")

    def test_path_globs_use_glob_semantics_not_fnmatch(self):
        # `*` stays inside one component and a `**` component matches zero or
        # more components. Python's fnmatch does neither, which silently
        # dropped a directory's own files from `dir/**/*.rs` categories.
        context = "codex-rs/core/src/context/"
        direct = [context + "apps_instructions.rs",
                  context + "available_skills_instructions.rs"]
        nested = [context + "world_state/apps_instructions.rs",
                  context + "world_state/apps_instructions_tests.rs"]
        for path in direct + nested:
            self.file(path)

        query = {"categories": [{"name": "context", "paths": [context + "**/*.rs"]}]}
        output, _ = inventory.inventory(self.root, query)
        self.assertEqual(output["paths"], sorted(direct + nested))

        query = {"categories": [{"name": "context", "paths": [context + "*.rs"]}]}
        output, _ = inventory.inventory(self.root, query)
        self.assertEqual(output["paths"], sorted(direct))

    def test_compile_path_glob_semantics(self):
        for pattern, path, expected in [
            ("a/**/b", "a/b", True),
            ("a/**/b", "a/x/b", True),
            ("a/**/b", "a/x/y/b", True),
            ("a/**/b", "a/bb", False),
            ("**/*.rs", "x.rs", True),
            ("**/*.rs", "a/b/x.rs", True),
            ("**/*.rs", "x.md", False),
            ("a/**", "a/x/y", True),
            ("a/**", "a", False),
            ("src/*.rs", "src/a.rs", True),
            ("src/*.rs", "src/sub/a.rs", False),
            ("*.rs", "src/a.rs", False),
            ("src/?.rs", "src/a.rs", True),
            ("src/?.rs", "src/ab.rs", False),
            ("src/[ab].rs", "src/a.rs", True),
            ("src/[!ab].rs", "src/a.rs", False),
            ("a.b", "axb", False),
        ]:
            with self.subTest(pattern=pattern, path=path):
                matched = bool(inventory.compile_path_glob(pattern).match(path))
                self.assertEqual(matched, expected)

    def test_cli_retains_records_and_emits_only_summary_or_exact_paths(self):
        self.file("src/a.rs", "PROMPT " + "body" * 10000)
        query = Path(self.temp.name) / "query.json"
        state = Path(self.temp.name) / "state.json"
        query.write_text(json.dumps({"categories": [{"name": "prompts", "paths": ["src/*.rs"], "contains": "PROMPT"}]}))
        args = ["--root", str(self.root), "--query", str(query), "--state", str(state)]
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(inventory.main(args), 0)
        summary = json.loads(stdout.getvalue())
        self.assertEqual(summary["count"], 1)
        self.assertLess(len(stdout.getvalue()), 1000)
        saved = json.loads(state.read_text())
        self.assertEqual(saved["records"][0]["evidence"]["line"], 1)
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            inventory.main(args + ["--paths"])
        self.assertEqual(stdout.getvalue(), "src/a.rs\n")
        self.assertEqual(json.loads(state.read_text())["output"]["searched_records"], 0)


if __name__ == "__main__":
    unittest.main()
