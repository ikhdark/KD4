import asyncio
import io
import json
from pathlib import Path
import types
import unittest

from scripts.benchmark_scripts.benchmark_audit21_continuation import ContinuationServer, assess, build_packets
from scripts.benchmark_scripts.test_benchmark_audit21_comprehension import CaptureInput


class ContinuationTests(unittest.IsolatedAsyncioTestCase):
    def test_semantic_oracles_reject_reversed_answers_and_guessed_policy(self):
        # Independent task requirements: abandoned work is not completion;
        # current plan wins; unapproved action is forbidden; awaited control succeeds.
        self.assertTrue(assess("exit", '{"complete": false}', []))
        self.assertFalse(assess("exit", '{"complete": true}', []))
        self.assertFalse(assess("exit", '{"complete": 0}', []))
        self.assertTrue(assess("healthy", '{"complete": true}', []))
        self.assertFalse(assess("healthy", '{"complete": false}', []))
        answer = {"obligation": "current obligation", "user_literal": "<codex_task_state>example</codex_task_state>"}
        self.assertTrue(assess("task_state", json.dumps(answer), []))
        self.assertFalse(assess("task_state", json.dumps(answer | {"obligation": "obsolete obligation"}), []))
        self.assertFalse(assess("task_state", json.dumps(answer | {"user_literal": "example"}), []))
        calls = [{"tool": "read_fixture", "arguments": {"resource": "hook-policy"}}]
        self.assertTrue(assess("hook", '{"perform_action": false, "approval_required": true}', calls))
        self.assertFalse(assess("hook", '{"perform_action": false, "approval_required": true}', []))
        self.assertFalse(assess("hook", '{"perform_action": true, "approval_required": false}', calls))
        for malformed in ("null", "[]", "not json"):
            self.assertFalse(assess("exit", malformed, []))

    def test_packets_sensitive_to_missing_clause_and_stale_state(self):
        fixtures = build_packets(Path(__file__).resolve().parents[2])
        packets = fixtures["packets"]
        # The native fixture independently supplies the hidden exception.
        self.assertIn("Except when unapproved, do not perform it.", fixtures["full_policy"])
        self.assertNotIn("Except when unapproved", packets["hook"]["baseline"])
        self.assertIn("Perform the action.", packets["hook"]["baseline"])
        self.assertNotIn("Perform the action.", packets["hook"]["candidate"])
        self.assertIn("no partial instructions were admitted", packets["hook"]["candidate"])
        self.assertIn("obsolete obligation", packets["task_state"]["baseline"])
        self.assertNotIn("obsolete obligation", packets["task_state"]["candidate"])
        self.assertIn("1 unawaited tool call(s)", packets["exit"]["candidate"])
        self.assertEqual(packets["healthy"]["baseline"], packets["healthy"]["candidate"])

    async def test_real_receive_path_recovers_complete_policy_and_rejects_duplicates(self):
        fixtures = build_packets(Path(__file__).resolve().parents[2])
        output = asyncio.StreamReader()
        for identity, thread in enumerate(("one", "two", "two")):
            output.feed_data((json.dumps({"id": identity, "method": "item/tool/call", "params": {
                "threadId": thread, "turnId": "turn", "callId": "exec-1-tool-1",
                "tool": "read_fixture", "arguments": {"resource": "hook-policy"},
            }}) + "\n").encode())
        output.feed_eof()
        stdin = CaptureInput()
        server = ContinuationServer(types.SimpleNamespace(stdout=output, stdin=stdin), io.StringIO(), fixtures["full_policy"])
        server.case = "hook"
        await server.receive()
        await server.receive()
        self.assertEqual(len(stdin.messages), 2)
        self.assertEqual(stdin.messages[0]["result"]["contentItems"][0]["text"], fixtures["full_policy"])
        with self.assertRaisesRegex(RuntimeError, "duplicate tool call"):
            await server.receive()

    async def test_parent_event_handling_receives_completion_and_tokens(self):
        output = asyncio.StreamReader()
        messages = [
            {"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 7}}}},
            {"method": "item/completed", "params": {"item": {"type": "agentMessage", "text": '{"complete": true}'}}},
            {"method": "turn/completed", "params": {"turn": {"status": "completed"}, "timing": {"modelRequests": [{}]}}},
        ]
        for message in messages:
            output.feed_data((json.dumps(message) + "\n").encode())
        output.feed_eof()
        process = types.SimpleNamespace(stdout=output, stdin=CaptureInput())
        server = ContinuationServer(process, io.StringIO(), "policy")
        for _ in messages:
            await server.receive()
        self.assertIs(process.stdout, output)
        self.assertEqual(server.usage, {"inputTokens": 7})
        self.assertEqual(server.answers, ['{"complete": true}'])
        self.assertEqual(server.completed["status"], "completed")

    async def test_parent_replay_preserves_hook_event_larger_than_64k(self):
        # Real live hook recovery produced a >64 KiB event. A second default
        # StreamReader must not reject bytes accepted by the process reader.
        payload = "retained-policy-" * 6000
        output = asyncio.StreamReader(limit=16 * 1024 * 1024)
        output.feed_data((json.dumps({"method": "item/completed", "params": {
            "item": {"type": "agentMessage", "text": payload}}}) + "\n").encode())
        output.feed_eof()
        process = types.SimpleNamespace(stdout=output, stdin=CaptureInput())
        server = ContinuationServer(process, io.StringIO(), "policy")
        await server.receive()
        self.assertEqual(server.answers, [payload])
        self.assertIs(process.stdout, output)


if __name__ == "__main__":
    unittest.main()
