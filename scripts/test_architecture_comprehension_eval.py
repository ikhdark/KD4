from pathlib import Path
import sys
import unittest

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts import architecture_comprehension_eval  # noqa: E402
from scripts import source_owners  # noqa: E402


class ArchitectureComprehensionEvalTest(unittest.TestCase):
    def test_repository_cases_have_complete_bounded_relationship_recall(self) -> None:
        report = architecture_comprehension_eval.evaluate()

        self.assertEqual(report["status"], "passed")
        self.assertEqual(report["summary"]["relationship_recall"], 1.0)
        self.assertEqual(report["summary"]["ranked_relationship_recall"], 1.0)
        self.assertEqual(report["summary"]["late_relationship_discoveries"], 0)
        self.assertGreater(report["summary"]["reading_reduction_ratio"], 0.0)
        self.assertTrue(
            all(case["relationship_count"] <= 32 for case in report["cases"])
        )
        self.assertLess(
            report["summary"]["ranked_noise_relationships"],
            report["summary"]["noise_relationships"],
        )
        self.assertGreater(report["summary"]["ranked_noise_reduction_ratio"], 0.75)

    def test_missing_relationship_is_insufficient_not_noise(self) -> None:
        manifest, digest = source_owners.load_and_validate(
            source_owners.DEFAULT_MANIFEST, source_owners.REPO_ROOT
        )
        case = {
            "id": "missing",
            "description": "Missing material relationship",
            "owners": ["source-owner-index"],
            "expected": [
                {
                    "facet": "generated_artifacts",
                    "target_contains": "does-not-exist",
                }
            ],
        }

        result = architecture_comprehension_eval.evaluate_case(
            case, manifest, digest, source_owners.REPO_ROOT
        )

        self.assertEqual(result["classification"], "insufficient")
        self.assertEqual(result["late_relationship_discoveries"], 1)
        self.assertGreater(result["noise_relationships"], 0)
        self.assertTrue(result["ranking_failures"])


if __name__ == "__main__":
    unittest.main()
