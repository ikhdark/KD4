import json
import unittest
from unittest import mock

from scripts.benchmark_model_payload import fixture, paired_summary, receive, summarize


class FakeSocket:
    def __init__(self, events):
        self.events = iter(events)
        self.sent = []

    def send(self, text):
        self.sent.append(json.loads(text))

    def recv(self, timeout):
        assert timeout > 0
        return json.dumps(next(self.events))


class PayloadBenchmarkTests(unittest.TestCase):
    def test_reduction_keeps_every_unique_record_and_later_correction(self):
        repeated, query, expected = fixture(32, 4, "fixed")
        unique, unique_query, unique_expected = fixture(32, 1, "fixed")
        self.assertEqual(query, unique_query)
        self.assertEqual(expected, unique_expected)
        self.assertEqual(set(repeated[1]["content"][0]["text"].splitlines()),
                         set(unique[1]["content"][0]["text"].splitlines()))
        self.assertEqual(len(unique[1]["content"][0]["text"].splitlines()), 32)
        self.assertEqual(repeated[1]["content"][0]["text"], unique[1]["content"][0]["text"] * 4)
        self.assertEqual(repeated[-1], query)
        self.assertEqual(unique[-1], query)
        self.assertEqual(set(expected), {"key-0000", "key-0016", "key-0031"})
        self.assertEqual(expected["key-0016"], "corrected-value")
        # The answer oracle must agree with the delivered records, not just
        # another call to the same fixture builder with the same defect.
        records = dict(line.split("=", 1) for line in unique[1]["content"][0]["text"].splitlines())
        self.assertEqual(expected, {
            "key-0000": records["key-0000"],
            "key-0016": "corrected-value",
            "key-0031": records["key-0031"],
        })

    def test_summary_keeps_cohorts_missing_values_and_independent_medians(self):
        def row(scenario="deduplicate", variant="unique", phase="cold", **metrics):
            return {
                "scenario": scenario, "variant": variant, "phase": phase,
                "correct": True, "wire_bytes": 400, "serialization_ms": 8,
                "send_ms": 6, "ttft_ms": 12, "wall_ms": 40,
                "post_first_text_ms": 28, "input_tokens": 40, "cached_tokens": 0,
                **metrics,
            }

        rows = [
            row(wire_bytes=100, serialization_ms=1, send_ms=9, ttft_ms=None,
                wall_ms=90, post_first_text_ms=None, input_tokens=10, cached_tokens=None),
            row(scenario="inherit"),
            row(correct=False, wire_bytes=300, serialization_ms=7, send_ms=1, ttft_ms=2,
                wall_ms=10, post_first_text_ms=6, input_tokens=None, cached_tokens=None),
            row(variant="repeated", wall_ms=50),
            row(wire_bytes=200, serialization_ms=3, send_ms=4, ttft_ms=8,
                wall_ms=20, post_first_text_ms=2, input_tokens=30, cached_tokens=None),
            row(phase="warm", wall_ms=60),
        ]
        expected = [{
            "scenario": "deduplicate", "variant": "unique", "phase": "cold",
            "n": 3, "correct": 2, "wire_bytes": 200, "serialization_ms": 3,
            "send_ms": 4, "ttft_ms": 5, "wall_ms": 20,
            "post_first_text_ms": 4, "input_tokens": 20,
        }]
        for scenario, variant, phase, wall in (
            ("inherit", "unique", "cold", 40),
            ("deduplicate", "repeated", "cold", 50),
            ("deduplicate", "unique", "warm", 60),
        ):
            expected.append({
                "scenario": scenario, "variant": variant, "phase": phase,
                "n": 1, "correct": 1, "wire_bytes": 400, "serialization_ms": 8,
                "send_ms": 6, "ttft_ms": 12, "wall_ms": wall,
                "post_first_text_ms": 28, "input_tokens": 40, "cached_tokens": 0,
            })
        self.assertEqual(summarize(rows), expected)

    def test_summary_separates_returned_service_tiers(self):
        base = {"scenario": "inherit", "variant": "full", "phase": "warm", "correct": True,
                "wire_bytes": 1, "serialization_ms": 1, "send_ms": 1, "ttft_ms": 2,
                "wall_ms": 3, "post_first_text_ms": 1, "input_tokens": 1, "cached_tokens": 0,
                "returned_model": "same", "returned_effort": "high"}
        rows = summarize([{**base, "returned_service_tier": "default"},
                          {**base, "returned_service_tier": "priority", "ttft_ms": 100}])
        self.assertEqual([(r["returned_service_tier"], r["n"], r["ttft_ms"]) for r in rows],
                         [("default", 1, 2), ("priority", 1, 100)])

    def test_paired_latency_uses_pair_deltas_and_includes_connection_setup(self):
        rows = []
        # Pair deltas [10, 10, -99] have median 10, but subtracting the
        # two group medians would report 11 - 100 = -89 (wrong direction).
        for pair, (before, after) in enumerate(((0, 10), (100, 110), (110, 11))):
            for variant, value, setup in (("full", before, 20), ("delta", after, 5)):
                rows.append({"scenario": "inherit", "phase": "cold", "pair": pair,
                             "variant": variant, "ttft_ms": value, "wall_ms": value + 30,
                             "connection_setup_ms": setup, "correct": True,
                             "returned_model": "same", "returned_effort": "high",
                             "returned_service_tier": "default"})
        report = paired_summary(rows)
        self.assertEqual(report["excluded_pairs"], {})
        metrics = report["groups"][0]["metrics"]
        self.assertEqual(metrics["ttft_ms"], {"n": 3, "median_delta_ms": 10,
                         "min_delta_ms": -99, "max_delta_ms": 10, "candidate_faster_pairs": 1})
        self.assertEqual(metrics["wall_ms"], metrics["ttft_ms"])
        self.assertEqual(metrics["setup_inclusive_wall_ms"]["median_delta_ms"], -5)
        self.assertEqual(metrics["setup_inclusive_ttft_ms"]["candidate_faster_pairs"], 3)

    def test_paired_latency_rejects_invalid_pairs_without_losing_valid_siblings(self):
        before = {"scenario": "deduplicate", "phase": "warm", "pair": 0,
                  "variant": "repeated", "correct": True, "returned_model": "same",
                  "returned_effort": "high", "returned_service_tier": "default",
                  "ttft_ms": None, "wall_ms": 5}
        after = {**before, "variant": "unique", "wall_ms": 4}
        rows = [before, after]
        for pair, mutation in enumerate(({"returned_service_tier": "priority"},
                                          {"returned_effort": None}, {"correct": False}), 1):
            rows.extend([{**before, "pair": pair}, {**after, "pair": pair, **mutation}])
        rows.extend([{**before, "pair": 4}, {**before, "pair": 5},
                     {**after, "pair": 5}, {**after, "pair": 5}])
        report = paired_summary(rows)
        self.assertEqual(report["excluded_pairs"], {"unknown_or_mismatched_provider_cohort": 2,
                         "incorrect_answer": 1, "incomplete_or_duplicate_pair": 2})
        self.assertEqual(set(report["groups"][0]["metrics"]), {"wall_ms"})
        self.assertEqual(report["groups"][0]["metrics"]["wall_ms"]["median_delta_ms"], -1)
        self.assertEqual(paired_summary([])["groups"], [])

    def test_first_token_is_text_not_created_or_reasoning_item(self):
        now = [0.0]
        events = [
            {"type": "response.created"},
            {"type": "response.output_item.added", "item": {"type": "reasoning"}},
            {"type": "response.output_text.delta", "delta": ""},
            {"type": "response.output_text.delta", "delta": '{"answer":'},
            {"type": "response.output_text.delta", "delta": '42}'},
            {"type": "response.completed", "response": {"status": "completed"}},
        ]

        class TimedSocket(FakeSocket):
            def send(self, text):
                super().send(text)
                now[0] += 1

            def recv(self, timeout):
                now[0] += 2
                return super().recv(timeout)

        socket = TimedSocket(events)
        from scripts.benchmark_model_payload import encode

        def timed_encode(value):
            now[0] += 1
            return encode(value)

        # Encoding takes 1s, send takes 1s, and each of the six events takes
        # 2s. The fourth event is first nonempty text: 2 + 4*2 = 10s.
        # Clock reads themselves cost nothing, independent of implementation.
        with (
            mock.patch("scripts.benchmark_model_payload.time.perf_counter", side_effect=lambda: now[0]),
            mock.patch("scripts.benchmark_model_payload.encode", side_effect=timed_encode),
        ):
            row, response = receive(socket, {"input": []}, {"answer": 42}, 20)
        self.assertTrue(row["correct"])
        self.assertEqual(row["first_provider_event_ms"], 4000)
        self.assertEqual(row["ttft_ms"], 10000)
        self.assertEqual(row["wall_ms"], 14000)
        self.assertEqual(row["post_first_text_ms"], 4000)
        self.assertEqual(row["serialization_ms"], 1000)
        self.assertEqual(row["send_ms"], 1000)
        self.assertEqual(socket.sent, [{"input": []}])
        self.assertEqual(response, {"status": "completed"})
        self.assertEqual(row["output"], '{"answer":42}')

    def test_completed_without_text_is_not_a_ttft_or_correct_answer(self):
        socket = FakeSocket([{"type": "response.completed", "response": {"status": "completed"}}])
        row, _ = receive(socket, {}, {}, 2)
        self.assertIsNone(row["ttft_ms"])
        self.assertFalse(row["correct"])
        self.assertEqual(summarize([]), [])

    def test_failed_provider_response_cannot_count_as_completion(self):
        for kind in ("error", "response.failed", "response.incomplete"):
            with self.subTest(kind=kind):
                socket = FakeSocket([{"type": kind, "message": "private details"}])
                with self.assertRaises(RuntimeError) as raised:
                    receive(socket, {}, {}, 2)
                self.assertEqual(str(raised.exception), f"provider returned {kind}")


if __name__ == "__main__":
    unittest.main()
