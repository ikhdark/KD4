"""A real dispatch marker must dominate a later busy-shaped result."""

import json
import tempfile
import unittest
from pathlib import Path

from scripts.build.full_suite_dispatch import record_dispatch, record_test_dispatch


class FullSuiteDispatchTest(unittest.TestCase):
    def test_busy_after_dispatch_never_refunds_and_unknown_stays_unknown(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "ack.json"
            env = {
                "CODEX_FULL_SUITE_ACK_PATH": str(path),
                "CODEX_FULL_SUITE_ATTEMPT": "attempt",
            }
            self.assertFalse(
                path.exists(), "missing acknowledgement cannot imply nonexecution"
            )
            record_test_dispatch(["cargo", "nextest", "run"], env)
            record_dispatch("not_started", env)
            self.assertEqual(json.loads(path.read_text())["status"], "started")
            self.assertEqual(json.loads(path.read_text())["attempt"], "attempt")

    def test_owned_admission_can_acknowledge_zero_dispatch(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "ack.json"
            env = {
                "CODEX_FULL_SUITE_ACK_PATH": str(path),
                "CODEX_FULL_SUITE_ATTEMPT": "attempt",
            }
            record_test_dispatch(["cargo", "nextest", "list"], env)
            self.assertFalse(path.exists())
            record_dispatch("not_started", env)
            self.assertEqual(json.loads(path.read_text())["status"], "not_started")
