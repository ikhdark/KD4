from __future__ import annotations

import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from scripts.tool_result_audit import audit, execution_context_audit, repeated_input_overlap, source_read_paths, tool_call_trace


class ToolResultAuditTest(unittest.TestCase):
    def test_request_output_index_preserves_missing_and_reordered_boundaries(self):
        records = []

        def add(kind, payload):
            records.append((len(records) + 1, {"type": kind, "payload": payload}, 0))

        add("response_item", {"type": "function_call_output", "output": "unowned"})
        for turn in ("first", "second"):
            add("event_msg", {"type": "task_started", "turn_id": turn})
            for index in range(40):
                add("response_item", {"type": "function_call_output", "output": str(index)})
                add("sampling_boundary", {"sampling_request_id": f"{turn}-{index}"})
            # Missing boundaries leave the previous boundary unchanged. A
            # backwards boundary must not acquire outputs from an empty range.
            order = [39, 0, None, *range(1, 39)]
            requests = [{"samplingRequestId": f"{turn}-{index}"} for index in order]
            add("event_msg", {"type": "task_complete", "turn_id": turn,
                              "timing": {"modelRequests": requests}})
        add("event_msg", {"type": "task_started", "turn_id": "open"})
        add("response_item", {"type": "function_call_output", "output": "pending"})

        report = execution_context_audit(records)
        self.assertEqual([turn["turn_id"] for turn in report["turns"]], ["first", "second"])
        self.assertEqual(
            [(output["record"], output["bytes"]) for output in report["tool_outputs"]],
            [(line, len(row["payload"]["output"].encode())) for line, row, _ in records
             if row["payload"].get("type") == "function_call_output"],
        )
        for turn in report["turns"]:
            self.assertEqual(
                [(request["sampling_request_id"], request["boundary_record"])
                 for request in turn["rounds"]],
                [(f"{turn['turn_id']}-{index}", next(
                    (line for line, row, _ in records
                     if row["payload"].get("sampling_request_id") == f"{turn['turn_id']}-{index}"),
                    None,
                )) for index in order],
            )
            previous = 0
            for request in turn["rounds"]:
                boundary = request["boundary_record"]
                expected = [output for output in report["tool_outputs"]
                            if output["turn_id"] == turn["turn_id"]
                            and boundary is not None
                            and previous < output["record"] < boundary]
                self.assertEqual(request["added_tool_result_records"],
                                 [output["record"] for output in expected])
                self.assertEqual(request["added_tool_result_bytes"],
                                 sum(output["bytes"] for output in expected))
                if boundary is not None:
                    previous = boundary
        for output in report["tool_outputs"]:
            expected = sum(request["boundary_record"] is not None
                           and request["boundary_record"] > output["record"]
                           for turn in report["turns"]
                           if turn["turn_id"] == output["turn_id"]
                           for request in turn["rounds"])
            self.assertEqual(output["subsequent_requests_in_turn"], expected)
            self.assertEqual(output["raw_replay_estimated_tokens"],
                             ((output["bytes"] + 3) // 4) * expected)
        self.assertEqual(report["coverage"]["unfinished_turn_ids"], ["open"])

    @staticmethod
    def output_records(bodies):
        return [(i, {"type": "response_item", "payload": {
            "type": "function_call_output", "output": body}}, len(body))
            for i, body in enumerate(bodies, 1)]

    def test_matching_index_skips_disjoint_and_fast_paths_identical_outputs(self):
        from unittest import mock
        source = ("long repetitive output " * 4 + "\n") * 1600
        records = self.output_records(
            [f"unique {i}\n" * 20 for i in range(100)] + [source, source])
        with mock.patch("scripts.tool_result_audit.difflib.SequenceMatcher",
                        side_effect=AssertionError("unnecessary quadratic comparison")):
            outputs = execution_context_audit(records)["tool_outputs"]
        self.assertEqual(len(outputs), len(records))
        self.assertEqual([o["repeated_block_bytes"] for o in outputs[:-1]], [0] * 101)
        self.assertEqual(outputs[-1]["matching_blocks"], [{
            "prior_record": 101, "start_line": 1, "lines": 1600,
            "bytes": len(source.encode()),
        }])
        self.assertEqual(outputs[-1]["repeated_block_bytes"], len(source.encode()))
        self.assertTrue(all(o["matching_blocks_coverage"]["complete"] for o in outputs))

    def test_expensive_matching_is_reported_as_incomplete_not_exact_zero(self):
        source = ("repetitive output " * 4 + "\n") * 1600
        report = execution_context_audit(self.output_records([source, source + "tail\n"]))
        output = report["tool_outputs"][-1]
        self.assertEqual(output["matching_blocks_coverage"],
                         {"candidate_pairs": 1, "omitted_pairs": 1, "complete": False})

    def test_repeated_output_evidence_stops_at_the_pair_budget(self):
        body = ("repeated " * 8 + "\n") * 4
        outputs = execution_context_audit(self.output_records([body] * 300))["tool_outputs"]
        self.assertEqual(sum(len(o["matching_blocks"]) for o in outputs), 10000)
        coverage = outputs[-1]["matching_blocks_coverage"]
        self.assertEqual(coverage, {"candidate_pairs": 299, "omitted_pairs": 299,
                                    "complete": False})

    def test_indexed_matching_preserves_small_sequence_matcher_results(self):
        import difflib
        import random
        randomizer = random.Random(7)
        bodies = ["".join(randomizer.choice(["a", "b", "c"]) * 45 + "\n"
                          for _ in range(20)) for _ in range(20)]
        outputs = execution_context_audit(self.output_records(bodies))["tool_outputs"]
        for index, output in enumerate(outputs):
            current = bodies[index].splitlines(keepends=True)
            expected = []
            for prior in range(index):
                for match in difflib.SequenceMatcher(
                    None, bodies[prior].splitlines(keepends=True), current,
                    autojunk=False
                ).get_matching_blocks():
                    size = sum(len(s.encode()) for s in current[match.b:match.b + match.size])
                    if match.size >= 4 and size >= 160:
                        expected.append({"prior_record": prior + 1, "start_line": match.b + 1,
                                         "lines": match.size, "bytes": size})
            self.assertEqual(output["matching_blocks"], expected)

    def test_repeated_input_bounds_require_unchanged_append_only_context(self):
        previous = {"provider_usage": {"inputTokens": 100}, "local_prompt_categories": {
            "promptSectionSha256": {"history": "old", "tool_schemas": "stable"}}}
        current = {"provider_usage": {"inputTokens": 150, "cachedInputTokens": 90},
                   "local_prompt_categories": {
                       "promptSectionSha256": {"history": "new", "tool_schemas": "stable"},
                       "historyItemsPrevious": 7, "historyPrefixItemsReused": 7,
                       "historyFirstDivergentIndex": 7}}
        result = repeated_input_overlap(previous, current)
        self.assertEqual(result["repeated_input_proxy"], 100)
        self.assertEqual((result["cached_overlap_min"], result["cached_overlap_max"]), (40, 90))
        self.assertEqual((result["uncached_overlap_min"], result["uncached_overlap_max"]), (10, 60))
        self.assertIsNone(repeated_input_overlap(None, current))
        self.assertIsNone(repeated_input_overlap({}, current))
        for key, value in [("historyPrefixItemsReused", 6), ("historyItemsPrevious", 0),
                           ("historyFirstDivergentIndex", 3),
                           ("promptSectionSha256", {"history": "new", "tool_schemas": "changed"}),
                           ("promptSectionSha256", {})]:
            changed = {**current, "local_prompt_categories": {**current["local_prompt_categories"], key: value}}
            self.assertIsNone(repeated_input_overlap(previous, changed), key)
        for usage in [{}, {"inputTokens": 90, "cachedInputTokens": 80},
                      {"inputTokens": 150, "cachedInputTokens": 151}]:
            self.assertIsNone(repeated_input_overlap(previous, {**current, "provider_usage": usage}))
        uncached = repeated_input_overlap(previous, {**current, "provider_usage": {
            "inputTokens": 150, "cachedInputTokens": 0}})
        self.assertEqual((uncached["uncached_overlap_min"], uncached["uncached_overlap_max"]), (100, 100))

    def test_trace_counts_dispatches_not_mentions_or_checkpoint_mirrors(self):
        timings = [{"callId": "outer", "toolName": "exec", "source": "direct"},
                   {"callId": "child", "toolName": "read_file", "source": "code_mode",
                    "parentCallId": "outer", "outcome": "success", "totalDurationMs": 3}]
        rows = [
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t"}},
            {"type": "response_item", "payload": {"type": "custom_tool_call", "call_id": "outer", "name": "exec", "input": "if(false) tools.exec_command({}); tools.read_file({path:'a'});"}},
            {"type": "sampling_boundary", "payload": {"timing_checkpoint": {"toolCalls": timings}}},
            {"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "outer", "output": "evidence"}},
            {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t", "timing": {"toolCalls": timings}}},
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "next"}},
            {"type": "response_item", "payload": {"type": "custom_tool_call", "call_id": "outer", "name": "exec", "input": "unresolved"}},
            {"type": "response_item", "payload": {"type": "custom_tool_call_output", "call_id": "orphan", "output": "unknown"}},
        ]
        trace = tool_call_trace([(i, row, 0) for i, row in enumerate(rows, 1)])
        self.assertEqual(trace["coverage"]["calls"], 3)
        self.assertEqual(trace["coverage"]["nested_calls"], 1)
        self.assertEqual(trace["coverage"]["outer_calls_without_output"], ["outer"])
        self.assertEqual(trace["coverage"]["orphan_output_records"], [8])
        child = trace["calls"][1]
        self.assertEqual(child["input_source_record"], 2)
        self.assertEqual(child["output_container_records"], [4])
        self.assertIsNone(child["input"])
        self.assertIsNone(child["material_contribution"])

    def test_shell_read_candidates_do_not_mistake_switches_for_paths(self):
        self.assertEqual(source_read_paths(
            "Get-Content -Raw -LiteralPath 'a file.py'; Get-Content scripts/test.py -TotalCount 4; "
            "Get-Content -LiteralPath $unknown; Get-Content -Encoding utf8 other.py"
        ), ["a file.py", "scripts/test.py"])

    def test_complete_execution_counts_terminal_usage_once_and_keeps_unknowns(self):
        def request(index, input_tokens, cached):
            return {
                "generationIndex": index,
                "samplingRequestId": str(index),
                "tokenUsage": {
                    "inputTokens": input_tokens,
                    "cachedInputTokens": cached,
                    "visibleOutputTokens": 7,
                    "reasoningTokens": 3,
                },
                "requestTokenCategories": {
                    "logicalTotal": input_tokens + 10,
                    "toolSchemas": 20,
                    "conversationHistory": index * 30,
                },
            }

        requests = [request(0, 100, 0), request(1, 150, 90)]
        source = "".join(f"source {i}: {'x' * 50}\n" for i in range(5))
        rows = [
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t"}},
            {
                "type": "sampling_boundary",
                "payload": {
                    "sampling_request_id": "0",
                    "timing_checkpoint": {"modelRequests": requests},
                },
            },
            {
                "type": "response_item",
                "payload": {"type": "custom_tool_call_output", "output": source},
            },
            {"type": "sampling_boundary", "payload": {"sampling_request_id": "1"}},
            {
                "type": "response_item",
                "payload": {"type": "custom_tool_call_output", "output": source},
            },
            {
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "t",
                    "timing": {"modelRequests": requests},
                },
            },
            {
                "type": "event_msg",
                "payload": {"type": "task_started", "turn_id": "unfinished"},
            },
        ]
        report = execution_context_audit(
            [(i, r, len(json.dumps(r))) for i, r in enumerate(rows, 1)]
        )
        self.assertEqual(report["provider_usage_totals"]["inputTokens"], 250)
        self.assertEqual(report["provider_usage_totals"]["reasoningTokens"], 6)
        self.assertEqual(report["provider_uncached_input_tokens"], 160)
        self.assertIsNone(report["repeated_uncached_input_tokens"])
        self.assertEqual(report["coverage"]["unfinished_turn_ids"], ["unfinished"])
        first, second = report["turns"][0]["rounds"]
        self.assertIsNone(first["input_growth_since_previous_request"])
        self.assertEqual(second["input_growth_since_previous_request"], 50)
        self.assertEqual(second["added_tool_result_records"], [3])
        self.assertEqual(second["added_tool_result_bytes"], len(source.encode()))
        self.assertEqual(
            report["tool_outputs"][1]["repeated_block_bytes"], len(source.encode())
        )
        self.assertEqual(report["tool_outputs"][0]["subsequent_requests_in_turn"], 1)
        self.assertIsNone(execution_context_audit([])["provider_uncached_input_tokens"])

    def test_incremental_checkpoints_count_partial_executions_without_duplicate_usage(self):
        def boundary(requests, calls):
            return {"type": "sampling_boundary", "payload": {
                "sampling_request_id": "b", "timing_checkpoint": {
                    "turn_id": "partial", "incremental": True, "tail_unknown": True,
                    "timing": {"modelRequests": requests, "toolCalls": calls}}}}

        request = {"samplingRequestId": "a", "generationIndex": 0,
                   "tokenUsage": {"inputTokens": 100, "cachedInputTokens": 80}}
        pending = {"samplingRequestId": "b", "generationIndex": 1}
        call = {"callId": "nested", "toolName": "read_tool_output", "source": "code_mode"}
        rows = [
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "partial"}},
            boundary([request, pending], [call]),
            boundary([request, pending], [{**call, "outcome": "success"}]),
        ]
        records = [(i, row, 0) for i, row in enumerate(rows, 1)]
        report = execution_context_audit(records)
        self.assertEqual(report["provider_usage_totals"]["inputTokens"], 100)
        self.assertEqual(report["coverage"]["completed_turns"], 0)
        self.assertEqual(report["coverage"]["unfinished_turn_ids"], ["partial"])
        self.assertEqual(report["coverage"]["requests_with_usage"], 1)
        self.assertEqual(report["coverage"]["requests_without_usage"], 1)
        self.assertEqual(report["turns"][0]["timing_source"], "checkpoint")
        trace = tool_call_trace(records)
        self.assertEqual(trace["coverage"]["nested_calls"], 1)
        self.assertEqual(trace["calls"][0]["outcome"], "success")

        # Final arrays supersede earlier deltas instead of adding to them.
        records.append((4, {"type": "event_msg", "payload": {
            "type": "task_complete", "turn_id": "partial", "timing": {
                "modelRequests": [{**request, "tokenUsage": {"inputTokens": 120, "cachedInputTokens": 80}}],
                "toolCalls": []}}}, 0))
        report = execution_context_audit(records)
        self.assertEqual(report["provider_usage_totals"]["inputTokens"], 120)
        self.assertEqual(report["coverage"]["completed_turns"], 1)
        self.assertEqual(report["coverage"]["requests_without_usage"], 0)
        self.assertEqual(report["turns"][0]["timing_source"], "terminal")
        self.assertEqual(tool_call_trace(records)["coverage"]["nested_calls"], 0)

    def test_aborted_terminal_arrays_replace_checkpoints_across_all_report_consumers(self):
        from scripts.tool_result_audit import compact_report

        pending = {"samplingRequestId": "request", "generationIndex": 0}
        tokens = {"inputTokens": 120, "cachedInputTokens": 80,
                  "visibleOutputTokens": 7, "reasoningTokens": 3, "totalTokens": 130}
        final = {**pending, "tokenUsage": tokens}
        call = {"callId": "call", "toolName": "exec", "source": "direct"}
        rows = [
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "aborted"}},
            {"type": "response_item", "payload": {"type": "custom_tool_call",
                "call_id": "call", "name": "exec", "input": "text(1)"}},
            {"type": "sampling_boundary", "payload": {"timing_checkpoint": {
                "turn_id": "aborted", "timing": {"modelRequests": [pending], "toolCalls": [call]}}}},
            {"type": "event_msg", "payload": {"type": "turn_aborted", "turn_id": "aborted",
                "timing": {"modelRequests": [final], "toolCalls": [
                    {**call, "outcome": "canceled", "totalDurationMs": 9}],
                    "counters": {"toolOutputTruncationCount": 4,
                                 "truncationInducedContinuationCount": 2}}}},
            # A late checkpoint must not replace authoritative terminal evidence.
            {"type": "sampling_boundary", "payload": {"timing_checkpoint": {
                "turn_id": "aborted", "timing": {"modelRequests": [pending], "toolCalls": [call]}}}},
            {"type": "event_msg", "payload": {"type": "token_count", "info": {
                "total_token_usage": {"input_tokens": 120, "cached_input_tokens": 80,
                    "output_tokens": 10, "reasoning_output_tokens": 3, "total_tokens": 130}}}},
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "open"}},
        ]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "aborted.jsonl"
            path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
            session = audit(path)
            context = session["execution_context"]
            self.assertEqual(context["provider_usage_totals"], tokens)
            self.assertEqual(context["provider_usage_reconciliation"]["status"], "matched")
            self.assertEqual(context["coverage"]["completed_turns"], 0)
            self.assertEqual(context["coverage"]["aborted_turns"], 1)
            self.assertEqual(context["coverage"]["terminal_turns"], 1)
            self.assertEqual(context["coverage"]["unfinished_turn_ids"], ["open"])
            turn = context["turns"][0]
            self.assertFalse(turn["completed"])
            self.assertIsNone(turn["completion_record"])
            self.assertEqual(turn["terminal_status"], "turn_aborted")
            self.assertEqual(turn["terminal_record"], 4)
            self.assertEqual(turn["timing_source"], "terminal")
            trace = session["tool_call_trace"]
            self.assertEqual(trace["coverage"]["aborted_turns"], 1)
            self.assertEqual(trace["coverage"]["terminal_turns"], 1)
            self.assertEqual(trace["calls"][0]["outcome"], "canceled")
            self.assertEqual(trace["calls"][0]["duration_ms"], 9)
            self.assertEqual(trace["coverage"]["outer_calls_without_output"], ["call"])
            summary = compact_report({"sessions": [session]}, path, "hash", 0)
            self.assertEqual(summary["totals"]["aborted_turns"], 1)
            self.assertEqual(summary["totals"]["unfinished_turns"], 1)
            self.assertEqual(summary["terminal_truncation"]["tool_output_truncations"], 4)
            self.assertEqual(summary["terminal_truncation"]["induced_continuations"], 2)

    def test_aborted_turn_keeps_missing_usage_unknown_and_empty_terminal_arrays_authoritative(self):
        pending = {"samplingRequestId": "pending"}
        rows = [
            (1, {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t"}}, 0),
            (2, {"type": "sampling_boundary", "payload": {"timing_checkpoint": {
                "modelRequests": [pending], "toolCalls": [{"callId": "old"}]}}}, 0),
            (3, {"type": "event_msg", "payload": {"type": "turn_aborted", "turn_id": "t"}}, 0),
        ]
        report = execution_context_audit(rows)
        self.assertEqual(report["coverage"]["unfinished_turn_ids"], [])
        self.assertEqual(report["coverage"]["requests_without_usage"], 1)
        self.assertIsNone(report["provider_uncached_input_tokens"])
        self.assertEqual(report["turns"][0]["timing_source"], "checkpoint")
        rows[-1][1]["payload"]["timing"] = {"modelRequests": [], "toolCalls": []}
        report = execution_context_audit(rows)
        self.assertEqual(report["coverage"]["requests_without_usage"], 0)
        self.assertEqual(report["turns"][0]["timing_source"], "terminal")
        self.assertEqual(tool_call_trace(rows)["calls"], [])

    def test_checkpoint_requests_without_identity_are_not_silently_merged(self):
        with self.assertRaisesRegex(ValueError, "missing samplingRequestId"):
            execution_context_audit([(1, {"type": "sampling_boundary", "payload": {
                "timing_checkpoint": {"timing": {"modelRequests": [{"generationIndex": 0}]}}
            }}, 0)])

    def test_usage_reconciliation_does_not_confuse_visible_and_reasoning_or_missing_data(self):
        tokens = {"inputTokens": 100, "cachedInputTokens": 80,
                  "visibleOutputTokens": 7, "reasoningTokens": 3, "totalTokens": 110}
        cumulative = {"input_tokens": 100, "cached_input_tokens": 80,
                      "output_tokens": 10, "reasoning_output_tokens": 3, "total_tokens": 110}
        request = {"samplingRequestId": "r", "tokenUsage": tokens}
        rows = [(1, {"type": "sampling_boundary", "payload": {
            "timing_checkpoint": {"turn_id": "t", "timing": {"modelRequests": [request]}}
        }}, 0)]
        def reconciliation():
            return execution_context_audit(rows)["provider_usage_reconciliation"]
        self.assertEqual(reconciliation()["status"], "unavailable")
        rows.append((2, {"type": "event_msg", "payload": {"type": "token_count",
            "info": {"total_token_usage": cumulative}}}, 0))
        self.assertEqual(reconciliation()["status"], "matched")
        self.assertEqual(set(reconciliation()["request_minus_cumulative"].values()), {0})
        cumulative["input_tokens"] = 120
        self.assertEqual(reconciliation()["status"], "different")
        self.assertEqual(reconciliation()["request_minus_cumulative"]["input_tokens"], -20)
        cumulative["input_tokens"] = 100
        del tokens["reasoningTokens"]
        self.assertEqual(reconciliation()["status"], "partial_match")
        self.assertNotIn("output_tokens", reconciliation()["request_minus_cumulative"])
        rows.append((3, {"type": "event_msg", "payload": {"type": "token_count", "info": None}}, 0))
        self.assertEqual(reconciliation()["last_token_count"]["record"], 2)

    def test_partial_usage_does_not_invent_uncached_totals_or_complete_reconciliation(self):
        complete = {"inputTokens": 100, "cachedInputTokens": 80,
                    "visibleOutputTokens": 7, "reasoningTokens": 3, "totalTokens": 110}
        cumulative = {"input_tokens": 100, "cached_input_tokens": 80,
                      "output_tokens": 10, "reasoning_output_tokens": 3, "total_tokens": 110}
        # A missing request's contribution is unknown even if observed sums
        # happen to equal the cumulative event. Missing cache usage is not zero.
        cases = [
            ([{"inputTokens": 100}], None, "partial_match"),
            ([{"cachedInputTokens": 80}], None, "partial_match"),
            ([{"visibleOutputTokens": 7}], None, "unavailable"),
            ([complete, {}], None, "unavailable"),
            ([complete, {"inputTokens": 0}], None, "partial_match"),
            ([complete], 20, "matched"),
            ([{"inputTokens": 0, "cachedInputTokens": 0}], 0, "different"),
        ]
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "partial.jsonl"
            for usages, uncached, status in cases:
                with self.subTest(usages=usages):
                    rows = [
                        {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "t",
                            "timing": {"modelRequests": [
                                {"samplingRequestId": str(i), "tokenUsage": tokens}
                                for i, tokens in enumerate(usages)
                            ]}}},
                        {"type": "event_msg", "payload": {"type": "token_count",
                            "info": {"total_token_usage": cumulative}}},
                    ]
                    path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
                    context = audit(path)["execution_context"]
                    self.assertEqual(context["provider_uncached_input_tokens"], uncached)
                    self.assertEqual(context["provider_usage_reconciliation"]["status"], status)
                    if usages == [complete, {}]:
                        self.assertEqual(context["provider_usage_totals"], complete)
                        self.assertEqual(context["coverage"]["requests_without_usage"], 1)
                        self.assertEqual(context["provider_usage_reconciliation"]["request_minus_cumulative"], {})

    def test_exact_bytes_references_and_mentions_are_not_conflated(self):
        with tempfile.TemporaryDirectory() as temp:
            home = Path(temp)
            path = home / "sessions" / "sample.jsonl"
            path.parent.mkdir()
            artifact_id = "00000000-0000-0000-0000-000000000001"
            directory = home / "tool-output" / "session"
            directory.mkdir(parents=True)
            evidence = "λ exact\r\n".encode()
            digest = hashlib.sha256(evidence).hexdigest()
            (directory / f"{artifact_id}.log").write_bytes(evidence)
            (directory / f"{artifact_id}.meta.json").write_text(
                json.dumps(
                    {
                        "canonical_bytes": len(evidence),
                        "canonical_sha256": digest,
                    }
                ),
                encoding="utf-8",
            )
            output = json.dumps(
                {"output": "λ", "exit_code": 0, "artifact_id": artifact_id},
                ensure_ascii=False,
            )
            records = [{"type": "session_meta", "payload": {"id": "session"}}]
            for index in range(2):
                records.extend(
                    [
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call",
                                "name": "exec",
                                "call_id": str(index),
                                "input": "const r=await tools.exec_command({});text(r.output);text(r.exit_code);",
                            },
                        },
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call_output",
                                "call_id": str(index),
                                "output": output,
                            },
                        },
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "reasoning",
                                "summary": [
                                    {"text": "exit_code establishes completion"}
                                ],
                            },
                        },
                        {
                            "type": "tool_manifest",
                            "payload": {"manifest": ["same schema"]},
                        },
                    ]
                )
            data = b"".join(
                json.dumps(row, ensure_ascii=False).encode() + b"\n" for row in records
            )
            path.write_bytes(data)
            report = audit(path)
            self.assertEqual(report["records"], len(records))
            self.assertEqual(
                report["snapshot"]["sha256"], hashlib.sha256(data).hexdigest()
            )
            self.assertEqual(report["snapshot"]["byteLength"], len(data))
            self.assertEqual(report["manifest_records"], 2)
            self.assertEqual(report["distinct_manifest_hashes"], 1)
            first, second = report["results"]
            self.assertEqual(first["visible_bytes"], len(output.encode()))
            self.assertEqual(
                first["visible_estimated_tokens"], (len(output.encode()) + 3) // 4
            )
            self.assertEqual(first["producer_field_mentions"], ["output", "exit_code"])
            self.assertEqual(first["later_reasoning_field_mentions"], ["exit_code"])
            self.assertIsNone(first["exact_duplicate_of"])
            self.assertEqual(second["exact_duplicate_of"], "0")
            self.assertEqual(len(report["artifacts"]), 1)
            self.assertTrue(report["artifacts"][0]["canonical_verified"])
            self.assertEqual(
                report["artifacts"][0]["retained_file_bytes"], len(evidence)
            )
            self.assertNotEqual(len(evidence), first["visible_bytes"])

    def test_missing_evidence_and_incomplete_tail_remain_explicit(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "sample.jsonl"
            row = {
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "call_id": "unknown",
                    "output": '{"artifact_id":"00000000-0000-0000-0000-000000000001"}',
                },
            }
            prefix = json.dumps(row).encode() + b"\n"
            path.write_bytes(prefix + b'{"partial":')
            report = audit(path)
            self.assertTrue(report["incomplete_tail"])
            self.assertEqual(report["records"], 1)
            self.assertFalse(report["artifacts"][0]["available"])
            self.assertNotIn("retained_file_bytes", report["artifacts"][0])
            path.write_bytes(prefix + b'{"partial":\n')
            with self.assertRaisesRegex(ValueError, "invalid complete record"):
                audit(path)

    def test_follow_up_candidates_are_not_claimed_as_raw_bytes_or_causality(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "sample.jsonl"
            records = []
            for index, output in enumerate(
                ["Warning: truncated output", "new evidence"]
            ):
                records.extend(
                    [
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call",
                                "call_id": str(index),
                                "name": "exec",
                                "input": 'text((await tools.exec_command({cmd:"Get-Content src/a.rs"})).output);',
                            },
                        },
                        {
                            "type": "response_item",
                            "payload": {
                                "type": "custom_tool_call_output",
                                "call_id": str(index),
                                "output": output,
                            },
                        },
                    ]
                )
            path.write_text(
                "".join(json.dumps(row) + "\n" for row in records), encoding="utf-8"
            )
            report = audit(path)
            first, second = report["results"]
            self.assertIsNone(first["raw_tool_result_bytes"])
            self.assertEqual(first["nested_tool_mentions"], {"exec_command": 1})
            self.assertEqual(
                first["next_call"]["shared_shell_read_paths"], ["src/a.rs"]
            )
            self.assertEqual(first["next_call"]["call_id"], "1")
            self.assertIsNone(second["next_call"])

    def test_quoted_truncation_source_is_not_a_runtime_measurement(self):
        from scripts.tool_result_audit import compact_report

        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "quoted.jsonl"
            output = {"type": "response_item", "payload": {
                "type": "custom_tool_call_output", "call_id": "source",
                "output": 'let receipt = json!({"output_truncated":true});',
            }}
            path.write_text(json.dumps(output) + "\n", encoding="utf-8")
            session = audit(path)
            self.assertTrue(session["results"][0]["truncation_marker"])
            self.assertEqual(session["summary"]["truncation_marker_results"], 1)
            self.assertEqual(session["summary"]["truncated_results"], 1)
            # Saved v2 reports without the new descriptive alias remain usable.
            del session["summary"]["truncation_marker_results"]
            report = {"sessions": [session]}
            summary = compact_report(report, path, "hash", 0)
            self.assertEqual(summary["totals"]["truncation_marker_results"], 1)
            self.assertEqual(summary["terminal_truncation"]["sessions_with_counts"], 0)
            self.assertIsNone(summary["terminal_truncation"]["tool_output_truncations"])
            self.assertIsNone(summary["terminal_truncation"]["induced_continuations"])
            session["timing_counter_totals"] = {
                "toolOutputTruncationCount": 0,
                "truncationInducedContinuationCount": 0,
            }
            summary = compact_report(report, path, "hash", 0)
            self.assertEqual(summary["terminal_truncation"]["sessions_with_counts"], 1)
            self.assertEqual(summary["terminal_truncation"]["tool_output_truncations"], 0)
            self.assertEqual(summary["terminal_truncation"]["induced_continuations"], 0)
            self.assertEqual(summary["totals"]["truncation_marker_results"], 1)


