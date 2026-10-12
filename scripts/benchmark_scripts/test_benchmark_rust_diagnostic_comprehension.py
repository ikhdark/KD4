"""Offline checks for the opt-in live diagnostic benchmark; no model calls."""
import asyncio
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

from scripts.benchmark_scripts import benchmark_rust_diagnostic_comprehension as benchmark


SOURCE = """use std::sync::Mutex;
fn require_send<T: Send>(_: T) {}
async fn holds_guard(lock: &Mutex<u8>) {
    let guard = lock.lock().unwrap();
    async {}.await;
    drop(guard);
}
fn main() { require_send(holds_guard(&Mutex::new(0))); }
"""
DIAGNOSTIC = """std::sync::MutexGuard<'_, u8>
guard.rs:5:14
guard.rs:2:20
future is not `Send` as this value is used across an await
"""


class DiagnosticScoringTests(unittest.TestCase):
    def test_source_and_compiler_evidence_are_required_before_scoring(self):
        expected = benchmark.ground_truth(SOURCE, DIAGNOSTIC)
        self.assertEqual((expected["guard_line"], expected["suspension_line"], expected["bound_line"]), (4, 5, 2))
        with self.assertRaises(ValueError):
            benchmark.ground_truth(SOURCE.replace("async {}.await;", "drop(guard);"), DIAGNOSTIC)
        with self.assertRaises(ValueError):
            benchmark.ground_truth(SOURCE, DIAGNOSTIC.replace("guard.rs:5:14", "guard.rs:8:26"))

    def test_score_rejects_wrong_type_or_location_and_never_proves_prose(self):
        expected = benchmark.ground_truth(SOURCE, DIAGNOSTIC)
        answer = {"blocked_type": "std::sync::MutexGuard<'_, u8>", "suspension_line": 5,
                  "bound_line": 2, "mechanism": "unverified prose", "narrow_repair": "unverified prose"}
        scored = benchmark.assess(json.dumps(answer), expected)
        self.assertTrue(scored["factual_correct"])
        self.assertIn("pending independent review", scored["mechanism_and_repair_review"])
        for key, value in (("blocked_type", "std::sync::Mutex<u8>"), ("suspension_line", 4),
                           ("bound_line", 8), ("bound_line", "2"), ("bound_line", True)):
            self.assertFalse(benchmark.assess(json.dumps({**answer, key: value}), expected)["factual_correct"])
        for invalid in ("not JSON", "[]", "null"):
            self.assertFalse(benchmark.assess(invalid, expected)["factual_correct"])

    def test_cli_help_works_without_repository_cwd(self):
        script = Path(benchmark.__file__).resolve()
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, "-B", str(script), "--help"], cwd=directory,
                                    capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--fixture", result.stdout)


class FixtureDispatchTests(unittest.IsolatedAsyncioTestCase):
    def server(self, messages):
        class Input:
            def __init__(self):
                self.messages = []

            def write(self, value):
                self.messages.append(json.loads(value))

            async def drain(self):
                pass

        output = asyncio.StreamReader()
        for message in messages:
            output.feed_data((json.dumps(message) + "\n").encode())
        output.feed_eof()
        process = SimpleNamespace(stdin=Input(), stdout=output)
        return benchmark.DiagnosticServer(process, io.StringIO(), DIAGNOSTIC), process

    def call(self, tool="read_full_diagnostic", arguments=None):
        return {"id": 1, "method": "item/tool/call", "params": {
            "threadId": "thread", "turnId": "turn", "callId": "call",
            "tool": tool, "arguments": {} if arguments is None else arguments}}

    async def test_real_dispatch_returns_only_exact_fixture_and_rejects_duplicate(self):
        server, process = self.server([self.call(), self.call()])
        await server.receive()
        self.assertEqual(process.stdin.messages[0]["result"]["contentItems"],
                         [{"type": "inputText", "text": DIAGNOSTIC}])
        with self.assertRaisesRegex(RuntimeError, "duplicate"):
            await server.receive()
        self.assertEqual(len(process.stdin.messages), 1)
        self.assertEqual(len(server.calls), 1)

    async def test_unexpected_host_tool_or_path_cannot_execute(self):
        for call in (self.call("exec_command"), self.call(arguments={"path": "auth.json"})):
            server, process = self.server([call])
            with self.assertRaisesRegex(RuntimeError, "unexpected tool or arguments"):
                await server.receive()
            self.assertEqual(process.stdin.messages, [])
            self.assertEqual(server.calls, [])


if __name__ == "__main__":
    unittest.main()
