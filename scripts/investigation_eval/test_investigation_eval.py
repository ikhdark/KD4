from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

if __package__:
    from .score_results import FROZEN_EXECUTION
    from .score_results import FROZEN_MODEL_SETTINGS
    from .score_results import score
    from .validate_cases import ValidationError
    from .validate_cases import _validate_patch
    from .validate_cases import case_fingerprint
    from .validate_cases import load_cases
    from .validate_cases import validate_cases
else:  # Direct execution from this directory.
    from score_results import FROZEN_EXECUTION
    from score_results import FROZEN_MODEL_SETTINGS
    from score_results import score
    from validate_cases import ValidationError
    from validate_cases import _validate_patch
    from validate_cases import case_fingerprint
    from validate_cases import load_cases
    from validate_cases import validate_cases


class InvestigationEvalTests(unittest.TestCase):
    def _write_result(
        self,
        results_dir: Path,
        case: dict[str, object],
        binary_sha256: str,
    ) -> None:
        result = {
            "case_id": case["id"],
            "case_fingerprint": case_fingerprint(case),
            "completed_at": "2026-07-29T12:00:00Z",
            "model": {
                **FROZEN_MODEL_SETTINGS,
                "binary_sha256": binary_sha256,
            },
            "execution": FROZEN_EXECUTION,
            "final_output": "No findings.",
            "reported_findings": [],
            "raw_events": [
                {
                    "type": "item.completed",
                    "item": {
                        "id": "item_0",
                        "type": "agent_message",
                        "text": "No findings.",
                    },
                }
            ],
        }
        (results_dir / f"{case['id']}.json").write_text(
            json.dumps(result),
            encoding="utf-8",
        )

    def test_committed_corpus_is_history_independent(self) -> None:
        cases = load_cases()

        validate_cases(cases)

        self.assertTrue(all("base_commit" not in case for case in cases))
        self.assertTrue(all(len(case_fingerprint(case)) == 64 for case in cases))

    def test_patch_validation_treats_added_double_plus_as_content(self) -> None:
        patch_text = """\
diff --git a/investigation_cases/plus.txt b/investigation_cases/plus.txt
new file mode 100644
--- /dev/null
+++ b/investigation_cases/plus.txt
@@ -0,0 +1 @@
+++value
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            patch = Path(temp_dir) / "plus.patch"
            patch.write_text(patch_text, encoding="utf-8", newline="\n")

            _validate_patch(patch, case_id="double-plus-content")

    def test_patch_validation_normalizes_crlf_before_git_apply(self) -> None:
        patch_text = """\
diff --git a/investigation_cases/crlf.txt b/investigation_cases/crlf.txt
new file mode 100644
--- /dev/null
+++ b/investigation_cases/crlf.txt
@@ -0,0 +1 @@
+value
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            patch = Path(temp_dir) / "crlf.patch"
            patch.write_bytes(patch_text.replace("\n", "\r\n").encode("utf-8"))

            _validate_patch(patch, case_id="crlf-content")

    def test_scorer_requires_independently_hashed_binary(self) -> None:
        case = load_cases()[0]
        binary_sha256 = "a" * 64
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(results_dir, case, binary_sha256)

            report = score([case], results_dir, binary_sha256)

        self.assertEqual(report["binary_sha256"], binary_sha256)

    def test_scorer_rejects_mixed_binary_hashes(self) -> None:
        cases = load_cases()[:2]
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(results_dir, cases[0], "a" * 64)
            self._write_result(results_dir, cases[1], "b" * 64)

            with self.assertRaisesRegex(
                ValidationError,
                "binary_sha256 does not match the hashed benchmark binary",
            ):
                score(cases, results_dir, "a" * 64)


if __name__ == "__main__":
    unittest.main()
