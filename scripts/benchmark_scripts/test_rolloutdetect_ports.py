"""Validation-ledger regression retained with its KD4 producer."""
import unittest


class RolloutDetectPortsTest(unittest.TestCase):
    def test_missing_proof_status_preserves_work_without_claiming_current_proof(self):
        from scripts.build import validation_metrics as metrics
        from scripts.build.test_validation_metrics import record

        row = record()
        row.update(input_coverage="complete", input_digest="digest")
        row["proof"].update(freshness="current", freshness_basis={"source": "fixture"}, coverage="verified")
        row["proof"].pop("status")
        report = metrics.summarize([row])
        self.assertEqual(report["launches"], 1)
        self.assertEqual(report["launches_producing_current_proof"], 0)


if __name__ == "__main__":
    unittest.main()
