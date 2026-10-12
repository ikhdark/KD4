"""Offline safety/scoring regressions; these do not measure agent improvement."""
import asyncio
import copy
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

from scripts.benchmark_scripts import benchmark_investigation_coverage as bench


class FixtureTests(unittest.TestCase):
    def test_versioned_preflight_preserves_checksum_guard_and_legacy_inputs(self):
        legacy = bench.load_bundle(1)
        current = bench.load_bundle(2)
        self.assertEqual(current["version"], 2)
        self.assertEqual(current["cases"], legacy["cases"])
        self.assertNotEqual(current["baseline_sha256"], legacy["baseline_sha256"])
        fixtures, _ = bench.experiment_paths(2)
        with patch.object(bench, "ACTIVE", fixtures / "baseline.md"):
            with patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(bench.main(["check"]), 0)
            self.assertEqual(json.loads(output.getvalue())["experiment_version"], 2)
            with self.assertRaisesRegex(ValueError, "active baseline changed"):
                bench.main(["check", "--experiment-version", "1"])
        with patch.object(bench, "ACTIVE", bench.FIXTURES / "baseline.md"):
            self.assertEqual(bench.load_bundle(1, require_active=True)["version"], 1)

    def test_rebased_arms_change_only_the_original_causal_revision(self):
        old = (bench.FIXTURES / "baseline.md").read_text(encoding="utf-8")
        candidate = (bench.FIXTURES / "candidate.md").read_text(encoding="utf-8")
        fixtures, _ = bench.experiment_paths(2)
        baseline_v2 = (fixtures / "baseline.md").read_text(encoding="utf-8")
        candidate_v2 = (fixtures / "candidate.md").read_text(encoding="utf-8")
        original = "Stop inspecting when further reading would not change the answer."
        start = candidate.index("For a nontrivial investigation,")
        end = candidate.index(" For a review of a named file or unit,", start)
        replacement = candidate[start:end]
        self.assertEqual(old.replace(original, replacement), candidate)
        self.assertEqual(baseline_v2.replace(original, replacement), candidate_v2)

    def test_run_rejects_prompt_drift_before_creating_output_or_starting_model(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            active = root / "active.md"
            active.write_text("unversioned prompt change", encoding="utf-8")
            for version in (1, 2):
                args = SimpleNamespace(experiment_version=version, output=root / "not-created")
                with patch.object(bench, "ACTIVE", active), patch.object(bench.asyncio, "create_subprocess_exec") as start:
                    with self.assertRaisesRegex(ValueError, "active baseline changed"):
                        asyncio.run(bench.execute(args))
                    start.assert_not_called()
                self.assertFalse(args.output.exists())

    def test_archived_fixture_checks_do_not_require_reverting_shared_prompt_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            active = Path(directory) / "active.md"
            active.write_text("another session's legitimate prompt edit", encoding="utf-8")
            with patch.object(bench, "ACTIVE", active):
                self.assertEqual(len(bench.load_bundle()["cases"]), 4)
                with self.assertRaisesRegex(ValueError, "active baseline changed"):
                    bench.load_bundle(require_active=True)

    def test_frozen_prompt_and_schedule_preserve_paired_conditions(self):
        bundle = bench.load_bundle()
        scheduled = bench.schedule(bundle)
        self.assertEqual(len(scheduled), 26)  # Preregistered 3*4*2 + 1*2.
        for case in bundle["cases"]:
            selected = [r for r in scheduled if r[0] == case["id"]]
            self.assertEqual(len(selected), 8 if case["split"] == "heldout" else 2)
            for index in range(0, len(selected), 2):
                self.assertEqual(selected[index][1], selected[index + 1][1])
                self.assertEqual({selected[index][2], selected[index + 1][2]}, {"baseline", "candidate"})
            if len(selected) == 8:
                self.assertEqual(sum(selected[i][2] == "baseline" for i in range(0, 8, 2)), 2)

    def test_counterfactuals_cover_complete_disjoint_or_serial_state_space(self):
        # Independently enumerate every combination from the written cohort/
        # serial contracts, not from an agent's answer or scorer's current output.
        cases = {c["id"]: c for c in bench.load_bundle()["cases"]}
        for name, baseline, changes in (("export", 76, [10, 8, 6]), ("access", 18, [5, 4, 3])):
            expected = sorted(baseline + sum(changes[i] for i in range(3) if bits & (1 << i)) for bits in range(8))
            self.assertEqual(sorted(cases[name]["counterfactuals"]["table"]), expected)
        self.assertEqual(sorted(cases["latency"]["counterfactuals"]["table"]),
                         sorted(100 + sum([200, 100, 300][i] for i in range(3) if bits & (1 << i)) for bits in range(8)))

    def test_help_does_not_need_repository_working_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, "-B", str(Path(bench.__file__).resolve()), "--help"],
                                    cwd=directory, capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("review-packet", result.stdout)


