import asyncio
import io
import json
from pathlib import Path
import tempfile
import types
import unittest

from scripts.benchmark_scripts.benchmark_audit21_comprehension import BenchmarkServer
from scripts.benchmark_scripts.benchmark_audit21_comprehension import assess


class CaptureInput:
    def __init__(self):
        self.messages = []

    def write(self, value):
        self.messages.append(json.loads(value))

    async def drain(self):
        pass


class BenchmarkTests(unittest.IsolatedAsyncioTestCase):
    async def test_tool_call_identity_is_thread_and_turn_scoped(self):
        # Code-mode nested call IDs restart in each thread. Equal leaf IDs do
        # not mean a repeated execution; an equal full identity does.
        output = asyncio.StreamReader()
        for identity, thread in enumerate(["first", "second", "second"]):
            output.feed_data((json.dumps({"id": identity, "method": "item/tool/call", "params": {
                "threadId": thread, "turnId": "turn", "callId": "exec-1-tool-1",
                "tool": "read_marker", "arguments": {},
            }}) + "\n").encode())
        output.feed_eof()
        stdin = CaptureInput()
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / "marker.txt"
            marker.write_text("independent fixture value", encoding="utf-8")
            server = BenchmarkServer(types.SimpleNamespace(stdout=output, stdin=stdin), io.StringIO(), marker, "failure")
            await server.receive()
            await server.receive()
            self.assertEqual(len(stdin.messages), 2)
            for message in stdin.messages:
                value = json.loads(message["result"]["contentItems"][0]["text"])
                self.assertEqual(value["marker"], "independent fixture value")
            with self.assertRaisesRegex(RuntimeError, "duplicate tool call"):
                await server.receive()
            self.assertEqual(len(stdin.messages), 2)

    def test_grading_requires_task_outcome_not_plausible_prose(self):
        correct = {"environment_id": "secondary", "status": "ready", "cause": "", "marker": "expected"}
        self.assertTrue(assess("route", json.dumps(correct), "expected"))
        for change in [{"environment_id": "primary"}, {"status": "failed"}, {"marker": "guessed"}]:
            self.assertFalse(assess("route", json.dumps(correct | change), "expected"))
        failure = {"environment_id": "primary", "status": "failed", "cause": "unsupported shell /bin/fictional", "marker": None}
        self.assertTrue(assess("cause", json.dumps(failure), "expected"))
        self.assertFalse(assess("cause", json.dumps(failure | {"cause": "unknown"}), "expected"))
        for malformed in ["[]", "null", "not JSON"]:
            self.assertFalse(assess("route", malformed, "expected"))


if __name__ == "__main__":
    unittest.main()
