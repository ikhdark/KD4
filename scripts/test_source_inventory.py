import base64
import contextlib
import io
import json
import os
import shutil
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
        report_root = mock.patch.object(
            inventory, "report_directory",
            side_effect=lambda state: Path(self.temp.name) / "reports" / state["scan"]["epoch"],
        )
        report_root.start()
        self.addCleanup(report_root.stop)
        recent_index = mock.patch.object(
            inventory, "recent_index_path",
            return_value=Path(self.temp.name) / "reports" / "recent.jsonl",
        )
        recent_index.start()
        self.addCleanup(recent_index.stop)

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

    def scan_stdin(self, query, *args, root=None):
        # An ASCII text wrapper proves that stdin bytes are decoded as UTF-8
        # independently of the caller's locale.
        stream = io.TextIOWrapper(
            io.BytesIO(json.dumps(query, ensure_ascii=False).encode("utf-8")),
            encoding="ascii",
        )
        with (stream, mock.patch.object(sys, "stdin", stream),
              mock.patch.object(tempfile, "tempdir", self.temp.name),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            self.assertEqual(inventory.main([
                "--root", str(root or self.root), "--query", "-", *args,
            ]), 0)
        return json.loads(stdout.getvalue())

    def test_stdin_utf8_matches_file_query_and_preserves_explicit_paths(self):
        self.file("src/café.md", "日本語")
        query = {"categories": [{"name": "日本語", "paths": ["src/*.md"],
                                 "contains": "日本語", "verification": "path"}]}
        query_path = Path(self.temp.name) / "query.json"
        query_path.write_text(json.dumps(query, ensure_ascii=False), encoding="utf-8")
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            inventory.main(["--root", str(self.root), "--query", str(query_path),
                            "--state", str(Path(self.temp.name) / "file-state.json"),
                            "--paths"])
        file_result = json.loads(stdout.getvalue())
        stdin_result = self.scan_stdin(
            query, "--state", str(Path(self.temp.name) / "stdin-state.json"), "--paths",
        )
        for key in ("count", "paths", "category_counts", "query_id", "ready_to_render"):
            self.assertEqual(stdin_result[key], file_result[key])
        self.assertEqual(stdin_result["paths"], ["src/café.md"])
        self.assertEqual(stdin_result["category_counts"], {"日本語": 1})
        self.assertNotIn("report", stdin_result)
        self.assertNotIn("report", file_result)

    def test_large_path_query_batches_without_losing_deleted_or_untracked_sources(self):
        paths = tuple(f"dir {i}/café-😀.md" for i in range(8))
        for i, path in enumerate(paths):
            self.file(path, tracked=i != 7)
        (self.root / paths[0]).unlink()
        expected = inventory.repository_source_records(self.root, paths=paths)
        real_run = subprocess.run
        with (
            mock.patch.object(inventory, "GIT_COMMAND_UNITS", 180),
            mock.patch.object(inventory.subprocess, "run", wraps=real_run) as run,
        ):
            actual = inventory.repository_source_records(self.root, paths=paths)
        self.assertEqual(actual, expected)
        self.assertEqual(actual[paths[0]], "deleted")
        self.assertEqual(actual[paths[-1]], "untracked")
        self.assertGreater(run.call_count, 1)
        for call in run.call_args_list:
            units = len(subprocess.list2cmdline(call.args[0]).encode("utf-16-le")) // 2
            self.assertLessEqual(units, 180)

    def test_review_resumes_idempotently_and_rejects_premature_completion(self):
        self.file("a.md", "abcd")
        result = self.scan_stdin({"categories": [{"name": "source", "paths": ["*.md"], "verification": "path"}]})
        state_path = Path(result["artifact"])
        initial = json.loads(state_path.read_text(encoding="utf-8"))
        update_path = Path(self.temp.name) / "review.json"
        record = {"path": "a.md", "sha256": initial["files"]["a.md"]["sha256"], "ranges": [[0, 2]]}

        def publish(record, observation=None):
            update = {"scan_epoch": result["scan_epoch"], "records": [record]}
            if observation is not None:
                update["observation"] = observation
            update_path.write_text(json.dumps(update), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                inventory.main(["--state", str(state_path), "--review", str(update_path)])
            return json.loads(stdout.getvalue())["review_progress"]

        observation = {"id": "batch-1", "elapsed_ms": 100, "model_input_tokens": 200}
        progress = publish(record, observation)
        self.assertEqual(progress["remaining_bytes"], 2)
        self.assertEqual(progress["reviewed_files"], 0)
        self.assertEqual(progress["forecast"]["estimated_remaining_read_ms"], 100)
        self.assertEqual(publish(record, observation), progress)
        with self.assertRaisesRegex(ValueError, "different contents"):
            publish(record, {**observation, "elapsed_ms": 200})
        before = state_path.read_bytes()
        with self.assertRaisesRegex(ValueError, "complete read coverage"):
            publish({**record, "disposition": "reviewed", "reason": "no issue"})
        self.assertEqual(state_path.read_bytes(), before)
        progress = publish({**record, "ranges": [[2, 4]], "disposition": "reviewed", "reason": "complete source review", "findings": ["F1"]},
                           {"id": "batch-2", "elapsed_ms": 300})
        self.assertTrue(progress["complete"])
        self.assertEqual(progress["next_batch"], [])
        self.assertEqual(progress["forecast"]["observed_elapsed_ms"], 400)
        self.assertTrue(progress["forecast"]["read_throughput_declining"])
        self.assertTrue(publish(record, observation)["complete"])
        restored = json.loads(state_path.read_text(encoding="utf-8"))
        self.assertEqual(restored["review"]["a.md"]["findings"], ["F1"])
        self.file("a.md", "changed")
        _, refreshed = inventory.inventory(self.root, initial["query"], restored, refresh=True)
        self.assertEqual(refreshed["review"], {})
        self.assertEqual(len(refreshed["stale_review"]), 1)
        self.assertFalse(refreshed["output"]["review_progress"]["complete"])

    def test_default_runs_are_unique_across_queries_and_repositories(self):
        self.file("a.md")
        query = {"categories": [{"name": "docs", "paths": ["*.md"],
                                 "verification": "path"}]}
        other = Path(self.temp.name) / "other"
        other.mkdir()
        subprocess.run(["git", "init", "-q", str(other)], check=True, capture_output=True)
        results = [self.scan_stdin(query), self.scan_stdin(query),
                   self.scan_stdin(query, root=other)]
        self.assertEqual(len({r["artifact"] for r in results}), 3)
        self.assertEqual([r["count"] for r in results], [1, 1, 0])
        for result in results:
            state = Path(result["artifact"])
            self.assertEqual(state.name, "state.json")
            self.assertEqual(state.parent.parent, Path(self.temp.name))
            self.assertTrue(state.parent.name.startswith("source-inventory-"))
            self.assertEqual(Path(result["report"]).parent.name, result["scan_epoch"])
            for key in ("artifact", "report", "canonical_paths"):
                self.assertTrue(Path(result[key]).is_absolute())
                self.assertTrue(Path(result[key]).is_file())
            self.assertEqual(result["next_action"], "deliver_report")
        self.assertEqual(results[0]["report"], results[1]["report"])
        self.assertEqual(results[0]["scan_epoch"], results[1]["scan_epoch"])
        self.assertNotEqual(results[0]["report"], results[2]["report"])
        (other / "a.md").write_bytes((self.root / "a.md").read_bytes())
        subprocess.run(["git", "-C", str(other), "add", "a.md"], check=True, capture_output=True)
        isolated = self.scan_stdin(query, root=other)
        self.assertEqual(isolated["report"], results[0]["report"])
        self.assertEqual(isolated["canonical_paths"], results[0]["canonical_paths"])

    def test_default_state_honors_explicit_report(self):
        self.file("a.md")
        query = {"categories": [{"name": "docs", "paths": ["*.md"],
                                 "verification": "path"}]}
        report = Path(self.temp.name) / "chosen.md"
        result = self.scan_stdin(query, "--report", str(report))
        self.assertEqual(Path(result["report"]).read_bytes(), report.read_bytes())
        self.assertFalse((Path(result["artifact"]).parent / "inventory.md").exists())

    def test_stdin_continuation_advances_evidence_and_can_render_without_rescan(self):
        self.file("a.md", "aaaa")
        self.file("b.md", "bbbb")
        query = {"categories": [{"name": "docs", "paths": ["*.md"],
                                 "verification": "path"}]}
        with mock.patch.object(inventory, "MAX_SCAN_BYTES", 4):
            first = self.scan_stdin(query, "--paths")
            self.assertEqual(first["scan_pending"], 1)
            self.assertEqual(first["paths"], ["a.md"])
            report = Path(first["artifact"]).parent / "inventory.md"
            second = self.scan_stdin(
                query, "--state", first["artifact"], "--report", str(report), "--paths",
            )
        fresh, _ = inventory.inventory(self.root, query)
        self.assertEqual(second["scan_epoch"], fresh["scan_epoch"])
        self.assertEqual(second["scan_pending"], 0)
        self.assertEqual(second["paths"], ["a.md", "b.md"])
        self.assertEqual(second["source_bytes_read"], 4)
        self.assertEqual(second["next_action"], "deliver_report")
        saved = Path(second["artifact"]).read_bytes()
        with (mock.patch.object(inventory, "inventory", side_effect=AssertionError("rescan")),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            inventory.main(["--state", second["artifact"], "--render-only", "--paths"])
        self.assertEqual(json.loads(stdout.getvalue())["paths"], second["paths"])
        self.assertEqual(Path(second["artifact"]).read_bytes(), saved)

    def test_snapshot_identity_includes_negative_evidence_and_rejects_epoch_drift(self):
        source = self.file("a.md", "prompt")
        negative = self.file("b.md", "absent")
        query = {"categories": [{"name": "prompts", "paths": ["*.md"],
                                 "contains": "prompt", "verification": "path"}]}
        first = self.scan_stdin(query)
        second = self.scan_stdin(query)
        self.assertEqual(first["source_snapshot_sha256"], second["source_snapshot_sha256"])
        self.assertEqual(first["delivery_sha256"], second["delivery_sha256"])
        negative.write_text("still absent", encoding="utf-8")
        changed = self.scan_stdin(query)
        self.assertEqual(first["count"], changed["count"])
        self.assertNotEqual(first["source_snapshot_sha256"], changed["source_snapshot_sha256"])
        document = json.loads(Path(changed["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual([r["path"] for r in document["sources"]], ["a.md", "b.md"])

        with mock.patch.object(inventory, "MAX_SCAN_BYTES", 6):
            pending = self.scan_stdin(query)
        self.assertGreater(pending["scan_pending"], 0)
        self.assertNotIn("source_snapshot_sha256", pending)
        saved = Path(pending["artifact"]).read_bytes()
        source.write_text("changed prompt", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "source changed.*--refresh"):
            self.scan_stdin(query, "--state", pending["artifact"])
        self.assertEqual(Path(pending["artifact"]).read_bytes(), saved)
        refreshed = self.scan_stdin(query, "--state", pending["artifact"], "--refresh")
        self.assertEqual(refreshed["scan_pending"], 0)
        self.assertNotEqual(refreshed["scan_epoch"], pending["scan_epoch"])

    def test_stdin_malformed_or_non_utf8_query_does_not_create_run(self):
        for raw in (b"", b"{", b"\xff"):
            with self.subTest(raw=raw):
                stream = io.TextIOWrapper(io.BytesIO(raw))
                with (stream, mock.patch.object(sys, "stdin", stream),
                      mock.patch.object(tempfile, "mkdtemp") as create,
                      self.assertRaises(ValueError)):
                    inventory.main(["--query", "-"])
                create.assert_not_called()

    def test_stdin_accepts_utf8_bom(self):
        query = {"categories": [{"name": "docs", "paths": ["*.md"],
                                 "verification": "path"}]}
        stream = io.TextIOWrapper(io.BytesIO(b"\xef\xbb\xbf" + json.dumps(query).encode()))
        with (stream, mock.patch.object(sys, "stdin", stream),
              mock.patch.object(tempfile, "tempdir", self.temp.name),
              contextlib.redirect_stdout(io.StringIO())):
            self.assertEqual(inventory.main(["--root", str(self.root), "--query", "-"]), 0)

    def test_stdin_controls_still_protect_sources_and_state(self):
        self.file("a.md")
        query = {"categories": [{"name": "docs", "paths": ["*.md"],
                                 "verification": "path"}]}
        state = Path(self.temp.name) / "state.json"
        with self.assertRaisesRegex(ValueError, "state file overlaps"):
            self.scan_stdin(query, "--state", str(self.root / "state.md"))
        for report in (state, self.root / "a.md"):
            with (self.subTest(report=report), contextlib.redirect_stderr(io.StringIO()),
                  self.assertRaises(SystemExit)):
                self.scan_stdin(query, "--state", str(state), "--report", str(report))
            self.assertFalse(state.exists())
        self.assertEqual((self.root / "a.md").read_text(), "prompt evidence")
        with (contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit),
              mock.patch.object(tempfile, "mkdtemp") as create):
            inventory.main(["--render-only"])
        create.assert_not_called()

    def test_describe_exposes_complete_contract_without_io(self):
        with (mock.patch.object(inventory, "inventory", side_effect=AssertionError("scan")),
              mock.patch.object(Path, "read_text", side_effect=AssertionError("read")),
              mock.patch.object(tempfile, "mkdtemp", side_effect=AssertionError("temp")),
              mock.patch.object(sys, "stdin", None),
              mock.patch.object(inventory, "MAX_FILE_BYTES", 123),
              mock.patch.object(inventory, "MAX_SCAN_BYTES", 456),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            self.assertEqual(inventory.main(["--describe"]), 0)
        contract = json.loads(stdout.getvalue())
        self.assertEqual(contract["budget"]["max_file_bytes"], 123)
        self.assertEqual(contract["budget"]["max_scan_bytes"], 456)
        self.assertIn("--query -", contract["invocation"]["powershell_stdin"])
        self.assertIn("$OutputEncoding", contract["invocation"]["powershell_stdin"])
        self.assertIn("--state STATE --report REPORT", contract["invocation"]["file"])
        self.assertIn("fnmatch.fnmatchcase", contract["globs"])
        self.assertIn("re.search", contract["contains"])
        self.assertIn("--refresh", contract["invocation"]["refresh"])
        self.assertIn("--render-only", contract["paging"])
        self.assertIn("unique", contract["control_files"])
        self.assertIn("not that its scope answers the entire task", contract["delivery"])

    def test_documented_powershell_stdin_preserves_unicode_scope_and_failure_status(self):
        shells = [path for name in ("powershell", "pwsh") if (path := shutil.which(name))]
        if not shells:
            self.skipTest("PowerShell is not available")
        self.file("src/templates/café.md", "日本語")
        example = inventory.describe_contract()["invocation"]["powershell_stdin"]
        example = example.replace('"name":"templates"', '"name":"日本語"')
        example = example.replace(
            "python -X utf8", "& '" + sys.executable.replace("'", "''") + "' -X utf8",
        ).replace("--root .", "--root '" + str(self.root).replace("'", "''") + "'")
        script = (
            "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n"
            "$OutputEncoding = [System.Text.ASCIIEncoding]::new()\n"
            "$before = $OutputEncoding\n"
            "$result = " + example + "\n"
            "if ($OutputEncoding -ne $before) { throw 'OutputEncoding leaked' }\n"
            "$result\n"
        )
        for shell in shells:
            for invalid_regex in (False, True):
                with self.subTest(shell=shell, invalid_regex=invalid_regex):
                    command = script
                    if invalid_regex:
                        command = command.replace(
                            '"verification":"path"', '"verification":"path","contains":"(?i)("',
                        )
                    result = subprocess.run(
                        [shell, "-NoProfile", "-NonInteractive", "-EncodedCommand",
                         base64.b64encode(command.encode("utf-16-le")).decode("ascii")],
                        cwd=Path(inventory.__file__).resolve().parent.parent,
                        env={**os.environ, "TEMP": self.temp.name, "TMP": self.temp.name,
                             "TMPDIR": self.temp.name},
                        capture_output=True, encoding="utf-8",
                    )
                    if invalid_regex:
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("unterminated subpattern", result.stderr)
                        self.assertEqual(result.stdout.strip(), "")
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        summary = json.loads(result.stdout)
                        self.assertEqual(summary["category_counts"], {"日本語": 1})
                        canonical = json.loads(Path(summary["canonical_paths"]).read_text(encoding="utf-8"))
                        self.assertEqual(canonical["paths"], ["src/templates/café.md"])

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
        with mock.patch("scripts.source_inventory.subprocess.run", wraps=real_run) as run:
            output, state = inventory.inventory(self.root, query)
        # Discovery and the end-of-scan source-set fence both prune before descent.
        self.assertEqual(run.call_count, 2)
        for call in run.call_args_list:
            argv = call.args[0]
            self.assertIn("--exclude=target/", argv)
            self.assertIn("--exclude=node_modules/", argv)
        self.assertEqual(output["paths"], ["src/tracked.rs"])
        self.assertEqual(output["untracked_paths"], ["src/new name.rs"])
        records = {r["path"]: r for r in state["records"]}
        self.assertEqual(records["src/gone.rs"]["tracking"], "deleted")
        self.assertFalse(records["src/gone.rs"]["exists"])
        self.assertIsNone(records["nested/target/deep/build.rs"]["exists"])
        self.assertNotIn("nested/target/deep/build.rs", state["files"])

    def test_worktree_deletion_is_reported_without_blocking_delivery(self):
        self.file("src/kept.rs")
        self.file("src/gone.rs").unlink()
        query = {"categories": [{"name": "sources", "paths": ["src/*.rs"],
                                 "verification": "path"}]}
        summary = self.scan_stdin(query)
        self.assertEqual(summary["count"], 1)
        self.assertEqual(summary["unresolved_count"], 0)
        self.assertEqual(summary["deleted_count"], 1)
        self.assertTrue(summary["ready_to_render"])
        self.assertEqual(summary["next_action"], "deliver_report")
        delivered = json.loads(Path(summary["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual(delivered["deleted"], ["src/gone.rs"])
        self.assertNotIn("src/gone.rs", delivered["paths"])
        report = Path(summary["report"]).read_text(encoding="utf-8")
        self.assertIn("Deleted in the working tree", report)
        self.assertIn("src/gone.rs", report)
        # The deletion is part of the snapshot identity: restoring it changes it.
        self.git("checkout", "--", "src/gone.rs")
        restored = self.scan_stdin(query)
        self.assertNotIn("deleted_count", restored)
        self.assertNotEqual(restored["source_snapshot_sha256"], summary["source_snapshot_sha256"])

    def test_short_remainder_is_returned_without_another_call(self):
        self.file("src/a.rs")
        summary = self.scan_stdin(
            {"categories": [{"name": "runtime", "paths": ["src/*.rs"]}]})
        self.assertEqual(summary["unresolved_count"], 1)
        self.assertEqual(
            [(r["path"], r["unresolved"]) for r in summary["remaining"]],
            [("src/a.rs", "runtime consumer requires inspection")],
        )
        self.assertIsNone(summary["next_offset"])

    def test_delivered_scope_is_listed_and_reproducible(self):
        self.file("src/a.rs")
        self.file("docs/b.md")
        first = self.scan_stdin(
            {"categories": [{"name": "rust", "paths": ["*.rs"], "verification": "path"}]})
        second = self.scan_stdin(
            {"categories": [{"name": "docs", "paths": ["*.md"], "verification": "path"}]})
        self.assertNotIn("prior_queries", first)
        self.assertEqual(
            [(p["query_id"], p["categories"], p["canonical_paths"])
             for p in second["prior_queries"]],
            [(first["query_id"], ["rust"], first["canonical_paths"])],
        )
        with (mock.patch.object(tempfile, "tempdir", self.temp.name),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            self.assertEqual(inventory.main([
                "--root", str(self.root), "--query", first["canonical_paths"],
            ]), 0)
        reproduced = json.loads(stdout.getvalue())
        self.assertEqual(reproduced["query_id"], first["query_id"])
        self.assertEqual(reproduced["count"], first["count"])
        self.assertEqual(reproduced["prior_queries"][0]["query_id"], second["query_id"])

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

    def test_equivalent_glob_paths_match_and_reuse_the_same_scope(self):
        self.file("src/a.rs", "prompt")
        state = None
        query_id = None
        for pattern in ["src/*.rs", "src\\*.rs", "./src/*.rs"]:
            with self.subTest(pattern=pattern):
                query = {"categories": [{"name": "prompts", "paths": [pattern],
                                          "contains": "prompt", "verification": "path"}]}
                output, state = inventory.inventory(self.root, query, state)
                self.assertEqual(output["paths"], ["src/a.rs"])
                self.assertTrue(output["ready_to_render"])
                self.assertEqual(query["categories"][0]["paths"], [pattern])
                if query_id is not None:
                    self.assertEqual(output["query_id"], query_id)
                    self.assertEqual(output["searched_records"], 0)
                    self.assertEqual(output["reused_records"], 1)
                query_id = output["query_id"]

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

    def test_cli_retains_records_and_keeps_summary_with_exact_paths(self):
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
        path_summary = json.loads(stdout.getvalue())
        self.assertEqual(path_summary["paths"], ["src/a.rs"])
        self.assertIsNone(path_summary["paths_next_offset"])
        self.assertEqual(path_summary["count"], 1)
        self.assertEqual(path_summary["query_id"], summary["query_id"])
        self.assertTrue(path_summary["ready_to_render"])
        self.assertEqual(path_summary["format"], "source_inventory_result_v1")
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
        with mock.patch("scripts.source_inventory.subprocess.run", wraps=real_run) as run:
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
        self.assertEqual(fields["base_instructions"], {"type": "string", "length": len(body), "sha256": inventory.digest(body.encode())})
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

    def test_public_contract_example_returns_successful_json_evidence_and_delivery(self):
        with (mock.patch.object(inventory, "inventory", side_effect=AssertionError("must not scan")),
              mock.patch.object(Path, "read_text", side_effect=AssertionError("must not read state")),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            self.assertEqual(inventory.main(["--describe"]), 0)
        contract = json.loads(stdout.getvalue())
        self.assertEqual(contract["format"], "source_inventory_result_v1")
        self.assertIn("json_summaries", contract["result"])
        self.file("src/templates/a.md")
        body = "PRIVATE PROMPT CONTENT" * 2000
        catalog = self.file("src/models.json", json.dumps({"models": [{"prompt": body}]}))
        # File listings do not need JSON parsing. Opt in only for the catalog
        # when this caller actually needs structural evidence.
        definition = contract["example"]
        for category in definition["categories"]:
            if category["name"] == "catalog":
                category["json_summary"] = True
        query = Path(self.temp.name) / "query.json"
        query.write_text(json.dumps(definition), encoding="utf-8")
        state = Path(self.temp.name) / "state.json"
        report = Path(self.temp.name) / "report.md"
        result = subprocess.run([sys.executable, str(Path(inventory.__file__).resolve()),
                                 "--root", str(self.root), "--query", str(query),
                                 "--state", str(state), "--report", str(report), "--paths"],
                                check=True, capture_output=True, text=True)
        summary = json.loads(result.stdout)
        self.assertEqual(summary["paths"], ["src/models.json", "src/templates/a.md"])
        self.assertEqual(summary["count"], 2)
        self.assertTrue(summary["ready_to_render"])
        self.assertEqual(summary["next_action"], "deliver_report")
        self.assertEqual(summary["json_summary_count"], 1)
        self.assertIsNone(summary["json_summary_next_offset"])
        evidence = summary["json_summaries"][0]
        self.assertEqual((evidence["path"], evidence["category"], evidence["status"]),
                         ("src/models.json", "catalog", "matched"))
        self.assertEqual(evidence["sha256"], inventory.digest(catalog.read_bytes()))
        self.assertEqual(evidence["structure"]["fields"]["models"]["items"]["0"]["fields"]["prompt"],
                         {"type": "string", "length": len(body), "sha256": inventory.digest(body.encode())})
        delivered = json.loads(Path(summary["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual(delivered["json_summaries"], summary["json_summaries"])
        self.assertEqual(delivered["paths"], summary["paths"])
        for text in (result.stdout, Path(summary["canonical_paths"]).read_text(encoding="utf-8"), report.read_text(encoding="utf-8")):
            self.assertNotIn("PRIVATE PROMPT CONTENT", text)
        self.assertLess(len(result.stdout), 3000)

    def test_describe_example_discovers_unknown_layout_in_one_stdin_scan(self):
        with (mock.patch.object(inventory, "repository_source_records",
                                side_effect=AssertionError("describe must not discover paths")),
              mock.patch.object(Path, "read_text",
                                side_effect=AssertionError("describe must not read files")),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            self.assertEqual(inventory.main(["--describe"]), 0)
        contract = json.loads(stdout.getvalue())
        self.assertIn("describe -> scan -> final", contract["workflow"]["default"])
        self.assertIn("same execution cell", contract["workflow"]["default"])
        self.assertIn('// @exec: {"deliver": true}', contract["delivery"])
        self.assertIn("streams_complete", contract["delivery"])
        self.assertIn("scan_pending and unresolved_count are zero", contract["delivery"])
        self.assertIn("await yield_control()", contract["delivery"])
        self.assertIn("not that its scope answers the entire task", contract["delivery"])
        self.assertIn("restricted request", contract["workflow"]["scope"])
        self.assertIn("uncertainty", contract["workflow"]["inspection_exception"])
        assets = {
            "AGENTS.md": "Keep changes small.",
            "models.json": "{}",
            "packages/unknown/deep/models.json": "{}",
            "packages/unknown/deep/skills/demo/SKILL.md": "Use the local runner.",
            "packages/unknown/deep/skills/demo/references/usage.md": "Run with --quiet.",
            "packages/unknown/deep/templates/nested/a.md": "Answer briefly.",
            "templates/a.md": "Be concise.",
        }
        references = {
            "config/settings.toml": 'prompt = "example"',
            "config/settings.yaml": "prompt: example",
            "docs/prompts.md": "Documents a prompt consumer, not a definition.",
            "src/consumer.rs": "load_prompt();",
        }
        for path, body in {**assets, **references}.items():
            self.file(path, body)
        self.file("packages/unknown/deep/not-models.json", "{}")
        self.file("docs/a.md", "Build with cargo.")
        self.file("tui/frames/frame_01.txt", "....")
        self.file("tests/fixtures/patch.txt", "*** Begin Patch\n*** End Patch")
        definition = contract["example"]
        self.assertNotIn("required_categories", definition)
        result = self.scan_stdin(definition)
        expected = sorted([*assets, *references])
        self.assertEqual(result["count"], len(expected))
        self.assertEqual(result["category_counts"], {
            "templates": 2, "catalog": 2, "guidance_assets": 3,
            "related_text_candidates": 4,
        })
        self.assertEqual(result["scan_pending"], 0)
        self.assertEqual(result["unresolved_count"], 0)
        self.assertEqual(result["missing_categories"], [])
        self.assertNotIn("json_summaries", result)
        self.assertTrue(result["ready_to_render"])
        self.assertEqual(result["next_action"], "deliver_report")
        delivered = json.loads(Path(result["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual(delivered["paths"], expected)
        self.assertEqual(delivered["categories"]["related_text_candidates"], sorted(references))
        primary = {
            path for name in ("templates", "catalog", "guidance_assets")
            for path in delivered["categories"][name]
        }
        self.assertEqual(primary, set(assets))

    def test_successful_evidence_excludes_unresolved_and_reviewed_exclusions(self):
        self.file("include.json", '{"prompt":"included"}')
        self.file("exclude.json", '{"prompt":"excluded"}')
        self.file("unresolved.json", '{"prompt":"unknown"}')
        self.file("consumer.rs", "load_prompt();")
        query = {"categories": [{"name": "runtime", "paths": ["*.json"], "json_summary": True}],
                 "decisions": [self.decision("include.json", "consumer.rs"),
                               self.decision("exclude.json", "consumer.rs", "exclude")]}
        _, state = inventory.inventory(self.root, query)
        state_path = Path(self.temp.name) / "state.json"
        state_path.write_text(json.dumps(state), encoding="utf-8")
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            inventory.main(["--state", str(state_path), "--render-only", "--remaining"])
        summary = json.loads(stdout.getvalue())
        self.assertEqual([record["path"] for record in summary["json_summaries"]], ["include.json"])
        self.assertEqual(summary["json_summaries"][0]["status"], "verified")
        self.assertEqual([record["path"] for record in summary["remaining"]], ["unresolved.json"])
        self.assertFalse(summary["ready_to_render"])

    def test_json_evidence_pages_reuse_snapshot_with_exact_coverage_and_byte_target(self):
        # One source in many categories exercises both byte and record bounds.
        source = self.file("catalog.json", json.dumps({f"field_{index}": "body" * 500 for index in range(32)}))
        query = {"categories": [{"name": f"catalog_{index:02}", "paths": ["catalog.json"],
                                  "verification": "path", "json_summary": True} for index in range(55)]}
        _, state = inventory.inventory(self.root, query)
        state_path = Path(self.temp.name) / "state.json"
        state_path.write_text(json.dumps(state), encoding="utf-8")
        saved = state_path.read_bytes()
        source.unlink()
        seen = []
        offset = 0
        while offset is not None:
            with (mock.patch.object(inventory, "repository_source_records", side_effect=AssertionError("must not rescan")),
                  contextlib.redirect_stdout(io.StringIO()) as stdout):
                inventory.main(["--state", str(state_path), "--render-only", "--offset", str(offset)])
            page = json.loads(stdout.getvalue())
            self.assertEqual(page["json_summary_count"], 55)
            self.assertEqual(page["count"], 1)
            self.assertTrue(page["ready_to_render"])
            self.assertLessEqual(len(json.dumps(page["json_summaries"], ensure_ascii=False).encode("utf-8")), inventory.SUMMARY_PAGE_BYTES)
            seen.extend(record["category"] for record in page["json_summaries"])
            next_offset = page["json_summary_next_offset"]
            if next_offset is not None:
                self.assertGreater(next_offset, offset)
                self.assertLessEqual(next_offset - offset, inventory.PAGE_RECORDS)
            offset = next_offset
        self.assertEqual(seen, [f"catalog_{index:02}" for index in range(55)])
        self.assertEqual(state_path.read_bytes(), saved)

    def test_oversized_summary_without_report_explains_retained_recovery(self):
        self.file("catalog.json", '{"prompt":"private body"}')
        query = {"categories": [{"name": "catalog", "paths": ["catalog.json"],
                                  "verification": "path", "json_summary": True}]}
        _, state = inventory.inventory(self.root, query)
        state_path = Path(self.temp.name) / "state.json"
        state_path.write_text(json.dumps(state), encoding="utf-8")
        with (mock.patch.object(inventory, "SUMMARY_PAGE_BYTES", 1),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            inventory.main(["--state", str(state_path), "--render-only"])
        summary = json.loads(stdout.getvalue())
        self.assertNotIn("canonical_paths", summary)
        self.assertIsNone(summary["json_summary_next_offset"])
        self.assertIn("--report PATH", summary["json_summaries"][0]["structure"]["detail_omitted"])
        report = Path(self.temp.name) / "report.md"
        with (mock.patch.object(inventory, "repository_source_records", side_effect=AssertionError("must not rescan")),
              contextlib.redirect_stdout(io.StringIO()) as stdout):
            inventory.main(["--state", str(state_path), "--render-only", "--report", str(report)])
        delivery = json.loads(Path(json.loads(stdout.getvalue())["canonical_paths"]).read_text(encoding="utf-8"))
        self.assertEqual(delivery["json_summaries"][0]["structure"]["fields"]["prompt"],
                         {"type": "string", "length": len("private body"), "sha256": inventory.digest(b"private body")})

    def test_path_pages_keep_counts_readiness_and_report_links(self):
        for index in range(52):
            self.file(f"prompts/{index:02}.md")
        query = {"categories": [{"name": "prompts", "paths": ["prompts/*.md"], "verification": "path"}]}
        _, state = inventory.inventory(self.root, query)
        state_path = Path(self.temp.name) / "state.json"
        state_path.write_text(json.dumps(state), encoding="utf-8")
        report = Path(self.temp.name) / "report.md"
        seen = []
        for offset, expected_next in [(0, 50), (50, None)]:
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                inventory.main(["--state", str(state_path), "--render-only", "--paths",
                                "--report", str(report), "--offset", str(offset)])
            summary = json.loads(stdout.getvalue())
            self.assertEqual(summary["count"], 52)
            self.assertTrue(summary["ready_to_render"])
            self.assertEqual(summary["next_action"], "deliver_report")
            self.assertEqual(summary["paths_next_offset"], expected_next)
            self.assertTrue(Path(summary["canonical_paths"]).is_file())
            seen.extend(summary["paths"])
        self.assertEqual(seen, [f"prompts/{index:02}.md" for index in range(52)])

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

    def test_control_files_cannot_overlap_discovery_or_each_other(self):
        outside = Path(self.temp.name)
        for control in ("query", "state", "candidate", "same"):
            with self.subTest(control=control):
                query = self.root / "query.json" if control == "query" else outside / "query.json"
                state = self.root / "state.json" if control in ("state", "candidate") else outside / "state.json"
                definition = {"categories": [{"name": "json", "paths": ["./*.json"],
                                                "verification": "path"}]}
                if control == "candidate":
                    definition["categories"][0]["paths"] = ["src/*.json"]
                    definition["candidates"] = [{"path": "state.json", "category": "json"}]
                if control == "same":
                    state = query
                query.write_text(json.dumps(definition), encoding="utf-8")
                before = query.read_bytes()
                with (mock.patch.object(inventory, "inventory", side_effect=AssertionError("must reject before scan")),
                      contextlib.redirect_stderr(io.StringIO())):
                    if control == "same":
                        with self.assertRaises(SystemExit):
                            inventory.main(["--root", str(self.root), "--query", str(query), "--state", str(state)])
                    else:
                        with self.assertRaisesRegex(ValueError, f"{'state' if control == 'candidate' else control} file overlaps selected sources"):
                            inventory.main(["--root", str(self.root), "--query", str(query), "--state", str(state)])
                self.assertEqual(query.read_bytes(), before)
                if state != query:
                    self.assertFalse(state.exists())

    def test_control_files_outside_selection_or_in_pruned_trees_are_allowed(self):
        self.file("src/catalog.json", '{"prompt":"body"}')
        for pattern, controls in [("*.json", self.root / "target"),
                                  ("src/*.json", self.root / "controls")]:
            with self.subTest(pattern=pattern):
                controls.mkdir()
                query = controls / "query.json"
                state = controls / "state.json"
                query.write_text(json.dumps({"categories": [
                    {"name": "json", "paths": [pattern], "verification": "path"}]}), encoding="utf-8")
                with contextlib.redirect_stdout(io.StringIO()) as stdout:
                    self.assertEqual(inventory.main(["--root", str(self.root), "--query", str(query), "--state", str(state)]), 0)
                summary = json.loads(stdout.getvalue())
                self.assertEqual(summary["count"], 1)
                self.assertEqual(summary["untracked_count"], 0)
                self.assertTrue(summary["ready_to_render"])
                self.assertTrue(state.is_file())

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
