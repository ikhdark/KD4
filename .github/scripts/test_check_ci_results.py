from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("check_ci_results.py")
REPO = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("check_ci_results", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


DECISION = "sha256:" + "a" * 64
JOBS = list(MODULE.JOB_ENV)


def needs(expected: dict[str, bool]) -> dict[str, dict[str, object]]:
    result: dict[str, dict[str, object]] = {
        "classify": {
            "result": "success",
            "outputs": {"decision_id": DECISION},
        }
    }
    for job in JOBS:
        if expected[job]:
            result[job] = {
                "result": "success",
                "outputs": {"consumed_decision_id": DECISION},
            }
        else:
            result[job] = {"result": "skipped", "outputs": {}}
    return result


class CheckCiResultsTest(unittest.TestCase):
    def test_expected_runs_and_skips_pass(self) -> None:
        expected = {job: index % 2 == 0 for index, job in enumerate(JOBS)}
        self.assertEqual(MODULE.validate(needs(expected), DECISION, expected), [])

    def test_unexpected_skip_failure_and_cancellation_fail(self) -> None:
        expected = {job: True for job in JOBS}
        data = needs(expected)
        data["rust-ci"]["result"] = "skipped"
        data["sdk"]["result"] = "failure"
        data["cargo-full"]["result"] = "cancelled"
        failures = MODULE.validate(data, DECISION, expected)
        self.assertTrue(any("rust-ci" in failure for failure in failures))
        self.assertTrue(any("sdk" in failure for failure in failures))
        self.assertTrue(any("cargo-full" in failure for failure in failures))

    def test_stale_missing_hash_and_classifier_failure_fail(self) -> None:
        expected = {job: True for job in JOBS}
        data = needs(expected)
        data["classify"]["result"] = "failure"
        data["rust-ci"]["outputs"] = {"consumed_decision_id": "sha256:stale"}
        data["sdk"]["outputs"] = {}
        failures = MODULE.validate(data, DECISION, expected)
        self.assertTrue(any("classify" in failure for failure in failures))
        self.assertTrue(any("rust-ci" in failure for failure in failures))
        self.assertTrue(any("sdk" in failure for failure in failures))

    def test_unexpected_execution_of_expected_skip_fails(self) -> None:
        expected = {job: False for job in JOBS}
        data = needs(expected)
        data["codespell"] = {
            "result": "success",
            "outputs": {"consumed_decision_id": DECISION},
        }
        failures = MODULE.validate(data, DECISION, expected)
        self.assertTrue(any("codespell" in failure for failure in failures))

    def test_blocking_ci_static_terminal_gate_contract(self) -> None:
        workflow = (REPO / ".github/workflows/blocking-ci.yml").read_text()
        self.assertIn("if: ${{ always() }}", workflow)
        for job in [
            "classify",
            "blob-size-policy",
            "cargo-deny",
            "cargo-full",
            "codespell",
            "repo-checks",
            "rust-ci",
            "sdk",
        ]:
            self.assertIn(f"      - {job}", workflow)
        self.assertIn("python3 .github/scripts/check_ci_results.py", workflow)

    def test_reusable_workflows_consume_exact_decision_artifact(self) -> None:
        workflows = {
            "blob-size-policy.yml": "blob-size-policy",
            "cargo-deny.yml": "cargo-deny",
            "codespell.yml": "codespell",
            "repo-checks.yml": "repo-checks",
            "sdk.yml": "sdk",
        }
        for filename, workflow_id in workflows.items():
            with self.subTest(filename=filename):
                text = (REPO / f".github/workflows/{filename}").read_text()
                self.assertIn("decision_id:", text)
                self.assertIn("decision_artifact:", text)
                self.assertIn("consumed_decision_id", text)
                self.assertIn("actions/download-artifact@", text)
                self.assertIn("python3 .github/scripts/verify_ci_decision.py", text)
                self.assertIn(f"--workflow {workflow_id}", text)

    def test_rust_ci_consumes_decision_instead_of_git_text_diff(self) -> None:
        text = (REPO / ".github/workflows/rust-ci.yml").read_text()
        self.assertNotIn("git diff --name-only", text)
        self.assertIn("actions/download-artifact@", text)
        self.assertIn("--workflow rust-ci", text)
        self.assertIn("value: ${{ jobs.results.outputs.consumed_decision_id }}", text)
        self.assertIn("consumed_decision_id: ${{ needs.changed.outputs.consumed_decision_id }}", text)
        self.assertIn("needs.changed.result", text)

    def test_rust_ci_full_has_aggregate_decision_result(self) -> None:
        text = (REPO / ".github/workflows/rust-ci-full.yml").read_text()
        self.assertIn("decision:", text)
        self.assertIn("--workflow cargo-full", text)
        self.assertIn("value: ${{ jobs.results.outputs.consumed_decision_id }}", text)
        self.assertIn("consumed_decision_id: ${{ needs.decision.outputs.consumed_decision_id }}", text)
        self.assertIn("needs.decision.result", text)
        for job in [
            "general",
            "cargo_shear",
            "argument_comment_lint_package",
            "argument_comment_lint_prebuilt",
            "lint_build",
            "tests_macos_aarch64",
            "tests_linux_x64_remote",
            "tests_linux_arm64",
            "tests_windows_x64",
            "tests_windows_arm64",
        ]:
            self.assertIn(f"  {job}:", text)
        self.assertGreaterEqual(text.count("needs: decision"), 10)


if __name__ == "__main__":
    unittest.main()
