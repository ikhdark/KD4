import contextlib
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
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
        query = {"categories": [{"name": "templates", "paths": ["codex-rs/*.md"], "verification": "path"}],
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
        query = {"categories": [{"name": "sources", "paths": ["*.rs"], "verification": "path"}],
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
        query = {"categories": [{"name": "prompts", "paths": ["src/*.rs"], "contains": "prompt", "verification": "path"}]}
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

    def test_cli_retains_records_and_emits_only_summary_or_exact_paths(self):
        self.file("src/a.rs", "PROMPT " + "body" * 10000)
        query = Path(self.temp.name) / "query.json"
        state = Path(self.temp.name) / "state.json"
        query.write_text(json.dumps({"categories": [{"name": "prompts", "paths": ["src/*.rs"], "contains": "PROMPT", "verification": "path"}]}))
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

    def decision(self, path, consumer, disposition="include"):
        return {"path": path, "category": "runtime", "disposition": disposition,
                "source_sha256": inventory.digest((self.root / path).read_bytes()),
                "reason": "reviewed runtime consumer" if disposition == "include" else "maintainer reference only",
                "evidence": [{"path": consumer, "sha256": inventory.digest((self.root / consumer).read_bytes()),
                              "line": 1, "text": (self.root / consumer).read_text().splitlines()[0]}]}

    def test_runtime_matches_require_exact_consumer_evidence_even_when_marked_resolved(self):
        self.file("prompts/runtime.md")
        self.file("prompts/orchestrator.md", "Maintainer reference, not delivered to the model.")
        self.file("src/loader.rs", 'send(include_str!("../prompts/runtime.md"));')
        query = {"categories": [{"name": "runtime", "paths": ["prompts/*.md"], "unresolved": False}]}
        output, state = inventory.inventory(self.root, query)
        self.assertEqual(output["paths"], [])
        self.assertEqual(len(output["unresolved"]), 2)
        self.assertFalse(output["ready_to_render"])
        query["decisions"] = [self.decision("prompts/runtime.md", "src/loader.rs"),
                              self.decision("prompts/orchestrator.md", "prompts/orchestrator.md", "exclude")]
        output, state = inventory.inventory(self.root, query, state)
        self.assertEqual(output["paths"], ["prompts/runtime.md"])
        self.assertEqual(output["count"], 1)
        self.assertEqual(output["excluded"][0]["path"], "prompts/orchestrator.md")
        self.assertTrue(output["ready_to_render"])
        self.assertEqual(output["next_action"], "render")
        self.assertEqual({r["path"]: r["status"] for r in state["records"]},
                         {"prompts/runtime.md": "verified", "prompts/orchestrator.md": "excluded"})
        without_decisions = {key: value for key, value in query.items() if key != "decisions"}
        reused, _ = inventory.inventory(self.root, without_decisions, state)
        self.assertEqual(reused["paths"], output["paths"])
        self.assertEqual(reused["excluded"], output["excluded"])
        self.assertTrue(reused["ready_to_render"])

    def test_consumer_changes_and_invented_lines_invalidate_classification(self):
        source = self.file("prompts/runtime.md")
        consumer = self.file("src/loader.rs", 'send(include_str!("../prompts/runtime.md"));')
        query = {"categories": [{"name": "runtime", "paths": ["prompts/*.md"]}],
                 "decisions": [self.decision("prompts/runtime.md", "src/loader.rs")]}
        output, state = inventory.inventory(self.root, query)
        self.assertTrue(output["ready_to_render"])
        consumer.write_text("// no runtime consumer now", encoding="utf-8")
        output, stale = inventory.inventory(self.root, query, state)
        self.assertEqual(output["count"], 0)
        self.assertEqual(output["unresolved"][0]["unresolved"], "consumer evidence is unavailable or stale")
        query["decisions"][0]["evidence"][0]["sha256"] = inventory.digest(consumer.read_bytes())
        output, _ = inventory.inventory(self.root, query, stale)
        self.assertEqual(output["unresolved"][0]["unresolved"], "consumer evidence does not match the exact source line")
        source.write_text("changed prompt", encoding="utf-8")
        output, _ = inventory.inventory(self.root, query, state)
        self.assertEqual(output["unresolved"][0]["unresolved"], "classification source is stale")

    def test_missing_required_category_survives_an_otherwise_resolved_inventory(self):
        self.file("prompts/a.md")
        query = {"required_categories": ["assets", "catalog", "inline"],
                 "categories": [{"name": "assets", "paths": ["prompts/*.md"], "verification": "path"}]}
        output, state = inventory.inventory(self.root, query)
        self.assertEqual(output["missing_categories"], ["catalog", "inline"])
        self.assertFalse(output["ready_to_render"])
        self.assertEqual(output["next_action"], "resolve_remaining")
        report = inventory.render_report(state)
        self.assertIn("Status: partial", report)
        self.assertIn("Missing required category: <code>catalog</code>", report)
        self.assertIn("(matched)", report)
        self.assertNotIn("(verified)", report)

    def test_cli_renders_exact_snapshot_without_search_or_state_rewrite(self):
        paths = ["prompts/compact/incremental_prompt.md", "prompts/review/exit_success.xml"]
        for path in paths:
            self.file(path)
        query = Path(self.temp.name) / "query.json"
        state_path = Path(self.temp.name) / "state.json"
        report = Path(self.temp.name) / "report.md"
        query.write_text(json.dumps({"categories": [
            {"name": "assets", "paths": ["prompts/*"], "verification": "path"},
            {"name": "overlap", "paths": [paths[0]], "verification": "path"}]}))
        result = subprocess.run([sys.executable, str(Path(inventory.__file__).resolve()),
                                 "--root", str(self.root), "--query", str(query), "--state", str(state_path),
                                 "--report", str(report)], check=True, capture_output=True, text=True)
        summary = json.loads(result.stdout)
        self.assertEqual(summary["count"], 2)
        self.assertNotEqual(summary["report"], str(report.resolve()))
        self.assertEqual(Path(summary["report"]).read_bytes(), report.read_bytes())
        delivered = json.loads(Path(summary["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual(delivered["paths"], paths)
        self.assertEqual(delivered["count"], len(delivered["paths"]))
        self.assertEqual(delivered["query_id"], summary["query_id"])
        # These sets have the right count, but must never pass exact delivery.
        for wrong in [[paths[0], paths[1].replace(".xml", ".md")],
                      [paths[0].replace("compact/", "compaction/"), paths[1]]]:
            self.assertEqual(len(wrong), delivered["count"])
            self.assertNotEqual(delivered["paths"], wrong)
        self.assertEqual(summary["next_action"], "deliver_report")
        saved = state_path.read_bytes()
        before = report.read_text(encoding="utf-8")
        self.assertIn("Included tracked files: **2**", before)
        for path in paths:
            self.assertIn(f"<code>{path}</code>", before)
        self.assertNotIn("with_tool_output.md", before)
        (self.root / paths[0]).unlink()
        with (mock.patch.object(inventory, "inventory", side_effect=AssertionError("must not rescan")),
              contextlib.redirect_stdout(io.StringIO())):
            inventory.main(["--state", str(state_path), "--render-only", "--report", str(report)])
        self.assertEqual(state_path.read_bytes(), saved)
        self.assertEqual(report.read_text(encoding="utf-8"), before)
        self.assertIn("Retained snapshot", before)

    def test_scope_cannot_silently_drop_required_or_unresolved_categories(self):
        self.file("prompts/a.md")
        self.file("catalog.json", '{"prompt":"body"}')
        broad = {"required_categories": ["assets", "catalog", "inline"], "categories": [
            {"name": "assets", "paths": ["prompts/*.md"], "verification": "path"},
            {"name": "catalog", "paths": ["catalog.json"]}]}
        _, state = inventory.inventory(self.root, broad)
        narrow = {"categories": [broad["categories"][0]]}
        with (mock.patch.object(inventory, "repository_source_records", side_effect=AssertionError("must reject before scan")),
              self.assertRaisesRegex(ValueError, "query scope changed")):
            inventory.inventory(self.root, narrow, state)
        narrow["scope_change"] = {"from_query_id": state["output"]["query_id"],
                                  "reason": "User requested only template assets"}
        output, revised = inventory.inventory(self.root, narrow, state)
        self.assertTrue(output["ready_to_render"])
        self.assertNotEqual(output["query_id"], state["output"]["query_id"])
        report = inventory.render_report(revised)
        for text in ["Earlier scope (not covered by current completion)", "catalog.json", "Not examined: <code>inline</code>", narrow["scope_change"]["reason"]]:
            self.assertIn(text, report)
        _, again = inventory.inventory(self.root, narrow, revised)
        self.assertEqual(len(again["scope_changes"]), 1)
        narrowed_rule = json.loads(json.dumps(narrow))
        narrowed_rule["categories"][0]["paths"] = ["prompts/a.md"]
        with self.assertRaisesRegex(ValueError, "query scope changed"):
            inventory.inventory(self.root, narrowed_rule, revised)

    def test_reordered_categories_produce_identical_delivery_bytes(self):
        self.file("a.md")
        self.file("b.rs")
        query = {"required_categories": ["assets", "sources", "missing"], "categories": [
            {"name": "assets", "paths": ["*.md"], "verification": "path"},
            {"name": "sources", "paths": ["*.rs"], "verification": "path"}]}
        _, state = inventory.inventory(self.root, query)
        report = Path(self.temp.name) / "report.md"
        first = inventory.export_delivery(state, report)
        saved_report = Path(first["report"]).read_bytes()
        saved_data = Path(first["canonical_paths"]).read_bytes()
        reordered = {"required_categories": list(reversed(query["required_categories"])),
                     "categories": list(reversed(query["categories"]))}
        self.assertEqual(inventory.query_identity(query), inventory.query_identity(reordered))
        for previous in (None, state):
            with self.subTest(reuse=previous is not None):
                _, reordered_state = inventory.inventory(self.root, reordered, previous)
                self.assertEqual(inventory.render_report(state), inventory.render_report(reordered_state))
                second = inventory.export_delivery(reordered_state, report)
                self.assertEqual(first, second)
                self.assertEqual(Path(second["report"]).read_bytes(), saved_report)
                self.assertEqual(Path(second["canonical_paths"]).read_bytes(), saved_data)
        Path(first["report"]).write_text("tampered")
        with self.assertRaisesRegex(ValueError, "immutable delivery artifact was modified"):
            inventory.export_delivery(state, report)

    def test_delivery_is_immutable_across_revisions_and_rejects_tampering(self):
        self.file("a.md")
        query = {"categories": [{"name": "assets", "paths": ["*.md"], "verification": "path"}]}
        _, state = inventory.inventory(self.root, query)
        report = Path(self.temp.name) / "report.md"
        first = inventory.export_delivery(state, report)
        saved = Path(first["canonical_paths"]).read_bytes()
        self.file("b.md")
        _, state = inventory.inventory(self.root, query, state)
        second = inventory.export_delivery(state, report)
        self.assertNotEqual(first["canonical_paths"], second["canonical_paths"])
        self.assertEqual(Path(first["canonical_paths"]).read_bytes(), saved)
        self.assertEqual(json.loads(saved)["paths"], ["a.md"])
        Path(second["canonical_paths"]).write_text("tampered")
        with self.assertRaisesRegex(ValueError, "immutable delivery artifact was modified"):
            inventory.export_delivery(state, report)

    def test_scoped_instructions_include_ancestors_and_prune_before_descent(self):
        for path in ["AGENTS.md", "src/AGENTS.md", "src/feature/AGENTS.override.md", "src/feature/nested/AGENTS.md"]:
            self.file(path, tracked=False)
        for path in ["unrelated/AGENTS.md", "src/feature/target/deep/AGENTS.md", "src/feature/node_modules/AGENTS.md"]:
            self.file(path, tracked=False)
        real_run = inventory.repository_source_records.__globals__["subprocess"].run
        with mock.patch("scripts.source_map_check.subprocess.run", wraps=real_run) as run:
            paths = inventory.instruction_paths(self.root, ["src/feature"])
        self.assertEqual(paths, ["AGENTS.md", "src/AGENTS.md", "src/feature/AGENTS.override.md", "src/feature/nested/AGENTS.md"])
        argv = run.call_args.args[0]
        self.assertEqual(argv[-2:], ["--", ":(literal)src/feature"])
        self.assertLess(argv.index("--exclude=target/"), argv.index("--"))
        self.assertEqual(run.call_count, 1)
        with self.assertRaises(ValueError):
            inventory.instruction_paths(self.root, ["../outside"])

    def test_json_structure_and_remaining_page_do_not_print_prompt_bodies_or_rescan(self):
        body = "SECRET PROMPT BODY" * 10000
        self.file("catalog.json", json.dumps({"models": [{"slug": "model", "base_instructions": body}]}))
        query = {"categories": [{"name": "catalog", "paths": ["catalog.json"], "json_summary": True}]}
        output, state = inventory.inventory(self.root, query)
        evidence = output["unresolved"][0]["evidence"]["structure"]
        fields = evidence["fields"]["models"]["items"]["0"]["fields"]
        self.assertEqual(fields["base_instructions"], {"type": "string", "length": len(body)})
        self.assertNotIn("SECRET", json.dumps(state))
        state_path = Path(self.temp.name) / "state.json"
        state_path.write_text(json.dumps(state), encoding="utf-8")
        out = io.StringIO()
        with mock.patch.object(inventory, "repository_source_records", side_effect=AssertionError("must not enumerate")), contextlib.redirect_stdout(out):
            inventory.main(["--state", str(state_path), "--render-only", "--remaining"])
        shown = json.loads(out.getvalue())
        self.assertEqual(shown["remaining"][0]["path"], "catalog.json")
        self.assertEqual(shown["remaining"][0]["evidence"]["structure"], evidence)
        self.assertIsNone(shown["next_offset"])
        self.assertLess(len(out.getvalue()), 2500)
        bounded = inventory.json_shape({"X" * 100000: body, "many": [body] * 5000})
        self.assertNotIn(body, json.dumps(bounded))
        self.assertLess(len(json.dumps(bounded)), 1000)
        self.assertEqual(bounded["fields"]["many"]["omitted"], 4997)

    def test_report_rejects_source_or_state_overwrite(self):
        self.file("a.rs")
        query = Path(self.temp.name) / "query.json"
        state = Path(self.temp.name) / "state.json"
        query.write_text(json.dumps({"categories": [{"name": "files", "paths": ["*.rs"], "verification": "path"}]}))
        for report in [query, state, self.root / "a.rs"]:
            with self.subTest(report=report), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                inventory.main(["--root", str(self.root), "--query", str(query), "--state", str(state), "--report", str(report)])
        self.assertFalse(state.exists())
        self.assertEqual((self.root / "a.rs").read_text(), "prompt evidence")

    def test_consumer_evidence_read_budget_and_legacy_render_rejection(self):
        self.file("prompts/runtime.md")
        self.file("src/loader.rs", "runtime consumer" * 20)
        query = {"categories": [{"name": "runtime", "paths": ["prompts/*.md"]}],
                 "decisions": [self.decision("prompts/runtime.md", "src/loader.rs")]}
        with mock.patch.object(inventory, "MAX_FILE_BYTES", 64):
            output, _ = inventory.inventory(self.root, query)
        self.assertEqual(output["paths"], [])
        self.assertEqual(output["unresolved"][0]["unresolved"], "consumer evidence is unavailable or stale")
        legacy = Path(self.temp.name) / "legacy.json"
        legacy.write_text(json.dumps({"version": 1, "output": {"count": 99}}))
        report = Path(self.temp.name) / "report.md"
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            inventory.main(["--state", str(legacy), "--render-only", "--report", str(report)])
        self.assertFalse(report.exists())


if __name__ == "__main__":
    unittest.main()
