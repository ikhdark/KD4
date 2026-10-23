"""Regression coverage for bounded Nextest evidence and Cargo status parsing."""

import json
import subprocess
import unittest

from scripts.build import rust_test_runner as runner


class NextestEvidenceScannerTests(unittest.TestCase):
    def test_every_escape_chunk_boundary_preserves_event(self):
        event = {"type": "test", "name": 'a"\\\n\t\u1234', "event": "ok"}
        encoded = json.dumps(event, ensure_ascii=True) + "\n"
        for boundary in range(len(encoded) + 1):
            with self.subTest(boundary=boundary):
                lines = list(
                    runner._nextest_json_lines(
                        [encoded[:boundary], encoded[boundary:]]
                    )
                )
                self.assertEqual([json.loads(line) for line in lines], [event])

    def test_oversized_payload_does_not_forge_an_event(self):
        payload = 'x' * 70_000 + r'\",\"type\":\"test\",\"event\":\"ok\"'
        encoded = json.dumps({"type": "test", "event": "failed", "stdout": payload})
        result = subprocess.CompletedProcess([], 2, encoded + "\n", "")
        events = list(runner._nextest_events(result))
        self.assertEqual(events, [{"type": "test", "event": "failed", "stdout": None}])
        self.assertEqual(result.stdout, encoded + "\n")

    def test_encoded_string_limit_includes_quotes(self):
        for length, retained in [(65_534, True), (65_535, False)]:
            value = "x" * length
            record = json.dumps({"payload": value})
            parsed = json.loads(next(iter(runner._nextest_json_lines([record]))))
            self.assertEqual(parsed["payload"], value if retained else None)

    def test_invalid_oversized_and_truncated_records_are_rejected(self):
        good = '{"type":"test","event":"ok"}\n'
        for suffix in [r'\q"}', r'\u12Z4"}', '\x01"}', '\\', ""]:
            with self.subTest(suffix=suffix):
                malformed = '{"stdout":"' + "x" * 70_000 + suffix + "\n"
                self.assertEqual(
                    list(runner._nextest_json_lines([malformed + good])),
                    [good.rstrip("\n")],
                )

    def test_metadata_bound_does_not_hide_next_record(self):
        oversized = "[" + "0," * 524_289 + "0]\n"
        good = '{"type":"suite","event":"ok"}'
        self.assertEqual(list(runner._nextest_json_lines([oversized + good])), [good])

    def test_large_escaped_spans_avoid_python_character_iteration(self):
        class CountedString(str):
            reads = 0

            def __getitem__(self, key):
                if isinstance(key, int):
                    type(self).reads += 1
                return super().__getitem__(key)

        chunk = CountedString(
            '{"type":"test","stdout":"' + r'ab\n\u0041' * 32_768 + '","event":"failed"}\n'
        )
        parsed = json.loads(next(iter(runner._nextest_json_lines([chunk]))))
        self.assertEqual(parsed, {"type": "test", "stdout": None, "event": "failed"})
        self.assertLess(CountedString.reads, 100)

    def test_only_cargo_status_position_supplies_duration(self):
        result = subprocess.CompletedProcess(
            [],
            2,
            "",
            "\x1b[32m    Finished\x1b[0m test profile in 1m 2.5s\n"
            + "diagnostic: Finished test profile in 999s\n"
            + "diagnostic: " + "Finished " * 20_000 + "not a status\n",
        )
        self.assertEqual(
            runner.RustTestRunner._reported_durations(result),
            {"cargo-reported": 62.5},
        )
