from __future__ import annotations

import copy
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

if __package__:
    from .score_results import (
        FROZEN_EXECUTION,
        FROZEN_MODEL_SETTINGS,
        FROZEN_REPAIR_EXECUTION,
    )
    from .validate_cases import (
        case_fingerprint,
        load_cases,
    )
else:  # Direct execution from this directory.
    from score_results import (
        FROZEN_EXECUTION,
        FROZEN_MODEL_SETTINGS,
        FROZEN_REPAIR_EXECUTION,
    )
    from validate_cases import (
        case_fingerprint,
        load_cases,
    )

REPO_ROOT = Path(__file__).resolve().parents[2]
EVAL_DIR = REPO_ROOT / "scripts" / "investigation_eval"
SCORE_RESULTS_CLI = EVAL_DIR / "score_results.py"
VALIDATE_CASES_CLI = EVAL_DIR / "validate_cases.py"


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

    def _run_score_cli(
        self,
        cases: list[dict[str, object]],
        *,
        candidate_patches: dict[str, str] | None = None,
        tool_actions: dict[str, list[dict[str, object]]] | None = None,
        binary_hash_overrides: dict[str, str] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        patches = self._all_minimal_repair_patches()
        patches.update(candidate_patches or {})
        tool_actions = tool_actions or {}
        binary_hash_overrides = binary_hash_overrides or {}
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            cases_path = root / "cases.jsonl"
            cases_path.write_text(
                "".join(
                    f"{json.dumps(case, separators=(',', ':'))}\n" for case in cases
                ),
                encoding="utf-8",
                newline="\n",
            )
            results_dir = root / "results"
            results_dir.mkdir()
            binary = root / "codex-under-test.exe"
            binary.write_bytes(b"kd4 investigation evaluation binary\n")
            binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
            for case in cases:
                case_id = str(case["id"])
                self._write_result(
                    results_dir,
                    case,
                    binary_hash_overrides.get(case_id, binary_sha256),
                    candidate_patch=(
                        patches[case_id] if "repair_contract" in case else None
                    ),
                    tool_actions=tool_actions.get(case_id),
                )
            result = subprocess.run(
                [
                    sys.executable,
                    str(SCORE_RESULTS_CLI),
                    "--results",
                    str(results_dir),
                    "--cases",
                    str(cases_path),
                    "--binary",
                    str(binary),
                ],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
        return result, binary_sha256

    def _assert_score_cli_passed(
        self, result: subprocess.CompletedProcess[str]
    ) -> dict[str, object]:
        self.assertEqual(
            result.returncode,
            0,
            msg=f"scorer CLI failed: {result.stderr or result.stdout}",
        )
        self.assertEqual(result.stderr, "")
        report = json.loads(result.stdout)
        self.assertIsInstance(report, dict)
        return report

    def _assert_validator_copy_passes(self, mutate_patch: str) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            copied_eval_dir = Path(temp_dir) / "scripts" / "investigation_eval"
            shutil.copytree(EVAL_DIR, copied_eval_dir)
            patch = copied_eval_dir / "patches" / "clean-control-batching.patch"
            if mutate_patch == "crlf":
                patch.write_bytes(
                    patch.read_text(encoding="utf-8").replace("\n", "\r\n").encode()
                )
            elif mutate_patch == "double-plus":
                patch_text = patch.read_text(encoding="utf-8")
                patch_text = patch_text.replace("@@ -0,0 +1,15 @@", "@@ -0,0 +1,16 @@")
                patch.write_text(
                    f"{patch_text.rstrip()}\n+++value\n",
                    encoding="utf-8",
                    newline="\n",
                )
            else:
                self.fail(f"unknown fixture mutation: {mutate_patch}")
            result = subprocess.run(
                [sys.executable, str(copied_eval_dir / "validate_cases.py")],
                cwd=Path(temp_dir),
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(
            result.returncode,
            0,
            msg=f"validator CLI rejected {mutate_patch}: {result.stderr or result.stdout}",
        )
        self.assertIn("validated 13 investigation cases", result.stdout)
        self.assertEqual(result.stderr, "")

    def test_validator_cli_accepts_crlf_and_added_double_plus_content(self) -> None:
        self._assert_validator_copy_passes("crlf")
        self._assert_validator_copy_passes("double-plus")

    def _assert_corpus_cli_validates_case(self, case_id: str) -> None:
        result = subprocess.run(
            [sys.executable, str(VALIDATE_CASES_CLI), "--show-fingerprints"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            result.returncode,
            0,
            msg=f"validator CLI failed for {case_id}: {result.stderr or result.stdout}",
        )
        self.assertEqual(result.stderr, "")
        output_lines = result.stdout.splitlines()
        self.assertTrue(
            output_lines
            and output_lines[0].startswith("validated 13 investigation cases (")
        )
        fingerprints = {}
        for line in output_lines[1:]:
            reported_case_id, fingerprint = line.split(maxsplit=1)
            self.assertNotIn(reported_case_id, fingerprints)
            fingerprints[reported_case_id] = fingerprint
        self.assertEqual(len(fingerprints), 13)
        self.assertIn(case_id, fingerprints)
        self.assertRegex(fingerprints[case_id], r"^[0-9a-f]{64}$")

    def test_corpus_cli_validates_clean_control_dispatch(self) -> None:
        self._assert_corpus_cli_validates_case("clean-control-dispatch")

    def test_corpus_cli_validates_clean_control_batching(self) -> None:
        self._assert_corpus_cli_validates_case("clean-control-batching")

    def test_corpus_cli_validates_static_wiring_missing_handler(self) -> None:
        self._assert_corpus_cli_validates_case("static-wiring-missing-handler")

    def test_corpus_cli_validates_lifecycle_cancelled_child_leak(self) -> None:
        self._assert_corpus_cli_validates_case("lifecycle-cancelled-child-leak")

    def test_corpus_cli_validates_persistence_receipt_sequence_collision(self) -> None:
        self._assert_corpus_cli_validates_case("persistence-receipt-sequence-collision")

    def test_corpus_cli_validates_protocol_sandbox_mode_swap(self) -> None:
        self._assert_corpus_cli_validates_case("protocol-sandbox-mode-swap")

    def test_corpus_cli_validates_local_logic_last_batch_dropped(self) -> None:
        self._assert_corpus_cli_validates_case("local-logic-last-batch-dropped")

    def test_corpus_cli_validates_repair_minimal_batch(self) -> None:
        self._assert_corpus_cli_validates_case("repair-minimal-batch")

    def test_corpus_cli_validates_repair_advisory_warning(self) -> None:
        self._assert_corpus_cli_validates_case("repair-advisory-warning")

    def test_corpus_cli_validates_repair_authoritative_status(self) -> None:
        self._assert_corpus_cli_validates_case("repair-authoritative-status")

    def test_corpus_cli_validates_repair_dependent_failure(self) -> None:
        self._assert_corpus_cli_validates_case("repair-dependent-failure")

    def test_corpus_cli_validates_repair_ready_signal(self) -> None:
        self._assert_corpus_cli_validates_case("repair-ready-signal")

    def test_corpus_cli_validates_repair_preserve_context(self) -> None:
        self._assert_corpus_cli_validates_case("repair-preserve-context")

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

    def _all_minimal_repair_patches(self) -> dict[str, str]:
        return {
            "repair-minimal-batch": self._minimal_repair_patch(),
            **self._other_minimal_repair_patches(),
        }

    def test_score_cli_hashes_binary_and_rejects_mixed_result_hashes(self) -> None:
        cases = load_cases()
        result, binary_sha256 = self._run_score_cli(cases)
        report = self._assert_score_cli_passed(result)
        self.assertEqual(report["binary_sha256"], binary_sha256)

        result, _ = self._run_score_cli(
            cases,
            binary_hash_overrides={str(cases[0]["id"]): "a" * 64},
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn(
            "binary_sha256 does not match the hashed benchmark binary",
            result.stderr,
        )
        self.assertEqual(result.stdout, "")

    def test_score_cli_accepts_every_smallest_repair(self) -> None:
        result, _ = self._run_score_cli(load_cases())
        report = self._assert_score_cli_passed(result)
        repairs = {
            score["id"]: score["repair"]
            for score in report["case_scores"]
            if "repair" in score
        }
        self.assertEqual(set(repairs), set(self._all_minimal_repair_patches()))
        self.assertTrue(all(repair["violations"] == [] for repair in repairs.values()))
        self.assertEqual(report["repair_cases_passed"], 6)
        self.assertEqual(report["repair_contract_pass_rate"], 1.0)

    def test_score_cli_reports_patch_that_does_not_fix_contract(self) -> None:
        result, _ = self._run_score_cli(
            load_cases(),
            candidate_patches={
                "repair-minimal-batch": self._minimal_repair_patch(
                    "while start + width <= len(values):"
                )
            },
        )
        report = self._assert_score_cli_passed(result)
        repair = next(
            score["repair"]
            for score in report["case_scores"]
            if score["id"] == "repair-minimal-batch"
        )
        self.assertEqual(repair["violations"], ["validation_failed"])

    def test_score_cli_reports_scope_and_change_limit_violations(self) -> None:
        cases = copy.deepcopy(load_cases())
        case = next(case for case in cases if case["id"] == "repair-minimal-batch")
        case["repair_contract"]["max_changed_lines"] = 1
        out_of_scope = self._minimal_repair_patch().replace(
            "repair_minimal_batch.py", "test_repair_minimal_batch.py"
        )
        result, _ = self._run_score_cli(
            cases,
            candidate_patches={"repair-minimal-batch": out_of_scope},
        )
        report = self._assert_score_cli_passed(result)
        repair = next(
            score["repair"]
            for score in report["case_scores"]
            if score["id"] == "repair-minimal-batch"
        )
        self.assertIn(
            "out_of_scope_paths:investigation_cases/test_repair_minimal_batch.py",
            repair["violations"],
        )
        self.assertIn("changed_line_limit:2>1", repair["violations"])

    def test_score_cli_reports_every_forbidden_added_mechanism(self) -> None:
        cases = load_cases()
        case = next(case for case in cases if case["id"] == "repair-minimal-batch")
        forbidden = case["repair_contract"]["forbidden_added_text"]
        result, _ = self._run_score_cli(
            cases,
            candidate_patches={
                "repair-minimal-batch": self._minimal_repair_patch(
                    f"while start < len(values):  # {' | '.join(forbidden)}"
                )
            },
        )
        report = self._assert_score_cli_passed(result)
        repair = next(
            score["repair"]
            for score in report["case_scores"]
            if score["id"] == "repair-minimal-batch"
        )
        self.assertTrue(
            all(
                f"forbidden_added_text:{fragment}" in repair["violations"]
                for fragment in forbidden
            )
        )

    def test_score_cli_reports_tool_action_limits(self) -> None:
        cases = copy.deepcopy(load_cases())
        case = next(case for case in cases if case["id"] == "repair-minimal-batch")
        case["repair_contract"]["max_tool_calls"] = 1
        actions = [
            {"id": "tool_1", "type": "command_execution", "command": "inspect"},
            {"id": "tool_2", "type": "command_execution", "command": "inspect"},
        ]
        result, _ = self._run_score_cli(
            cases,
            tool_actions={"repair-minimal-batch": actions},
        )
        report = self._assert_score_cli_passed(result)
        repair = next(
            score["repair"]
            for score in report["case_scores"]
            if score["id"] == "repair-minimal-batch"
        )
        self.assertIn("tool_call_limit:2>1", repair["violations"])
        self.assertIn("repeated_equivalent_action_limit:1>0", repair["violations"])

    def test_score_cli_rejects_unobserved_binary_diff_sections(self) -> None:
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
        result, _ = self._run_score_cli(
            load_cases(),
            candidate_patches={"repair-minimal-batch": patch},
        )
        report = self._assert_score_cli_passed(result)
        repair = next(
            score["repair"]
            for score in report["case_scores"]
            if score["id"] == "repair-minimal-batch"
        )
        self.assertEqual(
            repair["violations"],
            ["invalid_candidate_patch:candidate_patch must not contain binary diffs"],
        )

    def test_score_cli_rejects_editable_validation_script(self) -> None:
        cases = copy.deepcopy(load_cases())
        case = next(case for case in cases if case["id"] == "repair-minimal-batch")
        case["repair_contract"]["allowed_paths"].append(
            case["repair_contract"]["validation_script"]
        )
        result, _ = self._run_score_cli(cases)
        self.assertEqual(result.returncode, 1)
        self.assertIn("validation_script must not be editable", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