class DispatchTests(unittest.IsolatedAsyncioTestCase):
    def server(self, messages=(), eof=True):
        class Input:
            def __init__(self):
                self.messages = []

            def write(self, raw):
                self.messages.append(json.loads(raw))

            async def drain(self):
                pass

        output = asyncio.StreamReader()
        for message in messages:
            output.feed_data((json.dumps(message) + "\n").encode())
        if eof:
            output.feed_eof()
        process = SimpleNamespace(stdin=Input(), stdout=output)
        return bench.InvestigationServer(process, io.StringIO(), {"incident.txt": "visible evidence"}), process

    def call(self, tool="read_evidence", arguments=None):
        return {"id": 1, "method": "item/tool/call", "params": {
            "threadId": "thread", "turnId": "turn", "callId": "call",
            "tool": tool, "arguments": arguments if arguments is not None else {"paths": ["incident.txt"]}}}

    async def test_real_dispatch_reads_exact_fixture_and_rejects_duplicate_delivery(self):
        server, process = self.server([self.call(), self.call()])
        await server.receive()
        response = json.loads(process.stdin.messages[0]["result"]["contentItems"][0]["text"])
        self.assertEqual(response["files"], [{"path": "incident.txt", "text": "visible evidence", "complete": True}])
        self.assertEqual(server.calls[0]["read_paths"], ["incident.txt"])
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            await server.receive()
        self.assertEqual(len(process.stdin.messages), 1)

    async def test_unknown_paths_cannot_reach_oracle_credentials_or_host(self):
        paths = ["../cases.json", "auth.json", str(bench.FIXTURES / "cases.json"), "candidate.md"]
        server, process = self.server([self.call(arguments={"paths": paths})])
        await server.receive()
        response = json.loads(process.stdin.messages[0]["result"]["contentItems"][0]["text"])
        self.assertTrue(all(set(row) == {"path", "error"} for row in response["files"]))
        self.assertEqual(server.calls[0]["read_paths"], [])

    async def test_invalid_tools_cannot_execute_host_commands(self):
        server, process = self.server([self.call("exec_command", {"cmd": "whoami"})])
        with self.assertRaisesRegex(RuntimeError, "unexpected tool"):
            await server.receive()
        self.assertEqual(process.stdin.messages, [])

    async def test_budget_is_bounded_even_with_invalid_paths(self):
        server, _ = self.server()
        for _ in range(32):
            server.dispatch("read_evidence", {"paths": ["missing"]})
        with self.assertRaises(bench.BudgetExceeded):
            server.dispatch("list_evidence", {})
        self.assertEqual(len(server.calls), 32)

    async def test_eof_and_stalled_stream_do_not_count_as_completion(self):
        server, _ = self.server()
        with self.assertRaisesRegex(RuntimeError, "EOF"):
            await server.receive()
        server, _ = self.server(eof=False)
        with self.assertRaises(TimeoutError):
            await asyncio.wait_for(server.receive(), .01)
        self.assertIsNone(server.completed)

    async def test_shutdown_kills_and_awaits_blocked_server(self):
        process = SimpleNamespace(returncode=None, stdin=SimpleNamespace(close=lambda: None),
                                  wait=AsyncMock(side_effect=[TimeoutError(), 0]), killed=False)
        process.kill = lambda: setattr(process, "killed", True)
        await bench.close_server(process)
        self.assertTrue(process.killed)
        self.assertEqual(process.wait.await_count, 2)

    async def test_trial_sends_only_arm_prompt_and_visible_question(self):
        server, _ = self.server()
        case = bench.load_bundle()["cases"][0]
        sent = []

        async def request(method, params):
            sent.append((method, params))
            if method == "thread/start":
                return {"thread": {"id": "t"}, "model": "fixed-model"}
            server.completed = {"status": "completed"}
            return {}

        server.request = request
        await server.trial(SimpleNamespace(model="fixed-model", effort="high"), Path("empty"), case, "candidate", "frozen candidate")
        self.assertEqual(sent[0][1]["baseInstructions"], "frozen candidate")
        self.assertEqual(sent[0][1]["environments"], [])
        self.assertNotIn("oracle", json.dumps(sent))
        self.assertEqual(sent[1][1]["input"][0]["text"], case["question"])