class CompactAuditReportTest(unittest.TestCase):
    def test_summary_exposes_provider_usage_and_unknown_coverage_without_inventing_reconciliation(self):
        from scripts.tool_result_audit import compact_report

        with tempfile.TemporaryDirectory() as temp:
            _, report = self.fixture(Path(temp))
            context = report["sessions"][0]["execution_context"]
            context["provider_usage_totals"] = {"inputTokens": 100, "cachedInputTokens": 80,
                "visibleOutputTokens": 7, "reasoningTokens": 3, "totalTokens": 110}
            context["coverage"].update(requests_with_usage=1, requests_without_usage=1)
            context["provider_usage_reconciliation"] = {"status": "matched"}
            # Older version-2 ledgers lack reconciliation: never promote them to matched.
            report["sessions"][1]["execution_context"].pop("provider_usage_reconciliation")
            summary = compact_report(report, Path(temp) / "ledger.json", "hash", 100, limit=2)
            self.assertEqual(summary["provider_usage_totals"], context["provider_usage_totals"])
            self.assertEqual(summary["totals"]["sessions_without_provider_usage"], 11)
            self.assertEqual(summary["totals"]["requests_without_usage"], 1)
            self.assertEqual(summary["provider_usage_reconciliation"], {"matched": 1, "unavailable": 11})
            self.assertIsNone(summary["sessions"]["items"][1]["provider_input_tokens"])

    def fixture(self, directory):
        import copy

        path = directory / "sample.jsonl"
        records = [{"type": "event_msg", "payload": {
            "type": "task_started", "turn_id": "unfinished",
        }}]
        for index in range(12):
            records.extend([
                {"type": "response_item", "payload": {
                    "type": "custom_tool_call", "call_id": str(index),
                    "name": "exec", "input": "Get-Content file.py",
                }},
                {"type": "response_item", "payload": {
                    "type": "custom_tool_call_output", "call_id": str(index),
                    "output": 'Warning: truncated output\n' + 'λ' * 1000,
                }},
            ])
        path.write_text("".join(json.dumps(row) + "\n" for row in records), encoding="utf-8")
        session = audit(path)
        # Arbitrarily large evidence/identifiers must stay in the ledger.
        session["session_id"] = "session" * 10000
        session["artifacts"] = [{"available": False, "id": "id" * 10000}]
        return path, {"version": 2, "limitations": ["fixture"],
                      "sessions": [copy.deepcopy(session) for _ in range(12)]}

    def test_projection_is_bounded_deterministic_and_all_references_resolve(self):
        from scripts.tool_result_audit import compact_report, encoded

        with tempfile.TemporaryDirectory() as temp:
            _, report = self.fixture(Path(temp))
            # Put the largest rows late and out of size order. Equal-size-only
            # fixtures cannot distinguish ranking from unchanged input order.
            for session_index, result_index, size in ((0, 5, 6000), (2, 3, 9000), (11, 7, 9000)):
                session = report["sessions"][session_index]
                row = session["results"][result_index]
                row["visible_bytes"] = size
                row["visible_estimated_tokens"] = (size + 3) // 4
                for field in ("visible_bytes", "visible_estimated_tokens"):
                    session["summary"][field] = sum(row[field] for row in session["results"])
            original = encoded(report)
            digest = hashlib.sha256(original).hexdigest()
            summary = compact_report(report, Path(temp) / "ledger.json", digest,
                                     len(original), limit=2)
            self.assertEqual(summary, compact_report(
                report, Path(temp) / "ledger.json", digest, len(original), limit=2,
            ))
            self.assertEqual(encoded(report), original)
            self.assertEqual(summary["report_sha256"], digest)
            self.assertEqual(summary["report_bytes"], len(original))
            self.assertEqual(summary["totals"]["results"], 144)
            self.assertEqual(summary["totals"]["unfinished_turns"], 12)
            self.assertLess(len(encoded(summary)), 4000)
            for field in ("sessions", "largest_results", "follow_up_candidates",
                          "unavailable_artifact_references"):
                selected = summary[field]
                self.assertEqual(len(selected["items"]), 2)
                self.assertEqual(selected["omitted"], selected["total"] - 2)
                for item in selected["items"]:
                    value = report
                    for component in item["pointer"].split("/")[1:]:
                        value = value[int(component)] if isinstance(value, list) else value[component]
                    self.assertIsInstance(value, dict)
                    if "visible_estimated_tokens" in item:
                        self.assertEqual(item["visible_bytes"], value["visible_bytes"])
            # Equal-size ties use report order, including across session boundaries.
            for field in ("largest_results", "follow_up_candidates"):
                self.assertEqual(
                    [item["pointer"] for item in summary[field]["items"]],
                    ["/sessions/2/results/3", "/sessions/11/results/7"],
                )
            for limit in (0, 11):
                with self.assertRaises(ValueError):
                    compact_report(report, Path(temp), digest, len(original), limit)

    def test_cli_writes_full_ledger_and_replays_without_producer_or_writes(self):
        import contextlib
        import io
        from unittest.mock import patch
        from scripts.tool_result_audit import main

        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            source, _ = self.fixture(directory)
            target = directory / "ledger.json"
            out = io.StringIO()
            with patch("sys.argv", ["audit", str(source), "--output", str(target)]), contextlib.redirect_stdout(out):
                main()
            summary = json.loads(out.getvalue())
            original = target.read_bytes()
            report = json.loads(original)
            self.assertEqual(len(report["sessions"][0]["results"]), 12)
            self.assertEqual(summary["report_sha256"], hashlib.sha256(original).hexdigest())
            self.assertEqual(summary["report_bytes"], len(original))
            replay = io.StringIO()
            with patch("sys.argv", ["audit", "--from-report", str(target)]), \
                    patch("scripts.tool_result_audit.audit", side_effect=AssertionError("must not rescan")), \
                    contextlib.redirect_stdout(replay):
                main()
            self.assertEqual(json.loads(replay.getvalue()), summary)
            self.assertEqual(target.read_bytes(), original)
            with patch("sys.argv", ["audit", str(source), "--output", str(target)]), \
                    patch("scripts.tool_result_audit.audit", side_effect=AssertionError("must not rescan existing report")):
                with self.assertRaises(FileExistsError):
                    main()
            self.assertEqual(target.read_bytes(), original)
            for args in (["--from-report", str(target), "--output", str(target)],
                         ["--from-report", str(target), str(source)],
                         ["--from-report", str(target), "--summary-limit", "11"], []):
                with patch("sys.argv", ["audit", *args]), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as error:
                        main()
                    self.assertEqual(error.exception.code, 2)


