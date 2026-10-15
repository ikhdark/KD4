"""Verified compatible fixes ported from RolloutDetect, through KD4 owning paths."""
from __future__ import annotations

import contextlib
import copy
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.benchmark_scripts import kd4_model_attempt_analysis as model
from scripts.benchmark_scripts import kd4_timing_analysis as timing
from scripts.benchmark_scripts import kd4_turn_latency_audit as audit
from scripts.benchmark_scripts import kd4_session_diagnostics as diagnostics
from scripts.benchmark_scripts import restore_rollout_artifact as restore
from scripts.benchmark_scripts import rollout_snapshot as snapshots
from scripts.benchmark_scripts import tool_result_audit as tools
from scripts.benchmark_scripts import kd4_first_useful_action_analysis as actions
from scripts.benchmark_scripts.test_kd4_model_inference import attempt
from scripts.benchmark_scripts.test_kd4_timing_analysis import timing_profile


class RolloutDetectPortsTest(unittest.TestCase):
    def rollout(self, rows, **options):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "rollout.jsonl"
            source.write_text("\n".join(map(json.dumps, rows)), encoding="utf-8")
            return audit.analyze_session_path(source, root, **options)

    @staticmethod
    def event(kind, turn="t", **fields):
        return {"type": "event_msg", "timestamp": "2026-01-01T00:00:00Z",
                "payload": {"type": kind, "turn_id": turn, **fields}}

    def test_epoch_microseconds_are_exact_integer_nanoseconds(self):
        # Independent epoch arithmetic: one microsecond is exactly 1000 ns.
        self.assertEqual(audit._timestamp_ns("1970-01-01T00:00:00.000001Z"), 1000)
        base = audit._timestamp_ns("2026-10-14T00:00:00Z")
        self.assertEqual(audit._timestamp_ns("2026-10-14T00:00:00.000001Z") - base, 1000)

    def test_conflicting_terminal_error_cannot_be_collapsed_as_a_successful_replay(self):
        complete = self.event("task_complete", timing=timing_profile())
        failed = copy.deepcopy(complete)
        failed["payload"]["error"] = {"message": "failed after work"}
        for rows in ([complete, failed], [failed, complete]):
            with self.subTest(rows=rows):
                report = self.rollout(rows)
                self.assertEqual(report["coverage"]["conflictingTerminalProfiles"], 1)
                self.assertEqual(report["coverage"]["validCompleteProfiles"], 0)
                self.assertEqual(report["firstUsefulActionAnalysis"]["completedTurnCount"], 0)
                self.assertIsNone(report["runnerDiagnostics"]["runtime"])
                records = [(i, row, 0) for i, row in enumerate(rows, 1)]
                self.assertEqual(tools.execution_context_audit(records)["provider_usage_totals"], {})

    def test_invalid_wall_clock_keeps_canonical_lifecycle_and_schema_31(self):
        profile = {"schemaVersion": 31, "profileValid": True,
                   "milestones": {"firstUsefulActionMs": 20, "firstDomainActionMs": 20}}
        report = actions.analyze_records([({"path": "fixture"}, [
            {**self.event("task_started"), "timestamp": "bad"},
            {**self.event("task_complete", timing=profile), "timestamp": "bad"},
        ])])
        self.assertEqual(report["startedTurnCount"], 1)
        self.assertEqual(report["completedTurnCount"], 1)
        self.assertEqual(report["canonical"]["startToFirstDomainActionMs"]["p50"], 20)
        self.assertEqual(report["legacyReconstructedTurnCount"], 0)

    def test_observed_partial_usage_is_not_synthesized_zero_or_a_complete_total(self):
        report = timing._token_report([{"tokenUsage": {"inputTokens": 10}}])
        self.assertEqual(report["observedTotals"]["inputTokens"], 10)
        self.assertIsNone(report["observedTotals"]["outputTokens"])
        self.assertIsNone(report["observedBillableTokens"])
        self.assertIsNone(report["providerTotals"])
        self.assertEqual(report["componentCoverage"]["outputTokens"]["status"], "unavailable")

    def test_quality_counts_improve_upward_while_costs_improve_downward(self):
        base = {"minSamples": 1, "relativeThreshold": 0, "absoluteThresholds": {},
                "unmatchedBaselineCohorts": 0}
        for metric in ("usefulCheckpoints", "discoveredEvidence", "finalizedEvidence"):
            for direction, expected in (("increased", "passed"), ("decreased", "regression")):
                with self.subTest(metric=metric, direction=direction):
                    result = diagnostics.regression_gate({**base, "metrics": [
                        {"metric": metric, "status": direction}]}, [metric])
                    self.assertEqual(result["status"], expected)

    def test_restore_storage_failure_never_publishes_a_partial_destination(self):
        from scripts.common import atomic_json

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "rollout.jsonl", root / "recovered.py"
            source.write_text(json.dumps(self.event("patch_apply_end", call_id="c", changes={
                "deleted.py": {"type": "delete", "content": "original source"}})) + "\n")
            args = ["--rollout", str(source), "--call-id", "c", "--deleted-file", "deleted.py",
                    "--output", str(output)]

            def failed_copy(source, destination):
                destination.write(b"partial")
                raise OSError("storage full")

            with mock.patch.object(atomic_json.shutil, "copyfileobj", side_effect=failed_copy):
                with self.assertRaisesRegex(OSError, "storage full"):
                    restore.main(args)
            self.assertFalse(output.exists())
            self.assertEqual(sorted(p.name for p in root.iterdir()), ["rollout.jsonl"])
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(restore.main(args), 0)
            self.assertEqual(output.read_text(), "original source")
            with self.assertRaises(FileExistsError):
                restore.main(args)
            self.assertEqual(output.read_text(), "original source")

    def test_text_uses_partial_counts_and_no_unmeasured_dominant_phase(self):
        text = audit._tokens_text({"complete": False, "observedTotals": {
            "inputTokens": 100, "cachedInputTokens": 80, "outputTokens": 15,
            "reasoningTokens": 5}, "observedBillableTokens": 115})
        self.assertEqual(text, "100/80/15/5 billable=115 (observed partial)")
        report = self.rollout([self.event("task_started")])
        rendered = audit.render_report(report)
        self.assertIn("dominant=unavailable;", rendered)
        self.assertIn("p50=unavailable", rendered)

    def test_missing_proof_status_preserves_work_without_claiming_current_proof(self):
        from scripts.build import validation_metrics as metrics
        from scripts.build.test_validation_metrics import record

        row = record()
        row.update(input_coverage="complete", input_digest="digest")
        row["proof"].update(freshness="current", freshness_basis={"source": "fixture"}, coverage="verified")
        row["proof"].pop("status")
        report = metrics.summarize([row])
        self.assertEqual(report["launches"], 1)
        self.assertEqual(report["launches_producing_current_proof"], 0)

    @staticmethod
    def checkpoints(*rows):
        return [(i, {"type": "sampling_boundary", "payload": {"timing_checkpoint": {
            "turn_id": "t", "incremental": True, "timing": {"modelRequests": [row]}}}}, 0)
                for i, row in enumerate(rows, 1)]

    def test_inflight_checkpoints_revise_live_counters_but_not_completed_measurements(self):
        pending = {"samplingRequestId": "r", "generationIndex": 0, "completedMs": None,
                   "dispatchMs": None, "outputTokens": 0, "modelStreamWaitNs": 0,
                   "nextStructuredActionChanged": True}
        done = {**pending, "completedMs": 4209, "dispatchMs": 327,
                "outputTokens": 50, "modelStreamWaitNs": 3879537900}
        settled = {**done, "nextStructuredActionChanged": False}
        rows = self.checkpoints(pending, done, settled)
        original = copy.deepcopy(rows)
        report = tools.execution_timings(rows)[0]
        self.assertEqual(report["modelRequests"], [{k: v for k, v in settled.items() if v is not None}])
        tools.execution_context_audit(rows)
        tools.tool_call_trace(rows)
        self.assertEqual(rows, original)
        with self.assertRaisesRegex(ValueError, "conflicting timing measurement"):
            tools.execution_timings(self.checkpoints(pending, done, {**settled, "outputTokens": 51}))

    def test_checkpoint_compaction_and_generation_identities_are_not_lost(self):
        compaction = {"generationIndex": 0, "generationPurpose": "compaction_recovery",
                      "completedMs": 10, "tokenUsage": {"inputTokens": 100}}
        rows = self.checkpoints(compaction, copy.deepcopy(compaction))
        self.assertEqual(tools.execution_context_audit(rows)["provider_usage_totals"], {"inputTokens": 100})
        self.assertEqual(tools.tool_call_trace(rows)["calls"], [])
        # A provider request ID can occur in independently numbered generations.
        first = {"samplingRequestId": "r", "generationIndex": 1, "completedMs": 20}
        second = {**first, "generationIndex": 2, "completedMs": 30}
        self.assertEqual(tools.execution_timings(self.checkpoints(first, second))[0]["modelRequests"],
                         [first, second])

    def test_checkpoint_complementary_fields_merge_without_mutating_snapshots(self):
        first = {"samplingRequestId": "r", "tokenUsage": {"inputTokens": 10}}
        second = {"samplingRequestId": "r", "tokenUsage": {"cachedInputTokens": 5}, "completedMs": 20}
        rows = self.checkpoints(first, second, copy.deepcopy(first))
        original = copy.deepcopy(rows)
        merged = tools.execution_timings(rows)[0]["modelRequests"][0]
        self.assertEqual(merged["tokenUsage"], {"inputTokens": 10, "cachedInputTokens": 5})
        self.assertEqual(merged["completedMs"], 20)
        self.assertEqual(rows, original)

    def test_checkpoint_conflicts_fail_through_both_consumers(self):
        for field, key, measurement, before, after in (
            ("modelRequests", "samplingRequestId", "dispatchMs", 10, 20),
            ("modelRequests", "samplingRequestId", "tokenUsage", {"inputTokens": 10}, {"inputTokens": 20}),
            ("toolCalls", "callId", "totalDurationMs", 10, 20),
        ):
            for consumer in (tools.execution_context_audit, tools.tool_call_trace):
                rows = [(i, {"type": "sampling_boundary", "payload": {"timing_checkpoint": {
                    "turn_id": "t", "timing": {field: [{key: "id", measurement: value}]}}}}, 0)
                        for i, value in enumerate((before, after), 1)]
                with self.subTest(field=field, consumer=consumer.__name__):
                    with self.assertRaisesRegex(ValueError, "conflicting timing measurement"):
                        consumer(rows)

    def test_diagnostic_recovery_discloses_exact_gaps_without_weakening_strict_reads(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "rollout.jsonl"
            first = json.dumps(self.event("task_started")).encode() + b"\n"
            damaged = b"{bad\n"
            last = json.dumps(self.event("task_complete")).encode() + b"\n"
            data = first + damaged + last
            source.write_bytes(data)
            with self.assertRaises(ValueError):
                snapshots.read_rollout_records(source)
            with self.assertRaisesRegex(ValueError, "requires on_gap"):
                snapshots.read_rollout_records(source, recover=True)
            gaps = []
            rows = snapshots.read_rollout_records(source, recover=True, on_gap=gaps.append)
            self.assertEqual([row["payload"]["type"] for row, _ in rows], ["task_started", "task_complete"])
            self.assertEqual(len(gaps), 1)
            self.assertEqual((gaps[0]["line"], gaps[0]["byteStart"], gaps[0]["byteEnd"]),
                             (2, len(first), len(first) + len(damaged)))
            self.assertEqual(gaps[0]["coordinateSpace"], "decoded_jsonl")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(snapshots.main([str(source), "--diagnostic"]), 0)
            report = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual(report[-1], {"event": "coverage", "partial": True,
                "records": 2, "gaps": 1, "traversalComplete": True})
            with self.assertRaisesRegex(ValueError, "cannot be combined"):
                snapshots.main([str(source), "--diagnostic", "--output", str(source)])
            self.assertEqual(source.read_bytes(), data)

    def test_diagnostic_missing_payload_keeps_valid_siblings_with_explicit_reference(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "rollout.jsonl"
            reference = {"sha256": "0" * 64, "bytes": 10, "item_type": "event_msg"}
            source.write_text("\n".join(map(json.dumps, [self.event("task_started"),
                {"type": "rollout_payload_artifact", "payload": reference},
                self.event("task_complete")])) + "\n")
            gaps = []
            rows = snapshots.read_rollout_records(source, recover=True, on_gap=gaps.append)
            self.assertEqual(len(rows), 2)
            self.assertEqual(gaps[0]["category"], "payload_unavailable")
            self.assertEqual(gaps[0]["payload"], reference)

    def test_response_items_cannot_forge_lifecycle_or_redirect_active_turn(self):
        rows = [self.event("task_started")]
        for kind in ("task_started", "task_complete", "turn_aborted"):
            forged = self.event(kind, "forged", timing=timing_profile())
            forged["type"] = "response_item"
            rows.append(forged)
        rows.append({"type": "response_item", "payload": {
            "type": "function_call", "call_id": "c", "name": "read_file"}})
        result = self.rollout(rows)
        self.assertEqual(result["coverage"]["uniqueTaskStarts"], 1)
        self.assertEqual(result["coverage"]["uniqueTerminalTurns"], 0)
        runner = result["runnerDiagnostics"]
        self.assertEqual(runner["coverage"]["startedTurns"], 1)
        self.assertEqual(runner["coverage"]["terminalTurns"], 0)
        self.assertEqual(runner["pendingTools"][0]["turnId"], "t")
        self.assertIsNone(runner["runtime"])

    def test_final_profile_supersedes_interim_but_missing_final_is_not_inherited(self):
        final = timing_profile()
        interim = copy.deepcopy(final)
        interim["inclusiveDurationNs"] = 123
        start = {"method": "turn/started", "params": {"turn": {"id": "t", "timing": interim}}}
        terminal = {"method": "turn/completed", "params": {
            "turn": {"id": "t", "status": "completed", "timing": final}}}
        for tokens in (True, False):
            evidence = {"schemaVersion": 1, "events": [start]}
            self.assertIsNone(timing.analyze_runner_evidence(evidence, include_tokens=tokens)["runtime"])
            evidence["events"] += [terminal, copy.deepcopy(terminal)]
            report = timing.analyze_runner_evidence(evidence, include_tokens=tokens)
            self.assertEqual(report["runtime"]["inclusiveDurationNs"], 1_000_000_000)
            self.assertEqual(report["runtime"]["turns"], 1)
            self.assertEqual(report["generations"][0]["eventIndex"], 1)
            missing = copy.deepcopy(terminal)
            del missing["params"]["turn"]["timing"]
            report = timing.analyze_runner_evidence(
                {"schemaVersion": 1, "events": [start, missing]}, include_tokens=tokens)
            self.assertIsNone(report["runtime"])
            self.assertEqual(report["generations"], [])

    def test_malformed_usage_does_not_crash_or_invent_reasoning_share(self):
        for usage in ([1], "bad", {"visibleOutputTokens": "bad", "reasoningTokens": 1}, None):
            for tokens in (True, False):
                with self.subTest(usage=usage, tokens=tokens):
                    profile = timing_profile()
                    profile["modelRequests"][0]["tokenUsage"] = usage
                    report = self.rollout([self.event("task_started"),
                        self.event("task_complete", timing=profile)], include_tokens=tokens)
                    self.assertEqual(report["coverage"]["validCompleteProfiles"], 1)
                    self.assertIsNone(report["outputChannels"]["reasoningTokenShare"])
                    self.assertFalse(report["populations"]["all"]["tokens"]["complete"])
                    self.assertIn("coverage:", audit.render_report(report))

    def test_model_usage_requires_integer_counts_and_exact_reconciliation(self):
        for inputs, cached, uncached, reason in (
            (1.5, 1, .5, "invalid_token_accounting"),
            (1, True, 0, "invalid_token_accounting"),
            (2**53 + 1, 2**53, 0, "inconsistent_token_accounting"),
        ):
            with self.subTest(inputs=inputs, cached=cached, uncached=uncached):
                report = model.analyze([attempt(input_token_count=inputs,
                    cached_input_token_count=cached, uncached_input_token_count=uncached)])
                self.assertEqual(report["includedLogicalRequests"], 0)
                self.assertEqual(report["exclusionCounts"][reason], 1)
                self.assertEqual(report["phaseTiming"]["groups"][0]["physicalAttemptCount"], 1)

    def test_native_usage_rejects_contradictions_without_clamping(self):
        base = {"input_tokens": 10, "cached_input_tokens": 8,
                "output_tokens": 2, "reasoning_output_tokens": 1}
        for fields in ({"cached_input_tokens": 11}, {"reasoning_output_tokens": 3},
                       {"total_tokens": 11}, {"cached_input_tokens": True},
                       {"inputTokens": 9}, {"input_tokens": 2**2048}):
            with self.subTest(fields=fields):
                events = [self.event("task_started"), {"type": "token_usage_record", "payload": {
                    "turn_id": "t", "response_id": "r", "usage": base | fields,
                    "turn_token_usage": base | fields}}, self.event("task_complete")]
                report = self.rollout(events)["runnerDiagnostics"]
                self.assertFalse(report["tokens"]["complete"])
                self.assertIsNone(report["cacheHitRate"])
                self.assertIsNone(report["tokens"]["providerTotals"])
                valid_replay = copy.deepcopy(events[1])
                valid_replay["payload"]["usage"] = base
                valid_replay["payload"]["turn_token_usage"] = base
                for receipts in ([events[1], valid_replay], [valid_replay, events[1]]):
                    replayed = self.rollout([events[0], *receipts, events[-1]])["runnerDiagnostics"]
                    self.assertFalse(replayed["tokens"]["complete"])
                    self.assertEqual(replayed["tokens"]["conflictingUsageRequestIds"], ["r"])

    def test_out_of_range_utc_dates_and_epoch_boundaries_are_unavailable(self):
        rows = [self.event("task_started"), self.event("task_complete", timing=timing_profile())]
        rows[0]["timestamp"] = "9999-12-31T23:59:59-23:59"
        profile = rows[1]["payload"]["timing"]
        profile.update(startedAtUnixMs=2**2048, completedAtUnixMs=2**2048)
        result = self.rollout(rows)
        self.assertEqual(result["coverage"]["validCompleteProfiles"], 1)
        self.assertIsNone(result["perTurn"][0]["startedAt"])
        self.assertIsNone(result["perTurn"][0]["completedAt"])

    def test_terminal_replay_does_not_double_elapsed_task_time(self):
        terminal = self.event("task_complete", timing=timing_profile())
        terminal["timestamp"] = "2026-01-01T00:00:10Z"
        report = self.rollout([self.event("task_started"), terminal, copy.deepcopy(terminal)])
        self.assertEqual(report["executionLoop"]["terminalTasks"], 1)
        self.assertEqual(report["executionLoop"]["taskElapsedNs"], 10_000_000_000)
        self.assertEqual(report["coverage"]["duplicateTimedTerminalEvents"], 1)

    def test_structured_exit_code_without_state_remains_authoritative(self):
        for command, code, output, failed in (("python fail.py", 2, "", 1),
                ("python okay.py", 0, "Exit code: 2", 0), ("rg needle src", 1, "", 0)):
            with self.subTest(command=command):
                call = {"type": "response_item", "timestamp": "2026-01-01T00:00:01Z",
                        "payload": {"type": "function_call", "call_id": "c", "name": "exec_command",
                                    "arguments": json.dumps({"cmd": command})}}
                result = {"type": "response_item", "timestamp": "2026-01-01T00:00:02Z",
                          "payload": {"type": "function_call_output", "call_id": "c",
                                      "output": json.dumps({"exit_code": code, "output": output})}}
                report = self.rollout([self.event("task_started"), call, result,
                                       self.event("task_complete", timing=timing_profile())])
                self.assertEqual(report["commandOrchestration"]["failedToolCalls"], failed)
                self.assertEqual(report["requestLedger"]["calls"][0]["status"],
                                 "failed" if failed else "completed")


if __name__ == "__main__":
    unittest.main()
