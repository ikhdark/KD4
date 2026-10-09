import json
import unittest

from scripts.benchmark_model_payload import fixture, receive, summarize


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
        self.assertEqual(expected["key-0016"], "corrected-value")

    def test_first_token_is_text_not_created_or_reasoning_item(self):
        socket = FakeSocket([
            {"type": "response.created"},
            {"type": "response.output_item.added", "item": {"type": "reasoning"}},
            {"type": "response.output_text.delta", "delta": '{"answer":'},
            {"type": "response.output_text.delta", "delta": '42}'},
            {"type": "response.completed", "response": {"status": "completed"}},
        ])
        row, _ = receive(socket, {"input": []}, {"answer": 42}, 2)
        self.assertTrue(row["correct"])
        self.assertLessEqual(row["first_provider_event_ms"], row["ttft_ms"])
        self.assertLessEqual(row["ttft_ms"], row["wall_ms"])
        self.assertEqual(row["output"], '{"answer":42}')

    def test_completed_without_text_is_not_a_ttft_or_correct_answer(self):
        socket = FakeSocket([{"type": "response.completed", "response": {"status": "completed"}}])
        row, _ = receive(socket, {}, {}, 2)
        self.assertIsNone(row["ttft_ms"])
        self.assertFalse(row["correct"])
        self.assertEqual(summarize([]), [])

    def test_failed_provider_response_cannot_count_as_completion(self):
        socket = FakeSocket([{"type": "error", "message": "private details"}])
        with self.assertRaisesRegex(RuntimeError, "^provider returned error$"):
            receive(socket, {}, {}, 2)


if __name__ == "__main__":
    unittest.main()
