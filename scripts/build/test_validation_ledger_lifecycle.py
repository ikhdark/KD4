"""Real owner-lock, crash, and conservative ledger-maintenance regressions."""
from __future__ import annotations

import contextlib
import gc
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

from scripts.build import validation_metrics as metrics


class LedgerLifecycleTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.logs = self.root / "test-runner-logs"

    def ledger(self, outcome="passed", started="2026-01-01T00:00:00+00:00"):
        value = metrics.ValidationMetrics(["fixture"])
        value.record["started_at"] = started
        value.bind(self.root)
        self.addCleanup(value._release_owner)
        if outcome != "running":
            with contextlib.redirect_stderr(io.StringIO()):
                value.finish(outcome)
        return value

    def test_real_dead_owner_reconciles_without_claiming_exit_or_cleanup(self):
        program = """
import pathlib, sys, time
from scripts.build.validation_metrics import ValidationMetrics
root = pathlib.Path(sys.argv[1])
metrics = ValidationMetrics(['crash-fixture'])
metrics.bind(root)
metrics.command(['fixture-child'], 'build/test', 1)
(root / 'ready').write_text(str(metrics.path), encoding='utf-8')
time.sleep(60)
"""
        process = subprocess.Popen(
            [sys.executable, "-B", "-c", program, str(self.root)],
            cwd=Path(__file__).resolve().parents[2],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
        try:
            deadline = time.monotonic() + 10
            while not (self.root / "ready").exists():
                if process.poll() is not None:
                    self.fail(repr(process.communicate(timeout=1)))
                if time.monotonic() >= deadline:
                    self.fail("owner did not publish its ready barrier")
                time.sleep(.01)
            path = Path((self.root / "ready").read_text(encoding="utf-8"))
            original = path.read_bytes()
            report = metrics.maintain_ledgers(self.logs, apply=True)
            self.assertEqual(report["active"], 1)
            self.assertEqual(report["abandoned"], [])
            self.assertEqual(path.read_bytes(), original)
            process.kill()
            process.communicate(timeout=5)
            preview = metrics.maintain_ledgers(self.logs)
            self.assertEqual(preview["abandoned"], [str(path)])
            self.assertEqual(path.read_bytes(), original, "dry-run changed a ledger")
            report = metrics.maintain_ledgers(self.logs, apply=True)
            self.assertEqual(report["errors"], [])
            row = json.loads(path.read_text())
            self.assertEqual(row["outcome"], "abandoned")
            self.assertEqual(row["commands"][0]["outcome"], "abandoned")
            self.assertIsNone(row["commands"][0]["exit_code"])
            self.assertIsNone(row["finished_at"])
            self.assertIsNone(row["lifecycle_seconds"])
            self.assertEqual(row["proof"]["status"], "unknown")
            self.assertEqual(row["reconciliation"]["process_exit_and_cleanup"], "unknown")
            self.assertEqual(metrics.maintain_ledgers(self.logs, apply=True)["abandoned"], [])
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=5)

    def test_retention_keeps_live_recent_failed_and_legacy_records(self):
        old = self.ledger()
        recent = self.ledger(started="2026-02-01T00:00:00+00:00")
        failed = self.ledger("failed")
        live = self.ledger("running")
        legacy = self.ledger("running")
        legacy._release_owner()
        legacy.record.pop("owner_lock_version")
        legacy.checkpoint()
        malformed = self.logs / "validation-malformed.json"
        malformed.write_text("{not json", encoding="utf-8")
        # The explicit cutoff authorizes expiry; keep=1 independently protects
        # the newest record even when that cutoff would otherwise include it.
        report = metrics.maintain_ledgers(
            self.logs, apply=True, prune_before="2100-01-01T00:00:00+00:00", keep=1,
        )
        self.assertEqual(report["pruned"], [str(old.path)])
        self.assertEqual(report["active"], 1)
        self.assertEqual(report["unowned"], 1)
        self.assertEqual(len(report["errors"]), 1)
        self.assertFalse(old.path.exists())
        for path in (recent.path, failed.path, live.path, legacy.path, malformed):
            self.assertTrue(path.exists(), str(path))

    def test_checkpoint_failure_releases_owner_and_preserves_inline_outcome(self):
        value = self.ledger("running")
        owner = value._owner
        with mock.patch.object(metrics, "write_json_atomic", side_effect=OSError("slow disk failed")):
            with contextlib.redirect_stderr(io.StringIO()) as stderr:
                value.finish("cancelled")
        self.assertTrue(owner.closed)
        self.assertIsNone(value.path)
        self.assertIn('"outcome": "cancelled"', stderr.getvalue())
        self.assertEqual(metrics.maintain_ledgers(self.logs)["active"], 0)

    def test_cli_maintenance_defaults_to_preview_and_validates_retention(self):
        value = self.ledger("running")
        value._release_owner()
        before = value.path.read_bytes()
        with contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(metrics.main(["--maintain", str(self.logs)]), 0)
        self.assertFalse(json.loads(stdout.getvalue())["apply"])
        self.assertEqual(value.path.read_bytes(), before)
        for extra in (["--keep", "0"], ["--prune-before", "2026-01-01"]):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                metrics.main(["--maintain", str(self.logs), *extra])

    def test_discarded_observation_releases_its_handle_without_claiming_success(self):
        value = metrics.ValidationMetrics(["discarded"])
        value.bind(self.root)
        path, owner = value.path, value._owner
        del value
        gc.collect()
        self.assertTrue(owner.closed)
        self.assertEqual(json.loads(path.read_text())["outcome"], "running")
        self.assertEqual(metrics.maintain_ledgers(self.logs)["abandoned"], [str(path)])


if __name__ == "__main__":
    unittest.main()