class ScoringTests(unittest.TestCase):
    def sample(self):
        case = bench.load_bundle()["cases"][0]
        paths = list(case["files"])
        row = {"case": "cargo", "arm": "candidate", "repeat": 0,
               "calls": [{"read_paths": paths}], "usage": {"totalTokens": 100}, "wall_seconds": 1}
        review = {"causes": {c["id"]: {"stage": "resolved", "evidence": c["evidence"], "rationale": c["claim"]} for c in case["causes"]},
                  "claims": [{"text": "Source changed before B", "supported": True, "rationale": "history and inputs"}],
                  "classification_errors": [], "unexplained_observations": [], "omitted_material_evidence": [], "premature_closure": False}
        return row, review, case

    def test_unread_evidence_or_missing_cause_cannot_be_scored_as_resolved(self):
        row, review, case = self.sample()
        row["calls"] = []
        with self.assertRaisesRegex(ValueError, "observed evidence"):
            bench.scored_row(row, review, case)
        row, review, case = self.sample()
        del review["causes"]["source_change"]
        with self.assertRaisesRegex(ValueError, "every cause"):
            bench.scored_row(row, review, case)

    def test_discovery_and_resolution_are_not_conflated(self):
        row, review, case = self.sample()
        review["causes"]["source_change"]["stage"] = "unresolved"
        score = bench.scored_row(row, review, case)
        self.assertTrue(score["complete"])
        self.assertFalse(score["resolved"])
        review["causes"]["source_change"]["stage"] = "not_discovered"
        self.assertEqual(bench.scored_row(row, review, case)["recall"], .5)

    def test_incorrect_identifications_without_reads_receive_no_discovery_credit(self):
        row, review, case = self.sample()
        row["calls"] = []
        review["claims"] = []
        for judgment in review["causes"].values():
            judgment.update(stage="incorrect", evidence=[], rationale="Identification rejected by evaluator")
        score = bench.scored_row(row, review, case)
        self.assertEqual(score["recall"], 0.0)
        self.assertFalse(score["complete"])
        self.assertFalse(score["resolved"])

    def test_discovery_counts_only_correct_identifications_in_mixed_reviews(self):
        stages = ("not_discovered", "incorrect", "unresolved", "resolved")
        for first in stages:
            for second in stages:
                with self.subTest(stages=(first, second)):
                    row, review, case = self.sample()
                    for judgment, stage in zip(review["causes"].values(), (first, second)):
                        judgment["stage"] = stage
                    expected = sum(stage in ("unresolved", "resolved") for stage in (first, second)) / 2
                    score = bench.scored_row(row, review, case)
                    self.assertEqual(score["recall"], expected)
                    self.assertEqual(score["complete"], expected == 1)
                    self.assertEqual(score["resolved"], first == second == "resolved")

    def test_incorrect_branches_cannot_satisfy_discovery_acceptance_gates(self):
        rows = []
        for case_id in ("a", "b", "c"):
            for repeat in range(4):
                for arm, stage in (("baseline", "not_discovered"), ("candidate", "incorrect")):
                    row, review, case = self.sample()
                    row.update(case=case_id, repeat=repeat, arm=arm, calls=[])
                    review["claims"] = []
                    for judgment in review["causes"].values():
                        judgment.update(stage=stage, evidence=[])
                    rows.append(bench.scored_row(row, review, case))
        result = bench.decide(rows)
        self.assertEqual(result["decision"], "retain_baseline")
        self.assertFalse(result["checks"]["complete_rate"])
        self.assertFalse(result["checks"]["complete_gain"])
        self.assertFalse(result["checks"]["recall_gain"])

    def test_missing_cost_metrics_block_acceptance(self):
        row, review, case = self.sample()
        row["usage"] = None
        with self.assertRaisesRegex(ValueError, "cost metric"):
            bench.scored_row(row, review, case)

    def scores(self, improved=False):
        row, review, case = self.sample()
        prototype = bench.scored_row(row, review, case)
        rows = []
        for case_id in ("a", "b", "c"):
            for arm in ("baseline", "candidate"):
                for repeat in range(4):
                    value = {**prototype, "case": case_id, "arm": arm, "repeat": repeat}
                    if improved and arm == "baseline" and repeat < 2:
                        value.update(complete=False, resolved=False, recall=2 / 3)
                    rows.append(value)
        return rows

    def test_baseline_ceiling_is_not_evidence_of_improvement(self):
        result = bench.decide(self.scores())
        self.assertEqual(result["decision"], "retain_baseline")
        self.assertFalse(result["checks"]["complete_gain"])

    def test_predeclared_gains_pass_but_expensive_or_unsupported_findings_fail(self):
        rows = self.scores(improved=True)
        self.assertEqual(bench.decide(rows)["decision"], "passes_bounded_gate_no_automatic_promotion")
        expensive = copy.deepcopy(rows)
        for row in expensive:
            if row["arm"] == "candidate":
                row["tokens"] *= 3
        self.assertEqual(bench.decide(expensive)["decision"], "retain_baseline")
        rows[-1]["unsupported"] = 1
        self.assertFalse(bench.decide(rows)["checks"]["unsupported_claims"])

    def test_blocked_or_missing_pairs_cannot_pass(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            bench.write_json(output / "run.json", {"status": "blocked"})
            with self.assertRaisesRegex(ValueError, "incomplete run"):
                bench.evaluate(output, output / "absent-review.json")

    def test_blind_packet_omits_arm_prompt_and_cost(self):
        row, _, case = self.sample()
        row.update(trial_id="opaque", status="completed", messages=["answer"])
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            bench.write_json(output / "frozen-cases.json", {"cases": [case]})
            (output / "trials.jsonl").write_text(json.dumps(row) + "\n", encoding="utf-8")
            packet = bench.review_packet(output)["packets"][0]
        self.assertFalse({"arm", "usage", "wall_seconds", "baseInstructions"} & packet.keys())
        self.assertEqual(packet["messages"], ["answer"])

    def test_complete_scoring_accepts_only_newline_equivalent_frozen_oracle(self):
        # Legacy manifests lack experiment_version and must keep their v1 binding.
        bundle = bench.load_bundle(1)
        cases = {c["id"]: c for c in bundle["cases"]}
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            source = (bench.FIXTURES / "cases.json").read_bytes()
            (output / "frozen-cases.json").write_bytes(source.replace(b"\n", b"\r\n"))
            bench.write_json(output / "run.json", {
                "status": "completed_review_pending", "cases_sha256": bench.digest(bench.FIXTURES / "cases.json"),
                "spec_sha256": bench.digest(bench.SPEC)})
            rows, reviews = [], []
            for index, (case_id, repeat, arm) in enumerate(bench.schedule(bundle)):
                case = cases[case_id]
                row, review, _ = self.sample()
                row.update(trial_id=str(index), case=case_id, repeat=repeat, arm=arm, status="completed",
                           calls=[{"read_paths": list(case["files"])}],
                           messages=[json.dumps({"findings": [], "outcome": "offline scorer fixture", "unresolved": []})])
                review.update(trial_id=str(index), causes={c["id"]: {
                    "stage": "resolved", "evidence": c["evidence"], "rationale": "offline review fixture"} for c in case["causes"]})
                rows.append(row)
                reviews.append(review)
            (output / "trials.jsonl").write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
            review_path = output / "review.json"
            review = {"trials_sha256": bench.digest(output / "trials.jsonl"),
                      "cases_sha256": bench.digest(output / "frozen-cases.json"), "reviews": reviews}
            bench.write_json(review_path, review)
            result = bench.evaluate(output, review_path)
            self.assertEqual(result["decision"], "retain_baseline")  # Both at ceiling.
            self.assertEqual(result["experiment_version"], 1)
            self.assertEqual(result["scorer_revision"], 2)
            (output / "frozen-cases.json").write_bytes(source.replace(b"paid-only", b"free-only"))
            review["cases_sha256"] = bench.digest(output / "frozen-cases.json")
            bench.write_json(review_path, review)
            with self.assertRaisesRegex(ValueError, "oracle"):
                bench.evaluate(output, review_path)

    def test_score_rejects_cross_version_oracle_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            bench.write_json(output / "run.json", {"status": "completed_review_pending", "experiment_version": 2})
            bench.write_json(output / "frozen-cases.json", bench.load_bundle(1))
            with self.assertRaisesRegex(ValueError, "experiment versions differ"):
                bench.evaluate(output, output / "absent-review.json")

    def test_malformed_final_answers_are_not_silent_success(self):
        for messages in ([], ["not JSON"], ["[]"], ["{}"],
                         [json.dumps({"findings": [], "outcome": "x", "unresolved": [False]})]):
            with self.assertRaisesRegex(ValueError, "malformed"):
                bench.parse_answer(messages)


if __name__ == "__main__":
    unittest.main()
