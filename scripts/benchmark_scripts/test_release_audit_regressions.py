"""Regressions based on rollout integrity, request identity and report contracts."""

from __future__ import annotations

import contextlib
import hashlib
import importlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts.benchmark_scripts import (
    cache_audit, cache_audit2, cache_audit3, cache_audit4,
    completed_turn_reasoning_measure as completed,
    context_exec_output_shape as shape,
    context_retirement_policy_model as retirement,
    extract_rollout_reasoning as extract,
    failure_audit, manifest_audit, reasoning_profile_ledger as ledger,
    reasoning_attribution_ledger as attribution,
    rolloutdetect_parse_corpus as corpus,
    validation_orchestration_audit as validation,
)
from scripts.benchmark_scripts.rollout_snapshot import discover_rollouts


def record(kind, payload):
    return {"type": kind, "payload": payload}


def usage(total, inputs=10000, cached=0):
    return record("event_msg", {"type": "token_count", "info": {
        "total_token_usage": {"input_tokens": total, "output_tokens": total // 1000},
        "last_token_usage": {"input_tokens": inputs, "cached_input_tokens": cached,
                             "output_tokens": inputs // 1000},
        "model_context_window": 100000,
    }})


class AuditRegressions(unittest.TestCase):
    def setUp(self):
        owned = tempfile.TemporaryDirectory()
        self.addCleanup(owned.cleanup)
        self.home = Path(owned.name)
        self.path = self.home / "rollout-test.jsonl"

    def write(self, records):
        self.path.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        return self.path

    def external(self, item):
        data = json.dumps(item).encode()
        digest = hashlib.sha256(data).hexdigest()
        root = self.home / "rollout-payloads"
        root.mkdir(exist_ok=True)
        (root / f"{digest}.json").write_bytes(data)
        return record("rollout_payload_artifact", {
            "sha256": digest, "bytes": len(data), "item_type": item["type"],
        })

    def test_attribution_missing_reasoning_is_not_a_zero_measurement(self):
        rows = [{"kind": "gen", "file": "session", "turnId": "turn", "generationIndex": i,
                 "attemptKind": "primary", "effort": "high", "purpose": "implementation",
                 "inputTokens": 10000, "reasoningTokens": value}
                for i, value in enumerate([None, 0, 12])]
        self.path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
        destination = self.home / "report.json"
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(attribution.main(["report", str(self.path), "--json", str(destination)]), 0)
        report = json.loads(destination.read_text())
        self.assertEqual(report["totals"]["unmeasured"], 1)
        self.assertEqual(report["distribution"]["zero"], 1)
        self.assertEqual(report["distribution"]["p50"], 12)
        self.assertEqual(report["context"]["doseResponse"][0]["mean"], 6)
        self.assertEqual(report["context"]["doseResponse"][0]["gens"], 2)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(attribution.context_analysis(rows[:1], [])["doseResponse"], [])
        self.path.write_text(json.dumps(rows[0]) + "\n", encoding="utf-8")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(attribution.main(["report", str(self.path), "--json", str(destination)]), 0)
        self.assertIsNone(json.loads(destination.read_text())["distribution"]["p50"])

    def test_attribution_spearman_uses_tied_ranks_and_excludes_constant_sessions(self):
        # Average ranks [1.5,1.5,3,4] and [1,2,3,4]: covariance 4.5,
        # squared deviations 4.5 and 5, giving 4.5/sqrt(22.5).
        self.assertAlmostEqual(attribution.spearman([1, 1, 2, 3], [1, 2, 3, 4]),
                               4.5 / (22.5 ** .5), places=6)
        self.assertIsNone(attribution.spearman([1, 1, 1], [1, 2, 3]))
        rows = [{"file": "constant", "turnId": "turn", "generationIndex": i,
                 "attemptKind": "primary", "purpose": "implementation",
                 "inputTokens": 10000, "reasoningTokens": i} for i in range(30)]
        with contextlib.redirect_stdout(io.StringIO()):
            within = attribution.context_analysis(rows, [])["withinSession"]
        self.assertEqual(within["sessions"], 0)
        self.assertEqual(within["undefinedSessions"], 1)
        self.assertIsNone(within["medianRho"])

    def test_attribution_compaction_window_does_not_bridge_unknown_usage(self):
        rows = [{"file": "session", "turnId": "turn", "generationIndex": i,
                 "attemptKind": "primary", "inputTokens": 10000,
                 "reasoningTokens": value, "purpose": "implementation"}
                for i, value in enumerate([20, None, 0, 5])]
        rows[2].update(purpose="compaction_recovery", reason="compaction")
        result = attribution.compaction_windows(attribution.turns_of(rows), 1)
        self.assertEqual(result["events"], 0)
        self.assertIsNone(result["meanReasoningBefore"])
        rows[1]["reasoningTokens"] = 0
        result = attribution.compaction_windows(attribution.turns_of(rows), 1)
        self.assertEqual(result["events"], 1)
        self.assertEqual(result["meanReasoningBefore"], 0)
        self.assertEqual(result["meanReasoningAfter"], 5)

    def test_corpus_failed_json_is_retried_and_legacy_failure_cache_is_not_success(self):
        import subprocess
        sessions = self.home / "sessions"
        sessions.mkdir()
        rollout = sessions / "rollout-one.jsonl"
        rollout.write_text("{}\n", encoding="utf-8")
        out = self.home / "summaries"
        args = ["--rolloutdetect", str(self.home), "--out", str(out), "--home", str(self.home),
                "--stable-seconds", "0", "--jobs", "1"]
        failed = subprocess.CompletedProcess([], 1, '{"partial":true}', "parser failed")
        target = out / "rollout-one.summary.json"
        with patch.object(corpus.subprocess, "run", return_value=failed) as run:
            for _ in range(2):
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(corpus.main(args), 1)
                self.assertFalse(target.exists(), "failed JSON is not a successful cache entry")
            self.assertEqual(run.call_count, 2)
        # Older versions did write failed JSON; failures.json must force retry.
        target.write_text('{"partial":true}', encoding="utf-8")
        with patch.object(corpus.subprocess, "run") as run, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(corpus.main(args + ["--stable-seconds", "1000000000"]), 1)
            run.assert_not_called()
        self.assertIn(str(rollout), json.loads((out / "failures.json").read_text()))
        success = subprocess.CompletedProcess([], 0, '{"complete":true}', "")
        with patch.object(corpus.subprocess, "run", return_value=success) as run:
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(corpus.main(args), 0)
                self.assertEqual(corpus.main(args), 0)
            self.assertEqual(run.call_count, 1, "only successful parses may be reused")
        self.assertEqual(json.loads(target.read_text()), {"complete": True})
        self.assertEqual(json.loads((out / "failures.json").read_text()), {})

    def test_ledger_rejects_input_and_hardlink_aliases_without_writing(self):
        self.write([usage(10000)])
        original = self.path.read_bytes()
        alias = self.home / "alias.jsonl"
        os.link(self.path, alias)
        for destination in (self.path, alias):
            with self.subTest(destination=destination), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit):
                    ledger.main([str(self.path), "--out", str(destination)])
            self.assertEqual(self.path.read_bytes(), original)

    def test_ledger_preserves_destination_on_read_or_publication_failure(self):
        destination = self.home / "ledger.jsonl"
        destination.write_bytes(b"existing ledger")
        self.path.write_text("{bad}\n")
        with self.assertRaises(ValueError):
            ledger.main([str(self.path), "--out", str(destination)])
        self.assertEqual(destination.read_bytes(), b"existing ledger")
        self.write([usage(10000)])
        with patch("scripts.common.atomic_json.shutil.copyfileobj", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                ledger.main([str(self.path), "--out", str(destination)])
        self.assertEqual(destination.read_bytes(), b"existing ledger")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(ledger.main([str(self.path), "--out", str(destination)]), 0)
        self.assertTrue(json.loads(destination.read_text())["summaryRow"])

    def test_custom_hydration_authenticates_size_hash_and_record_type(self):
        item = record("event_msg", {"type": "task_complete"})
        ref = self.external(item)
        for change in ({"bytes": 1}, {"item_type": "response_item"}, {"sha256": "../escape"}):
            bad = {**ref, "payload": {**ref["payload"], **change}}
            for resolve in (ledger.hydrate, extract.resolve):
                with self.subTest(change=change, resolve=resolve), self.assertRaises(ValueError):
                    resolve(bad, self.home)
        blob = self.home / "rollout-payloads" / (ref["payload"]["sha256"] + ".json")
        blob.write_bytes(blob.read_bytes().replace(b"task_complete", b"turn_complete"))
        with self.assertRaisesRegex(ValueError, "checksum"):
            ledger.hydrate(ref, self.home)

    def test_single_rollout_resolver_rejects_corrupt_artifacts(self):
        # Import under text streams: this standalone helper configures consoles.
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            audit = importlib.import_module("scripts.benchmark_scripts.audit_rollout_01a13c94")
        ref = self.external(record("event_msg", {"type": "task_complete"}))
        bad = {**ref, "payload": {**ref["payload"], "bytes": 0}}
        with patch.object(audit, "HOME", str(self.home)), self.assertRaises(ValueError):
            audit.resolve(bad)
        self.write([bad])
        stdout = io.StringIO()
        with patch.object(audit, "HOME", str(self.home)), patch(
            "sys.argv", ["audit", str(self.path), "skeleton"]
        ), contextlib.redirect_stdout(stdout), self.assertRaises(ValueError):
            audit.main()
        self.assertEqual(stdout.getvalue(), "")
        self.write([self.external(record("event_msg", {"type": "user_message", "message": "hydrated"}))])
        with patch.object(audit, "HOME", str(self.home)), patch(
            "sys.argv", ["audit", str(self.path), "skeleton"]
        ), contextlib.redirect_stdout(stdout):
            audit.main()
        self.assertIn("hydrated", stdout.getvalue())

    def test_unmeasured_causal_savings_are_not_reported_as_results(self):
        oscillation = importlib.import_module("scripts.benchmark_scripts.oscillation_cost")
        prefix = importlib.import_module("scripts.benchmark_scripts.prefix_vs_shorten")
        self.write([
            record("tool_manifest", {"hash": "a"}), usage(10000, cached=9000),
            record("tool_manifest", {"hash": "b"}), usage(20000, cached=9000),
            record("tool_manifest", {"hash": "a"}), usage(30000),
            record("compacted", {}), usage(40000),
        ])
        event = oscillation.scan(self.path)[0]
        self.assertNotIn("recoverable", event)
        self.assertIn("heuristic_overlap", event)
        for module, expected in [
            (oscillation, "unknown (no causal measurement)"),
            (prefix, "counterfactual preserved-prefix usage: unknown"),
            (cache_audit4, "cause and byte identity unverified"),
        ]:
            stdout = io.StringIO()
            with patch("sys.argv", [module.__name__, str(self.home)]), contextlib.redirect_stdout(stdout):
                self.assertEqual(module.main(), 0)
            self.assertIn(expected, stdout.getvalue())

    def test_context_readers_measure_external_payloads_identically_and_reject_missing(self):
        output = record("response_item", {"type": "function_call_output", "call_id": "c",
                        "output": '{"path":"/a","canonical_uri":"file:///a"}\nbody'})
        readers = {
            "context_projection_savings": lambda m: m.audit(self.path),
            "context_projection_recoverability": lambda m: list(m.enveloped_headers(self.path)),
            "context_exec_output_shape": lambda m: m.audit(self.path),
            "context_output_envelope_audit": lambda m: m.audit(self.path),
            "context_notice_volume": lambda m: m.audit(self.path),
            "context_tool_output_audit": lambda m: m.audit(self.path, 200),
            "context_window_composition": lambda m: m.audit(self.path),
            "context_retirement_policy_model": lambda m: m.load_session(self.path),
        }
        for name, read in readers.items():
            module = importlib.import_module("scripts.benchmark_scripts." + name)
            # Usage itself is external for the retirement model, which needs it.
            item = usage(10000) if name == "context_retirement_policy_model" else output
            self.write([item])
            inline = read(module)
            self.write([self.external(item)])
            with self.subTest(reader=name):
                self.assertEqual(read(module), inline)
                self.write([record("rollout_payload_artifact", {"sha256": "0" * 64,
                            "bytes": 1, "item_type": item["type"]})])
                with self.assertRaises(FileNotFoundError):
                    read(module)

    def test_compressed_projection_and_discovery(self):
        try:
            from compression import zstd
        except ImportError:
            from backports import zstd
        module = importlib.import_module("scripts.benchmark_scripts.context_projection_savings")
        self.write([record("response_item", {"type": "function_call_output",
                    "output": '{"path":"/a","canonical_uri":"file:///a"}\nbody'})])
        expected = module.audit(self.path)
        compressed = self.path.with_suffix(".jsonl.zst")
        compressed.write_bytes(zstd.compress(self.path.read_bytes()))
        self.assertEqual(discover_rollouts(self.home), [self.path])
        self.path.unlink()
        self.assertEqual(discover_rollouts(self.home), [compressed])
        observed = module.audit(compressed)
        observed["session"] = expected["session"]
        self.assertEqual(observed, expected)

    def test_retirement_discount_ends_at_baseline_retirement(self):
        # Linear pool: retire 15k on each 90k request, then baseline already
        # contains that 30k saving: 75k + 75k + 60k + 60k = 270k.
        session = {"series": [90000, 90000, 60000, 60000], "retirements": [2], "window": 100000}
        self.assertEqual(retirement.simulate(session, .7, 10000)["total"], 270000)

    def test_compaction_stops_unproven_reasoning_carriage(self):
        self.write([
            record("response_item", {"type": "reasoning", "summary": [{"text": "abcd"}],
                "internal_chat_message_metadata_passthrough": {"turn_id": "t"}}),
            record("event_msg", {"type": "task_complete", "turn_id": "t"}),
            usage(10000), record("compacted", {"replacement_history": []}), usage(20000),
        ])
        result = completed.analyze(self.path)
        self.assertEqual(result["hypothetical_token_requests_if_retained"], 1)
        self.assertNotIn("estimated_token_requests_carried", result)
        self.assertFalse(result["history_membership_complete"])
        self.assertEqual(result["requests_membership_unknown"], 1)

    def test_validation_repeats_remain_unverified_and_same_cell_edits_exclude(self):
        launch = validation.Generation(calls=["cargo test -p a"], outputs=["test result: ok"])
        different = validation.Generation(calls=["cargo test -p b"])
        result = validation.summarise(self.path, [launch, different])
        self.assertNotIn("unchanged_relaunches", result)
        self.assertEqual(result["relaunch_samples"][0]["redundancy"], "unverified")
        edited = validation.Generation(calls=["apply_patch(...); cargo test -p a"])
        self.assertEqual(validation.summarise(self.path, [launch, edited])["unverified_relaunches"], 0)

    def test_validation_malformed_and_partial_tail_fail_with_coverage(self):
        for damaged in ("{bad}\n", "{bad"):
            self.path.write_text(damaged)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(io.StringIO()):
                code = validation.main([str(self.home)])
            coverage = json.loads(stdout.getvalue())["coverage"]
            self.assertEqual((code, coverage["selected"], coverage["failed"]), (1, 1, 1))
            self.assertFalse(coverage["complete"])

    def test_repeated_cumulative_usage_is_not_another_request_or_route(self):
        first, second = usage(10000), usage(20000, cached=10000)
        # A changed stale last record is not new provider usage either.
        stale = usage(10000, inputs=9000)
        self.write([first, first, stale, record("compacted", {}), first, second])
        self.assertEqual(len(cache_audit.load(self.path).reqs), 2)
        self.assertEqual(cache_audit2.classify(self.path, 1024, .75), [])
        streams3, streams4 = cache_audit3.parse(self.path), cache_audit4.parse(self.path)
        self.assertEqual([len(s.reqs) for s in streams3], [2])
        self.assertEqual([len(s["reqs"]) for s in streams4], [2])
        self.assertIn("compaction", streams3[0].reqs[1]["causes"])
        self.assertIn("compaction", streams4[0]["reqs"][1]["causes"])

    def test_token_ledger_missing_total_preserves_request_identity_and_pending_items(self):
        # Import under a text stream: this standalone helper configures stdout.
        with contextlib.redirect_stdout(io.StringIO()):
            token_ledger = importlib.import_module("scripts.benchmark_scripts.reasoning_token_ledger")
        first = usage(10000)
        first["payload"]["info"]["last_token_usage"]["reasoning_output_tokens"] = 7
        second = usage(20000)
        del second["payload"]["info"]["last_token_usage"]
        output = record("response_item", {"type": "reasoning", "summary": []})
        tool = record("response_item", {"type": "function_call_output", "call_id": "c",
                                       "output": "diagnostic"})
        for missing_info in (None, {}, {"last_token_usage": first["payload"]["info"]["last_token_usage"]}):
            for output_before_missing in (False, True):
                with self.subTest(info=missing_info, output_before_missing=output_before_missing):
                    self.write([
                        first, tool, record("event_msg", {"type": "warning"}),
                        *([output] if output_before_missing else []),
                        record("event_msg", {"type": "token_count", "info": missing_info}),
                        first,
                        *([] if output_before_missing else [output]),
                        second,
                    ])
                    rows = token_ledger.analyze(self.path, {})
                    # Only changed cumulative usage establishes request identity.
                    self.assertEqual(len(rows), 2)
                    self.assertEqual([row["reasoning"] for row in rows], [7, None])
                    self.assertIsNone(rows[1]["input"], "known request need not have measured usage")
                    self.assertEqual(rows[1]["trigger"], {"tool_output": 1})
                    self.assertEqual(rows[1]["trigger_lines"], [2])
                    self.assertEqual(rows[1]["outputs"], {"reasoning": 1})
                    self.assertEqual(rows[1]["events"], ["warning"])

    def test_manifest_schema_change_is_not_serialization_churn(self):
        self.write([record("tool_manifest", {"tools": [{"name": "x", "parameters": {"type": t}}]})
                    for t in ("string", "integer")])
        self.assertEqual(manifest_audit.scan(self.path)[0]["kind"], "definition_changed")
        a, b = {"name": "a", "parameters": {}}, {"name": "b", "parameters": {}}
        self.write([record("tool_manifest", {"tools": tools}) for tools in ([a, b], [b, a])])
        self.assertEqual(manifest_audit.scan(self.path)[0]["kind"], "REORDER_ONLY")

    def test_only_earlier_outputs_contribute_line_overlap(self):
        line = "a" * 28
        one = record("response_item", {"type": "function_call_output", "call_id": "c", "output": line + "\n" + line})
        self.write([one])
        self.assertEqual(shape.audit(self.path)["line_repeat_tokens"], 0)
        self.write([one, one])
        self.assertEqual(shape.audit(self.path)["line_repeat_tokens"], 14)
        # Unattributed outputs are not equivalent commands.
        self.assertEqual(shape.audit(self.path)["cmd_repeat_identical_tokens"], 0)

    def test_failure_headers_keep_source_json_opaque_and_recognize_cell_errors(self):
        header = '{"exit_code":0,"output_lines":1}'
        source = '[source {"body":1,"field":"output","text_lines":1}]'
        body = '{"exit_code":1,"error":"ReferenceError: example"}'
        output = "\n".join((header, source, body))
        self.assertEqual(failure_audit._classify(output, ""), (None, 0))
        failure = '{"exit_code":1,"stderr":"No such file or directory"}'
        self.assertEqual(failure_audit._classify(output + "\n" + failure, ""),
                         ("path-not-found", 1))
        runtime = "Script failed with cell ID abc\nScript error:\nReferenceError: x is not defined"
        self.assertEqual(failure_audit._classify(runtime, ""), ("code-mode-script-error", 1))
        self.assertEqual(failure_audit._classify(
            header + "\n" + '[source {"text_lines":3}]' + "\n" + runtime, ""), (None, 0))

    def test_call_identity_preserves_context_and_noncommand_arguments(self):
        for first, second in [
            ({"cmd": "pwd", "workdir": "/a"}, {"cmd": "pwd", "workdir": "/b"}),
            ({"path": "a"}, {"path": "b"}),
            ({"argv": ["a b", "c"]}, {"argv": ["a", "b c"]}),
        ]:
            self.assertNotEqual(shape.normalized_command({"name": "tool", "arguments": first}),
                                shape.normalized_command({"name": "tool", "arguments": second}))


if __name__ == "__main__":
    unittest.main()
