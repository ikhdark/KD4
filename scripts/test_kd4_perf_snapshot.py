from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import kd4_perf_snapshot


class Kd4PerfSnapshotTest(unittest.TestCase):
    def test_percentile_interpolates_sorted_values(self) -> None:
        self.assertEqual(kd4_perf_snapshot.percentile([4.0, 1.0, 3.0, 2.0], 0.5), 2.5)
        self.assertAlmostEqual(
            kd4_perf_snapshot.percentile([1.0, 2.0, 3.0, 4.0], 0.95),
            3.85,
        )

    def test_successful_scenario_records_cold_and_warm_samples(self) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            name="fixture",
            command=(sys.executable, "-c", "print('ok')"),
            cwd=Path.cwd(),
            default_iterations=3,
            category="test",
        )

        result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.status, "passed")
        self.assertEqual(len(result.samples), 3)
        self.assertIsNotNone(result.cold_ms)
        self.assertIsNotNone(result.warm_p50_ms)
        self.assertGreater(result.samples[0].stdout_bytes, 0)

    def test_missing_executable_is_skipped(self) -> None:
        scenario = kd4_perf_snapshot.Scenario(
            name="missing",
            command=("definitely-not-a-kd4-command",),
            cwd=Path.cwd(),
            default_iterations=1,
            category="test",
        )

        result = kd4_perf_snapshot.measure_scenario(scenario)

        self.assertEqual(result.status, "skipped")
        self.assertTrue(result.passed)

    def test_phase0_profile_covers_required_baseline_categories(self) -> None:
        catalog = kd4_perf_snapshot.scenario_catalog()
        categories = {
            catalog[name].category
            for name in kd4_perf_snapshot.PROFILE_SCENARIOS["phase0"]
        }

        self.assertTrue(
            {
                "startup",
                "repository",
                "validation",
                "test",
                "build",
                "app-server",
                "desktop-publish",
            }
            <= categories
        )

    def test_atomic_json_writer_replaces_target(self) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"
            kd4_perf_snapshot.write_json_atomic(target, {"ok": True})

            self.assertEqual(
                json.loads(target.read_text(encoding="utf-8")), {"ok": True}
            )

    def test_atomic_json_writer_removes_temporary_file_on_serialization_error(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tempdir:
            target = Path(tempdir) / "snapshot.json"

            with self.assertRaises(TypeError):
                kd4_perf_snapshot.write_json_atomic(target, {"bad": object()})

            self.assertEqual(list(Path(tempdir).glob("*.tmp")), [])
            self.assertFalse(target.exists())

    def test_environment_metadata_distinguishes_git_failure_from_clean_tree(
        self,
    ) -> None:
        with mock.patch.object(kd4_perf_snapshot, "_git_text", return_value=None):
            metadata = kd4_perf_snapshot.environment_metadata(
                Path.cwd(), hash_binary=False
            )

        self.assertIsNone(metadata["dirtyPaths"])

    def test_environment_metadata_reports_zero_dirty_paths_for_clean_tree(
        self,
    ) -> None:
        with mock.patch.object(
            kd4_perf_snapshot, "_git_text", side_effect=["", "head", "main"]
        ):
            metadata = kd4_perf_snapshot.environment_metadata(
                Path.cwd(), hash_binary=False
            )

        self.assertEqual(metadata["dirtyPaths"], 0)


if __name__ == "__main__":
    unittest.main()
