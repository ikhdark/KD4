from __future__ import annotations

import copy
import json
import tempfile
import unittest
from pathlib import Path

if __package__:
    from .score_results import FROZEN_EXECUTION
    from .score_results import FROZEN_MODEL_SETTINGS
    from .score_results import FROZEN_REPAIR_EXECUTION
    from .score_results import score
    from .validate_cases import ValidationError
    from .validate_cases import _validate_patch
    from .validate_cases import case_fingerprint
    from .validate_cases import load_cases
    from .validate_cases import validate_cases
else:  # Direct execution from this directory.
    from score_results import FROZEN_EXECUTION
    from score_results import FROZEN_MODEL_SETTINGS
    from score_results import FROZEN_REPAIR_EXECUTION
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
        *,
        candidate_patch: str | None = None,
        tool_actions: list[dict[str, object]] | None = None,
    ) -> None:
        raw_events = [
            {
                "type": "item.completed",
                "item": action,
            }
            for action in (tool_actions or [])
        ]
        raw_events.append(
            {
                "type": "item.completed",
                "item": {
                    "id": "item_final",
                    "type": "agent_message",
                    "text": "No findings.",
                },
            }
        )
        result = {
            "case_id": case["id"],
            "case_fingerprint": case_fingerprint(case),
            "completed_at": "2026-07-29T12:00:00Z",
            "model": {
                **FROZEN_MODEL_SETTINGS,
                "binary_sha256": binary_sha256,
            },
            "execution": (
                FROZEN_REPAIR_EXECUTION
                if "repair_contract" in case
                else FROZEN_EXECUTION
            ),
            "final_output": "No findings.",
            "reported_findings": [],
            "raw_events": raw_events,
        }
        if candidate_patch is not None:
            result["candidate_patch"] = candidate_patch
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

    @staticmethod
    def _repair_case() -> dict[str, object]:
        return next(
            case for case in load_cases() if case["id"] == "repair-minimal-batch"
        )

    @staticmethod
    def _minimal_repair_patch(replacement: str = "while start < len(values):") -> str:
        return f"""\
diff --git a/investigation_cases/repair_minimal_batch.py b/investigation_cases/repair_minimal_batch.py
--- a/investigation_cases/repair_minimal_batch.py
+++ b/investigation_cases/repair_minimal_batch.py
@@ -8,3 +8,3 @@ def batches(values, width):
-    while start + width < len(values):
+    {replacement}
         result.append(values[start : start + width])
         start += width
"""

    @staticmethod
    def _other_minimal_repair_patches() -> dict[str, str]:
        return {
            "repair-advisory-warning": """\
diff --git a/investigation_cases/repair_advisory_warning.py b/investigation_cases/repair_advisory_warning.py
--- a/investigation_cases/repair_advisory_warning.py
+++ b/investigation_cases/repair_advisory_warning.py
@@ -2 +2 @@ def completion_allowed(operation_succeeded, advisory_messages):
-    return operation_succeeded and not advisory_messages
+    return operation_succeeded
""",
            "repair-authoritative-status": """\
diff --git a/investigation_cases/repair_authoritative_status.py b/investigation_cases/repair_authoritative_status.py
--- a/investigation_cases/repair_authoritative_status.py
+++ b/investigation_cases/repair_authoritative_status.py
@@ -2 +2 @@ def command_succeeded(exit_code, stdout, stderr):
-    return "success" in stdout.casefold()
+    return exit_code == 0
""",
            "repair-dependent-failure": """\
diff --git a/investigation_cases/repair_dependent_failure.py b/investigation_cases/repair_dependent_failure.py
--- a/investigation_cases/repair_dependent_failure.py
+++ b/investigation_cases/repair_dependent_failure.py
@@ -2,5 +2,5 @@ def apply_then_consume(producer, consumer):
     try:
         value = producer()
     except RuntimeError:
-        return None
+        raise
     return consumer(value)
""",
            "repair-ready-signal": """\
diff --git a/investigation_cases/repair_ready_signal.py b/investigation_cases/repair_ready_signal.py
--- a/investigation_cases/repair_ready_signal.py
+++ b/investigation_cases/repair_ready_signal.py
@@ -15,2 +15,5 @@ class ReadySignal:
     def on_ready(self, callback):
-        self._callbacks.append(callback)
+        if self._ready:
+            callback()
+            return
+        self._callbacks.append(callback)
""",
            "repair-preserve-context": """\
diff --git a/investigation_cases/repair_preserve_context.py b/investigation_cases/repair_preserve_context.py
--- a/investigation_cases/repair_preserve_context.py
+++ b/investigation_cases/repair_preserve_context.py
@@ -2 +2 @@ def model_visible_messages(messages):
-    return messages[-4:]
+    return messages
""",
        }

    def test_repair_scorer_accepts_minimal_valid_patch(self) -> None:
        case = self._repair_case()
        binary_sha256 = "a" * 64
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(
                results_dir,
                case,
                binary_sha256,
                candidate_patch=self._minimal_repair_patch(),
            )

            report = score([case], results_dir, binary_sha256)

        self.assertEqual(report["repair_cases"], 1)
        self.assertEqual(report["repair_cases_passed"], 1)
        self.assertEqual(report["repair_contract_pass_rate"], 1.0)
        self.assertEqual(report["case_scores"][0]["repair"]["violations"], [])

    def test_each_repair_fixture_accepts_its_smallest_contract_fix(self) -> None:
        cases_by_id = {case["id"]: case for case in load_cases()}
        binary_sha256 = "a" * 64
        for case_id, candidate_patch in self._other_minimal_repair_patches().items():
            with (
                self.subTest(case_id=case_id),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                case = cases_by_id[case_id]
                results_dir = Path(temp_dir)
                self._write_result(
                    results_dir,
                    case,
                    binary_sha256,
                    candidate_patch=candidate_patch,
                )

                report = score([case], results_dir, binary_sha256)

                self.assertEqual(report["case_scores"][0]["repair"]["violations"], [])

    def test_repair_scorer_rejects_patch_that_does_not_fix_contract(self) -> None:
        case = self._repair_case()
        binary_sha256 = "a" * 64
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(
                results_dir,
                case,
                binary_sha256,
                candidate_patch=self._minimal_repair_patch(
                    "while start + width <= len(values):"
                ),
            )

            report = score([case], results_dir, binary_sha256)

        self.assertEqual(
            report["case_scores"][0]["repair"]["violations"],
            ["validation_failed"],
        )

    def test_repair_scorer_rejects_scope_and_change_limit_violations(self) -> None:
        case = copy.deepcopy(self._repair_case())
        case["repair_contract"]["max_changed_lines"] = 1
        binary_sha256 = "a" * 64
        out_of_scope = self._minimal_repair_patch().replace(
            "repair_minimal_batch.py", "test_repair_minimal_batch.py"
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(
                results_dir,
                case,
                binary_sha256,
                candidate_patch=out_of_scope,
            )

            report = score([case], results_dir, binary_sha256)

        violations = report["case_scores"][0]["repair"]["violations"]
        self.assertIn(
            "out_of_scope_paths:investigation_cases/test_repair_minimal_batch.py",
            violations,
        )
        self.assertIn("changed_line_limit:2>1", violations)

    def test_repair_scorer_rejects_unobserved_binary_diff_sections(self) -> None:
        case = self._repair_case()
        binary_sha256 = "a" * 64
        patch = (
            self._minimal_repair_patch()
            + """\
diff --git a/investigation_cases/repair_minimal_batch.py b/investigation_cases/repair_minimal_batch.py
GIT binary patch
literal 1
Ic${Nk000310RR91

literal 1
Ic${Nk000310RR91
"""
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(
                results_dir,
                case,
                binary_sha256,
                candidate_patch=patch,
            )

            report = score([case], results_dir, binary_sha256)

        self.assertEqual(
            report["case_scores"][0]["repair"]["violations"],
            ["invalid_candidate_patch:candidate_patch must not contain binary diffs"],
        )

    def test_repair_scorer_rejects_forbidden_added_mechanisms(self) -> None:
        case = self._repair_case()
        binary_sha256 = "a" * 64
        forbidden = case["repair_contract"]["forbidden_added_text"]
        for fragment in forbidden:
            with (
                self.subTest(fragment=fragment),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                results_dir = Path(temp_dir)
                patch = self._minimal_repair_patch(
                    f"while start < len(values):  # {fragment}"
                )
                self._write_result(
                    results_dir,
                    case,
                    binary_sha256,
                    candidate_patch=patch,
                )

                report = score([case], results_dir, binary_sha256)

                self.assertIn(
                    f"forbidden_added_text:{fragment}",
                    report["case_scores"][0]["repair"]["violations"],
                )

    def test_repair_scorer_rejects_extra_and_repeated_tool_actions(self) -> None:
        case = copy.deepcopy(self._repair_case())
        case["repair_contract"]["max_tool_calls"] = 1
        binary_sha256 = "a" * 64
        actions = [
            {"id": "tool_1", "type": "command_execution", "command": "inspect"},
            {"id": "tool_2", "type": "command_execution", "command": "inspect"},
        ]
        with tempfile.TemporaryDirectory() as temp_dir:
            results_dir = Path(temp_dir)
            self._write_result(
                results_dir,
                case,
                binary_sha256,
                candidate_patch=self._minimal_repair_patch(),
                tool_actions=actions,
            )

            report = score([case], results_dir, binary_sha256)

        violations = report["case_scores"][0]["repair"]["violations"]
        self.assertIn("tool_call_limit:2>1", violations)
        self.assertIn("repeated_equivalent_action_limit:1>0", violations)

    def test_repair_contract_keeps_validation_script_immutable(self) -> None:
        cases = load_cases()
        repair_case = next(
            case for case in cases if case["id"] == "repair-minimal-batch"
        )
        repair_case["repair_contract"]["allowed_paths"].append(
            repair_case["repair_contract"]["validation_script"]
        )

        with self.assertRaisesRegex(
            ValidationError, "validation_script must not be editable"
        ):
            validate_cases(cases)


if __name__ == "__main__":
    unittest.main()
