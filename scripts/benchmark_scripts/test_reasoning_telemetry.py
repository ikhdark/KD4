"""Synthetic regressions for measured usage, hydration, and attribution limits."""

from __future__ import annotations

import contextlib
import hashlib
import io
import json
import re
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts.benchmark_scripts import (
    completed_turn_reasoning_measure as completed,
    extract_rollout_reasoning as extract,
    reasoning_efficiency_corpus as corpus,
    reasoning_profile_ledger as profile,
    reasoning_token_ledger as tokens,
)
from scripts.benchmark_scripts.test_release_audit_regressions import record, usage


class ReasoningTelemetryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)
        self.path = self.home / "rollout-one.jsonl"

    def write(self, records):
        self.path.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        return self.path

    def external(self, item):
        data = json.dumps(item).encode("utf-8")
        sha = hashlib.sha256(data).hexdigest()
        root = self.home / "rollout-payloads"
        root.mkdir(exist_ok=True)
        (root / f"{sha}.json").write_bytes(data)
        return record("rollout_payload_artifact", {
            "sha256": sha, "bytes": len(data), "item_type": item["type"],
        })

    def test_token_reader_hydrates_deduplicates_and_preserves_unknown_usage(self):
        first = usage(10000)
        records = [record("turn_context", {"turn_id": "t", "model": "m", "effort": "high"}),
                   first, first, record("event_msg", {"type": "token_count", "info": {
                       "total_token_usage": {"input_tokens": 20000}}})]
        self.write(records)
        inline = tokens.analyze(self.path, {})
        self.write([self.external(r) for r in records])
        self.assertEqual(tokens.analyze(self.path, {}), inline)
        self.assertEqual(len(inline), 2, "unchanged cumulative totals are repeated emissions")
        self.assertIsNone(inline[0]["reasoning"], "omitted reasoning is not reported zero")
        self.assertIsNone(inline[1]["input"])
        self.assertIsNone(inline[1]["output"])

    def test_quoted_failure_is_a_textual_mention_not_an_incident(self):
        self.write([record("response_item", {"type": "function_call_output", "output":
            '[source {"path":"audit.txt"}]\nExample: Raw Cargo launch rejected'}), usage(10000)])
        markers = {name: re.compile(pattern, re.M) for name, pattern in tokens.DEFAULT_MARKERS.items()}
        row = tokens.analyze(self.path, markers)[0]
        self.assertEqual(row["marker_mentions"], {"cargo_admission_rejected": 1})
        self.assertNotIn("markers", row)
        stdout = io.StringIO()
        with patch.object(tokens, "rollout_paths", return_value=[self.path]), patch(
            "sys.argv", ["ledger"]
        ), contextlib.redirect_stdout(stdout):
            self.assertEqual(tokens.main(), 0)
        self.assertIn("not verified incidents", stdout.getvalue())
        self.assertIn("missing_reasoning_usage=1", stdout.getvalue())

    def test_all_readers_fail_closed_on_missing_corrupt_and_malformed_evidence(self):
        readers = [lambda: tokens.analyze(self.path, {}), lambda: completed.analyze(self.path),
                   lambda: corpus.rollout_settings(self.path),
                   lambda: list(profile.rows_for(self.path, self.home))]
        ref = self.external(record("event_msg", {"type": "task_complete", "turn_id": "t"}))
        blob = self.home / "rollout-payloads" / (ref["payload"]["sha256"] + ".json")
        original = blob.read_bytes()
        self.write([ref])
        blob.write_bytes(original.replace(b"task_complete", b"turn_complete"))
        for read in readers:
            with self.subTest(read=read, damage="checksum"), self.assertRaisesRegex(ValueError, "checksum"):
                read()
        blob.unlink()
        for read in readers:
            with self.subTest(read=read, damage="missing"), self.assertRaises(FileNotFoundError):
                read()
        self.path.write_text('{bad}\n', encoding="utf-8")
        for read in readers:
            with self.subTest(read=read, damage="malformed"), self.assertRaises(ValueError):
                read()

    def test_extractor_cli_hydrates_and_rejects_corrupt_payload(self):
        ref = self.external(record("event_msg", {"type": "task_complete", "turn_id": "t"}))
        self.write([ref])
        stderr = io.StringIO()
        args = ["extract", str(self.path), "--home", str(self.home), "--mode", "raw-types"]
        with patch("sys.argv", args), contextlib.redirect_stderr(stderr):
            self.assertEqual(extract.main(), 0)
        self.assertIn("event_msg/task_complete", stderr.getvalue())
        self.write([{**ref, "payload": {**ref["payload"], "bytes": 1}}])
        with patch("sys.argv", args), self.assertRaisesRegex(ValueError, "size mismatch"):
            extract.main()

    def test_completed_exposure_is_hypothetical_deduplicated_and_cache_is_paired(self):
        first = usage(10000, inputs=1000, cached=900)
        second = usage(20000, inputs=2000, cached=100)
        self.write([record("response_item", {"type": "reasoning", "summary": [{"text": "abcd"}],
                   "internal_chat_message_metadata_passthrough": {"turn_id": "t"}}),
                   self.external(record("event_msg", {"type": "task_complete", "turn_id": "t"})),
                   first, first, second])
        result = completed.analyze(self.path)
        # Four summary bytes estimate one token, across two distinct usage events.
        self.assertEqual(result["hypothetical_token_requests_if_retained"], 2)
        self.assertEqual(result["requests_total"], 2)
        self.assertNotIn("estimated_token_requests_carried", result)
        self.assertFalse(result["history_membership_complete"])
        self.assertEqual(result["measured_peak_cached_input_tokens"], 100)
        self.assertEqual(result["measured_cached_share_of_peak"], 100 / 2000)

    def test_completed_missing_input_does_not_claim_zero_peak(self):
        self.write([record("event_msg", {"type": "task_complete", "turn_id": "t"}),
                    record("event_msg", {"type": "token_count", "info": {
                        "total_token_usage": {"output_tokens": 1}}})])
        result = completed.analyze(self.path)
        self.assertEqual(result["requests_missing_input_usage"], 1)
        self.assertIsNone(result["measured_peak_input_tokens"])
        self.assertIsNone(result["measured_cached_share_of_peak"])

    def test_profiles_require_terminal_unless_explicit_and_do_not_borrow_settings(self):
        timing = {"modelRequests": [{"generationIndex": 0, "outputTokens": 0,
                                   "reasoningOutputTokens": 0}]}
        self.write([record("turn_context", {"turn_id": "other", "model": "m", "effort": "max"}),
                    record("sampling_boundary", {"turn_id": "live", "timing_checkpoint": {"timing": timing}}),
                    self.external(record("event_msg", {"type": "task_complete", "turn_id": "done", "timing": timing}))])
        rows = list(profile.rows_for(self.path, self.home))
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["profileSource"], "terminal")
        self.assertEqual(rows[0]["settingsStatus"], "missing")
        self.assertIsNone(rows[0]["effort"])
        self.assertIsNone(rows[0]["reasoningOutputTokens"])
        self.assertIsNone(rows[0]["outputTokens"])
        self.assertEqual(rows[-1]["turnsCheckpointOnly"], 1)
        self.assertEqual(rows[-1]["requestsMissingReasoningUsage"], 1)
        partial = list(profile.rows_for(self.path, self.home, include_checkpoints=True))
        self.assertEqual([r["profileSource"] for r in partial[:-1]], ["terminal", "checkpoint"])

    def test_conflicting_turn_settings_and_missing_terminal_profile_stay_unknown(self):
        self.write([record("turn_context", {"turn_id": "t", "model": "m", "effort": e})
                    for e in ("high", "max")] + [
            record("event_msg", {"type": "task_complete", "turn_id": "t", "timing": {
                "modelRequests": [{"tokenUsage": {"reasoningTokens": 3, "visibleOutputTokens": 2}}]}}),
            record("sampling_boundary", {"turn_id": "no-profile", "timing_checkpoint": {
                "timing": {"modelRequests": [{}]}}}),
            record("event_msg", {"type": "turn_aborted", "turn_id": "no-profile"}),
        ])
        rows = list(profile.rows_for(self.path, self.home, include_checkpoints=True))
        self.assertEqual(len(rows), 2, "a terminal with no profile must not masquerade as checkpoint-only")
        self.assertEqual(rows[0]["settingsStatus"], "conflicting")
        self.assertIsNone(rows[0]["effort"])
        self.assertEqual(rows[0]["reasoningOutputTokens"], 3)
        self.assertEqual(rows[0]["outputTokens"], 5)
        self.assertEqual(rows[-1]["terminalProfilesMissing"], 1)

    def test_corpus_hydrates_settings_and_never_uses_dominant_effort(self):
        records = [record("turn_context", {"model": "m", "effort": e})
                   for e in ("high", "high", "max")]
        self.write([self.external(r) for r in records])
        settings = corpus.rollout_settings(self.path)
        self.assertEqual(settings["efforts"], {"high": 2, "max": 1})
        self.assertEqual(corpus.model_effort_key(settings), "mixed/unattributed")
        self.assertEqual(corpus.model_effort_key({}), "unknown/unattributed")
        summary = {"populations": {"all": {"tokens": {"observedTotals": {"outputTokens": 7}}}}}
        (self.home / "rollout-one.summary.json").write_text(json.dumps(summary), encoding="utf-8")
        stdout = io.StringIO()
        with patch("sys.argv", ["corpus", "--summaries", str(self.home), "--sessions", str(self.home)]), contextlib.redirect_stdout(stdout):
            self.assertEqual(corpus.main(), 0)
        result = json.loads(stdout.getvalue())
        self.assertIsNone(result["corpus"]["reasoning"])
        self.assertEqual(result["by_model_effort"]["mixed/unattributed"]["output"], 7)
        self.assertNotIn("m/high", result["by_model_effort"])


if __name__ == "__main__":
    unittest.main()