class AuditSourceFreshnessTest(unittest.TestCase):
    def test_exact_prefix_identity_distinguishes_growth_change_and_unavailability(self):
        from scripts.tool_result_audit import check_report_sources

        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "source.jsonl"
            original = b'not JSON: raw bytes are enough\r\n'
            path.write_bytes(original)
            report = {"sessions": [{"snapshot": {
                "path": str(path), "byteLength": len(original),
                "sha256": hashlib.sha256(original).hexdigest(),
            }}]}
            self.assertEqual(check_report_sources(report)[0]["status"], "unchanged")
            path.write_bytes(original + b'partial new record')
            check = check_report_sources(report)[0]
            self.assertEqual(check["status"], "appended")
            self.assertEqual(check["unaudited_bytes"], len(b'partial new record'))
            self.assertEqual(check["observed_sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
            # A same-size replacement must never pass a stat-only freshness check.
            path.write_bytes(b'x' * len(original))
            self.assertEqual(check_report_sources(report)[0]["status"], "changed")
            path.write_bytes(b'x' * (len(original) + 1))
            self.assertEqual(check_report_sources(report)[0]["status"], "changed")
            path.write_bytes(original[:5])
            self.assertEqual(check_report_sources(report)[0]["status"], "shortened")
            path.unlink()
            check = check_report_sources(report)[0]
            self.assertEqual(check["status"], "unavailable")
            self.assertEqual(check["error_type"], "FileNotFoundError")

    def test_compression_transition_is_not_mislabeled_as_source_loss(self):
        import io
        from unittest.mock import patch
        from scripts.rollout_snapshot import RolloutSnapshot
        from scripts.tool_result_audit import check_report_sources

        report = {"sessions": [{"snapshot": {
            "path": "source.jsonl", "byteLength": 999, "sha256": "a" * 64,
        }}] * 2}
        stream = io.BytesIO(b'compressed bytes')
        snapshot = RolloutSnapshot(Path("source.jsonl.zst"), stream, "b" * 64, 16)
        with patch("scripts.tool_result_audit.read_rollout_snapshot", return_value=snapshot) as read:
            checks = check_report_sources(report)
        read.assert_called_once()
        self.assertTrue(stream.closed)
        self.assertEqual(checks[0], checks[1])
        self.assertEqual(checks[0]["status"], "representation_changed")
        self.assertNotIn("unaudited_bytes", checks[0])

    def test_cli_check_does_not_reaudit_or_mutate_the_saved_report(self):
        import contextlib
        import io
        from unittest.mock import patch
        from scripts.tool_result_audit import main

        with tempfile.TemporaryDirectory() as temp:
            source, report = CompactAuditReportTest().fixture(Path(temp))
            saved = Path(temp) / "ledger.json"
            data = json.dumps(report).encode()
            saved.write_bytes(data)
            with source.open("ab") as handle:
                handle.write(b'new incomplete tail')
            with patch("scripts.tool_result_audit.audit", side_effect=AssertionError("no reaudit")):
                for enabled in (False, True):
                    output = io.StringIO()
                    args = ["audit", "--from-report", str(saved), "--summary-limit", "1"]
                    if enabled:
                        args.append("--check-sources")
                    with patch("sys.argv", args), contextlib.redirect_stdout(output):
                        main()
                    summary = json.loads(output.getvalue())
                    self.assertEqual(summary["totals"]["results"], 144)
                    self.assertEqual(summary["sessions"]["omitted"], 11)
                    if enabled:
                        self.assertEqual(summary["source_check"]["counts"], {"appended": 12})
                        check = summary["sessions"]["items"][0]["source_check"]
                        self.assertEqual(check["unaudited_bytes"], len(b'new incomplete tail'))
                    else:
                        self.assertEqual(summary["source_check"]["status"], "not_checked")
            self.assertEqual(saved.read_bytes(), data)
            with patch("sys.argv", ["audit", str(source), "--output", str(saved), "--check-sources"]), \
                    contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    main()
                self.assertEqual(error.exception.code, 2)


class AuditContractTest(unittest.TestCase):
    def test_all_input_paths_are_checked_before_analysis(self):
        from unittest.mock import patch
        from scripts.tool_result_audit import main
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            valid = root / "valid.jsonl"
            valid.write_text('{}\n', encoding="utf-8")
            destination = root / "report.json"
            for bad in [root / "missing.jsonl", root]:
                with patch("sys.argv", ["audit", str(valid), str(bad), "--output", str(destination)]), \
                        patch("scripts.tool_result_audit.audit", side_effect=AssertionError("must not partially analyze")):
                    with self.assertRaisesRegex(FileNotFoundError, "relative paths use cwd"):
                        main()
                self.assertFalse(destination.exists())

    def test_serialization_failure_never_publishes_partial_report(self):
        from unittest.mock import patch
        from scripts.tool_result_audit import main
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / "valid.jsonl"
            source.write_text('{}\n', encoding="utf-8")
            target = root / "report.json"
            with patch("sys.argv", ["audit", str(source), "--output", str(target)]), \
                    patch("scripts.tool_result_audit.audit", return_value={"bad": {1, 2}}):
                with self.assertRaises(TypeError):
                    main()
            self.assertEqual(list(root.iterdir()), [source])

    def test_describe_is_bounded_without_io_and_names_actual_record_fields(self):
        import contextlib
        import io
        from unittest.mock import patch
        from scripts.tool_result_audit import main, describe_contract

        out = io.StringIO()
        with patch("sys.argv", ["audit", "--describe"]), \
                patch("scripts.tool_result_audit.audit", side_effect=AssertionError("no producer")), \
                patch.object(Path, "read_bytes", side_effect=AssertionError("no reads")), \
                contextlib.redirect_stdout(out):
            main()
        contract = json.loads(out.getvalue())
        self.assertEqual(contract, describe_contract())
        self.assertLess(len(out.getvalue()), 4096)
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "session.jsonl"
            path.write_text('\n'.join(json.dumps(row) for row in [
                {"type":"response_item", "payload":{"type":"custom_tool_call", "call_id":"c", "name":"exec", "input":"text(1)"}},
                {"type":"response_item", "payload":{"type":"custom_tool_call_output", "call_id":"c", "output":"1"}},
            ]) + '\n', encoding="utf-8")
            session = audit(path)
            for selector, fields in contract["record_fields"].items():
                value = session
                for part in selector.removeprefix("/sessions/*/").split("/"):
                    value = value[0] if part == "*" else value[part]
                self.assertTrue(set(fields).issubset(value), selector)
        with patch("sys.argv", ["audit", "--describe", "missing.jsonl"]), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                main()
        self.assertEqual(raised.exception.code, 2)


if __name__ == "__main__":
    unittest.main()
