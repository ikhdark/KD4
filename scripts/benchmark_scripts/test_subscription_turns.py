"""Subscription quota observations through the real saved-rollout reporting path."""
from __future__ import annotations

import copy
import datetime as dt
import hashlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

try:
    from compression import zstd
except ImportError:
    from backports import zstd

from scripts.benchmark_scripts import kd4_turn_latency_audit as audit


def file_hash(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def event(kind, second, turn=None, **payload):
    stamp = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc) + dt.timedelta(seconds=second)
    return {"type": "event_msg", "timestamp": stamp.isoformat(),
            "payload": {"type": kind, **({"turn_id": turn} if turn else {}), **payload}}


def quota(used, *, minutes=10080, reset=2_000_000_000, plan="pro", limit="codex", bucket="primary"):
    return {"limit_id": limit, "plan_type": plan,
            bucket: {"used_percent": used, "window_minutes": minutes, "resets_at": reset}}


def sample(used, second, turn=None, **limits):
    return event("token_count", second, turn, info=None, rate_limits=quota(used, **limits))


class SubscriptionTurnTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.source = self.root / "rollout.jsonl"

    def write(self, rows, *, path=None, tail=""):
        path = path or self.source
        meta = {"type": "session_meta", "payload": {"id": path.stem, "cwd": str(self.root)}}
        data = ("\n".join(json.dumps(row) for row in [meta, *rows]) + "\n" + tail).encode()
        path.write_bytes(zstd.compress(data) if path.suffix == ".zst" else data)
        return path

    def analyze(self, path=None, **options):
        path = path or self.source
        before = file_hash(path) if path.is_file() else None
        report = audit.analyze_session_path(path, self.root, **options)
        if before is not None:
            self.assertEqual(file_hash(path), before)
        return report, report["sessionDiagnostics"]["subscriptionUsage"]

    def test_first_turn_snapshot_never_borrows_previous_turn_balance(self):
        self.write([
            event("task_started", 0, "before"), sample(10, 1),
            event("task_complete", 2, "before"),
            event("task_started", 3, "after"), sample(12, 4),
        ])
        _, data = self.analyze()
        self.assertEqual(data["turns"][1]["status"], "active")
        for turn in data["turns"]:
            self.assertIsNone(turn["windows"][0]["observedChangePercentagePoints"])
            self.assertEqual(turn["windows"][0]["changeUnavailableReason"], "insufficient_snapshots")

    def test_ambiguous_overlap_is_unassigned_but_explicit_ids_are_preserved(self):
        self.write([
            event("task_started", 0, "a"), event("task_started", 1, "b"),
            sample(10, 2), sample(11, 3),
            sample(12, 4, "a"), sample(14, 5, "a"),
            event("task_complete", 6, "a"),
            sample(15, 7), sample(16, 8), event("task_complete", 9, "b"),
            sample(18, 10),
        ])
        _, data = self.analyze()
        a, b = data["turns"]
        self.assertEqual(data["unassignedTokenCountEvents"], 3)
        self.assertEqual(a["associationSources"], ["explicit_turn_id"])
        self.assertEqual(a["windows"][0]["observedChangePercentagePoints"], 2)
        self.assertEqual(b["windows"][0]["observedChangePercentagePoints"], 1)

    def test_missing_invalid_and_zero_quota_remain_distinct(self):
        self.write([
            event("task_started", 0, "missing"), event("token_count", 1, rate_limits=None),
            event("task_complete", 2, "missing"), event("task_started", 3, "invalid"),
            sample(True, 4), event("task_complete", 5, "invalid"),
            event("task_started", 6, "zero"), sample(0, 7), sample(0, 8),
        ])
        _, data = self.analyze()
        missing, invalid, zero = data["turns"]
        self.assertFalse(missing["available"])
        self.assertEqual(missing["missingRateLimitSnapshots"], 1)
        self.assertFalse(invalid["available"])
        self.assertEqual(invalid["invalidWindows"], 1)
        self.assertTrue(zero["available"])
        self.assertEqual(zero["windows"][0]["observedChangePercentagePoints"], 0)
        self.assertIsNone(zero["attributableUsagePercentagePoints"])

    def test_small_recorded_change_is_not_rounded_down_to_zero(self):
        self.write([event("task_started", 0, "t"), sample(0, 1), sample(1e-11, 2)])
        _, data = self.analyze()
        self.assertEqual(data["turns"][0]["windows"][0]["observedChangePercentagePoints"], 1e-11)

    def test_malformed_explicit_turn_id_never_falls_back_to_an_active_turn(self):
        rows = [event("task_started", 0, "valid")]
        for second, invalid in enumerate((123, [], "", False), 1):
            row = sample(40 + second, second)
            row["payload"]["turn_id"] = invalid
            rows.append(row)
        self.write(rows)
        _, data = self.analyze()
        self.assertEqual(data["unassignedTokenCountEvents"], 4)
        self.assertFalse(data["turns"][0]["available"])
        self.assertEqual(data["turns"][0]["tokenCountEvents"], 0)

    def test_reset_plan_limit_duration_and_bucket_changes_are_not_subtracted(self):
        for changed in ({"reset": 2_000_000_001}, {"plan": "plus"}, {"limit": "other"},
                        {"minutes": 300}, {"bucket": "secondary"}):
            with self.subTest(changed=changed):
                self.write([event("task_started", 0, "t"), sample(40, 1),
                            sample(42, 2, **changed)])
                _, data = self.analyze()
                self.assertEqual(data["turns"][0]["windowCount"], 2)
                self.assertTrue(all(row["observedChangePercentagePoints"] is None
                                    for row in data["turns"][0]["windows"]))

    def test_weekly_and_five_hour_windows_are_separate_and_not_summed(self):
        rows = [event("task_started", 0, "t")]
        for second, weekly, short in ((1, 40, 5), (2, 42, 9)):
            limits = quota(weekly, bucket="secondary")
            limits["primary"] = quota(short, minutes=300)["primary"]
            rows.append(event("token_count", second, rate_limits=limits))
        self.write(rows)
        _, data = self.analyze()
        short, weekly = data["turns"][0]["windows"]
        self.assertEqual((short["windowLabel"], short["observedChangePercentagePoints"]), ("5-hour", 4))
        self.assertEqual((weekly["windowLabel"], weekly["observedChangePercentagePoints"]), ("weekly", 2))
        self.assertIsNone(data["turns"][0]["attributableUsagePercentagePoints"])

    def test_decreasing_expired_unknown_and_out_of_order_evidence_is_unavailable(self):
        cases = [
            ([sample(42, 1), sample(40, 2)], "usage_decreased_within_window"),
            ([sample(40, 2), sample(42, 1)], "invalid_out_of_order_or_expired_timestamp"),
            ([sample(40, 1, reset=1), sample(42, 2, reset=1)],
             "invalid_out_of_order_or_expired_timestamp"),
            ([sample(40, 1, reset=None), sample(42, 2, reset=None)], "window_identity_unavailable"),
        ]
        for samples, reason in cases:
            with self.subTest(reason=reason):
                self.write([event("task_started", 0, "t"), *samples])
                _, data = self.analyze()
                window = data["turns"][0]["windows"][0]
                self.assertIsNone(window["observedChangePercentagePoints"])
                self.assertEqual(window["changeUnavailableReason"], reason)

    def test_parse_errors_invalidate_turn_changes_and_compression_preserves_details(self):
        rows = [event("task_started", 0, "t"), sample(40, 1), sample(42, 2)]
        self.write(rows)
        _, plain = self.analyze()
        packed = self.write(rows, path=self.root / "compressed.jsonl.zst")
        _, compressed = self.analyze(packed)
        self.assertEqual(compressed["turns"][0]["windows"], [
            {**plain["turns"][0]["windows"][0], "file": str(packed)}])
        self.write(rows, tail="{broken\n")
        _, broken = self.analyze()
        self.assertEqual(broken["parseErrorCount"], 1)
        self.assertEqual(broken["turns"][0]["windows"][0]["changeUnavailableReason"],
                         "incomplete_session_coverage")

    def test_same_turn_id_in_different_sessions_does_not_merge(self):
        for name, before, after in (("one", 10, 12), ("two", 40, 41)):
            self.write([event("task_started", 0, "same"), sample(before, 1), sample(after, 2)],
                       path=self.root / f"{name}.jsonl")
        _, data = self.analyze(self.root)
        self.assertEqual(data["turnCount"], 2)
        by_session = {turn["sessionId"]: turn["windows"][0]["observedChangePercentagePoints"]
                      for turn in data["turns"]}
        self.assertEqual(by_session, {"one": 2, "two": 1})

    def test_response_payloads_cannot_forge_quota_or_turn_association(self):
        fake_start = event("task_started", 0, "fake")
        fake_start["type"] = "response_item"
        fake_usage = sample(99, 2, "fake")
        fake_usage["type"] = "response_item"
        self.write([fake_start, sample(40, 1), fake_usage, sample(42, 3)])
        _, data = self.analyze()
        self.assertEqual(data["turnCount"], 0)
        self.assertEqual(data["tokenCountEvents"], 2)
        self.assertEqual(data["unassignedTokenCountEvents"], 2)

    def test_full_text_summary_cache_and_token_opt_out_keep_turn_detail(self):
        rows = []
        for i in range(audit._MAX_SUMMARY_TURNS + 2):
            rows.extend([event("task_started", i * 4, f"t{i}"),
                         sample(40, i * 4 + 1), sample(42, i * 4 + 2),
                         event("task_complete", i * 4 + 3, f"t{i}")])
        self.write(rows)
        report, data = self.analyze(include_tokens=False, cache_dir=self.root / "cache")
        _, cached = self.analyze(include_tokens=False, cache_dir=self.root / "cache")
        self.assertEqual(cached, data)
        text = audit.render_report(report)
        self.assertIn("2% of weekly allowance (observed account change)", text)
        self.assertIn("additional subscription turns in JSON output", text)
        original = copy.deepcopy(report)
        summary = audit.bounded_summary(report)["subscriptionUsage"]
        self.assertEqual(summary["omittedTurns"] + len(summary["turns"]), data["turnCount"])
        with mock.patch.object(audit, "_MAX_SUMMARY_BYTES", 1):
            tiny = audit.bounded_summary(report)["subscriptionUsage"]
        self.assertEqual(tiny["turns"], [])
        self.assertEqual(tiny["omittedTurns"], data["turnCount"])
        self.assertEqual(report, original)

if __name__ == "__main__":
    unittest.main()
