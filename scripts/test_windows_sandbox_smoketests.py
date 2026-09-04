from __future__ import annotations

import json
import hashlib
import os
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SMOKE_SCRIPT = REPO_ROOT / "codex-rs" / "windows-sandbox-rs" / "sandbox_smoketests.py"
PREFIX = "python-script-case::windows-sandbox-smoke::"
EXPECTED_CASE_IDS = [
    PREFIX + "read-only-write-cwd-denied",
    PREFIX + "workspace-write-cwd-allowed",
    PREFIX + "workspace-write-outside-denied",
    PREFIX + "workspace-write-additional-root-allowed",
    PREFIX + "read-only-write-additional-root-denied",
    PREFIX + "workspace-write-temp-allowed",
    PREFIX + "read-only-write-temp-cmd-denied",
    PREFIX + "workspace-write-append-allowed",
    PREFIX + "read-only-append-denied",
    PREFIX + "workspace-write-powershell-set-content-allowed",
    PREFIX + "read-only-powershell-set-content-denied",
    PREFIX + "workspace-write-mkdir-write-allowed",
    PREFIX + "workspace-write-rename-allowed",
    PREFIX + "workspace-write-delete-allowed",
    PREFIX + "read-only-python-write-denied",
    PREFIX + "workspace-write-python-write-allowed",
    PREFIX + "workspace-write-outbound-curl-denied",
    PREFIX + "workspace-write-outbound-iwr-denied",
    PREFIX + "workspace-write-loopback-proxy-allowed",
    PREFIX + "workspace-write-direct-loopback-denied",
    PREFIX + "read-only-write-temp-powershell-denied",
    PREFIX + "workspace-write-curl-version",
    PREFIX + "workspace-write-ripgrep-version",
    PREFIX + "workspace-write-git-version",
    PREFIX + "workspace-write-powershell-bytes-allowed",
    PREFIX + "read-only-powershell-bytes-denied",
    PREFIX + "workspace-write-deep-mkdir-write-allowed",
    PREFIX + "workspace-write-move-allowed",
    PREFIX + "read-only-cmd-redirection-denied",
    PREFIX + "workspace-write-junction-cwd-poisoning-denied",
    PREFIX + "workspace-write-junction-windows-denied",
    PREFIX + "workspace-write-raw-device-access-denied",
    PREFIX + "workspace-write-named-pipe-creation-denied",
    PREFIX + "workspace-write-ads-write-denied",
    PREFIX + "workspace-write-long-path-escape-denied",
    PREFIX + "workspace-write-protected-path-case-variation-denied",
    PREFIX + "workspace-write-codex-cap-sid-tamper-denied",
    PREFIX + "workspace-write-codex-policy-tamper-denied",
    PREFIX + "workspace-write-path-stub-bypass-denied",
    PREFIX + "workspace-write-symlink-race-denied",
    PREFIX + "workspace-write-deep-junction-world-writable-escape-denied",
    PREFIX + "workspace-root-symlink-poisoning-denied",
    PREFIX + "workspace-write-unc-link-escape-denied",
    PREFIX + "workspace-write-other-drive-link-escape-denied",
    PREFIX + "workspace-write-post-timeout-outside-denied",
    PREFIX + "read-only-start-process-uri-denied",
]


class WindowsSandboxSmokeModesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sandbox-smoke-modes-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)

    def _run(
        self, *arguments: str, env: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SMOKE_SCRIPT), *arguments],
            cwd=str(REPO_ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=20,
            check=False,
        )

    def _fake_codex(self) -> Path:
        implementation = self.base / "fake_codex.py"
        implementation.write_text(
            textwrap.dedent(
                """\
                import subprocess
                import sys

                arguments = sys.argv[1:]
                try:
                    separator = arguments.index("--")
                except ValueError:
                    raise SystemExit(91)
                child = arguments[separator + 1:]
                if not child:
                    raise SystemExit(92)
                completed = subprocess.run(child)
                raise SystemExit(completed.returncode)
                """
            ),
            encoding="utf-8",
        )
        if os.name == "nt":
            wrapper = self.base / "fake_codex.cmd"
            wrapper.write_text(
                f'@"{sys.executable}" "{implementation}" %*\r\n',
                encoding="utf-8",
            )
        else:
            wrapper = self.base / "fake_codex"
            wrapper.write_text(
                f'#!/bin/sh\nexec "{sys.executable}" "{implementation}" "$@"\n',
                encoding="utf-8",
            )
            wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR)
        return wrapper

    def _failing_codex(self) -> Path:
        if os.name == "nt":
            wrapper = self.base / "failing_codex.cmd"
            wrapper.write_text("@exit /b 1\r\n", encoding="utf-8")
        else:
            wrapper = self.base / "failing_codex"
            wrapper.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
            wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR)
        return wrapper

    def _read_report(self, path: Path) -> dict[str, object]:
        return json.loads(path.read_text(encoding="utf-8"))

    def test_list_json_is_deterministic_and_has_exact_frozen_case_ids(self) -> None:
        first = self._run("--list-json")
        second = self._run("--list-json")

        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(first.stdout, second.stdout)
        self.assertEqual(first.stderr, "")
        payload = json.loads(first.stdout)
        self.assertEqual(payload["report_type"], "WindowsSandboxSmokeCaseListV1")
        case_ids = [case["id"] for case in payload["cases"]]
        self.assertEqual(case_ids, EXPECTED_CASE_IDS)
        self.assertEqual(len(case_ids), 46)
        self.assertEqual(len(set(case_ids)), 46)

    def test_run_case_executes_only_the_exact_selection_under_attempt_root(
        self,
    ) -> None:
        case_id = PREFIX + "workspace-write-python-write-allowed"
        attempt_root = self.base / "attempt"
        report = self.base / "report.json"
        completed = self._run(
            "--run-case",
            case_id,
            "--report-json",
            str(report),
            "--attempt-root",
            str(attempt_root),
            "--codex-bin",
            str(self._fake_codex()),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = self._read_report(report)
        self.assertEqual(payload["intended_case_ids"], [case_id])
        self.assertEqual(payload["selected_case_ids"], [case_id])
        self.assertEqual(payload["executed_case_ids"], [case_id])
        self.assertEqual(payload["counts"]["executed"], 1)
        self.assertEqual(payload["counts"]["passed"], 1)
        self.assertEqual(payload["results"][0]["status"], "passed")
        self.assertEqual(payload["results"][0]["sandbox_launches"], 1)
        self.assertTrue(
            (
                attempt_root / "windows-sandbox-smoke" / "workspace" / "py_ok.txt"
            ).is_file()
        )

    def test_run_case_reports_exact_codex_binary_identity(self) -> None:
        case_id = PREFIX + "workspace-write-python-write-allowed"
        report = self.base / "binary-identity.json"
        codex_binary = self._fake_codex().resolve()
        expected_hash = hashlib.sha256(codex_binary.read_bytes()).hexdigest()

        completed = self._run(
            "--run-case",
            case_id,
            "--report-json",
            str(report),
            "--attempt-root",
            str(self.base / "binary-identity-attempt"),
            "--codex-bin",
            str(codex_binary),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = self._read_report(report)
        identity = payload["codex_executable_identity"]
        self.assertEqual(identity["requested"], str(codex_binary))
        self.assertEqual(identity["resolved_path"], str(codex_binary))
        self.assertEqual(identity["sha256_before"], expected_hash)
        self.assertEqual(identity["sha256_after"], expected_hash)
        self.assertEqual(
            payload["results"][0]["codex_executable_identity"],
            identity,
        )

    def test_build_current_codex_rejects_non_fork_binary_before_execution(
        self,
    ) -> None:
        case_id = PREFIX + "workspace-write-python-write-allowed"
        report = self.base / "non-fork-binary.json"

        completed = self._run(
            "--run-case",
            case_id,
            "--report-json",
            str(report),
            "--attempt-root",
            str(self.base / "non-fork-binary-attempt"),
            "--codex-bin",
            str(self._fake_codex()),
            "--build-current-codex",
        )

        self.assertEqual(completed.returncode, 2)
        payload = self._read_report(report)
        self.assertEqual(payload["selected_case_ids"], [case_id])
        self.assertEqual(payload["executed_case_ids"], [])
        self.assertEqual(payload["counts"]["pre_result_error"], 1)
        self.assertIn(
            "did not match the active Cargo target",
            payload["results"][0]["detail"],
        )

    def test_repeatable_run_case_executes_the_complete_exact_selection(self) -> None:
        selected = [
            PREFIX + "workspace-write-python-write-allowed",
            PREFIX + "workspace-write-mkdir-write-allowed",
        ]
        report = self.base / "multi-case-report.json"
        completed = self._run(
            *(part for case_id in selected for part in ("--run-case", case_id)),
            "--report-json",
            str(report),
            "--attempt-root",
            str(self.base / "multi-case-attempt"),
            "--codex-bin",
            str(self._fake_codex()),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = self._read_report(report)
        self.assertEqual(payload["intended_case_ids"], selected)
        self.assertEqual(payload["selected_case_ids"], selected)
        self.assertEqual(payload["executed_case_ids"], selected)
        self.assertEqual(payload["counts"]["intended"], 2)
        self.assertEqual(payload["counts"]["selected"], 2)
        self.assertEqual(payload["counts"]["executed"], 2)
        self.assertEqual(payload["counts"]["passed"], 2)
        self.assertEqual(
            [result["id"] for result in payload["results"]],
            selected,
        )

    def test_zero_unknown_and_duplicate_selections_execute_nothing(self) -> None:
        zero_report = self.base / "zero.json"
        zero_result = self._run(
            "--report-json",
            str(zero_report),
            "--attempt-root",
            str(self.base / "zero-attempt"),
        )
        self.assertEqual(zero_result.returncode, 2)
        zero_payload = self._read_report(zero_report)
        self.assertEqual(zero_payload["counts"]["intended"], 0)
        self.assertEqual(zero_payload["counts"]["selected"], 0)
        self.assertEqual(zero_payload["counts"]["executed"], 0)

        unknown = PREFIX + "does-not-exist"
        unknown_report = self.base / "unknown.json"
        unknown_result = self._run(
            "--run-case",
            unknown,
            "--report-json",
            str(unknown_report),
            "--attempt-root",
            str(self.base / "unknown-attempt"),
        )
        self.assertEqual(unknown_result.returncode, 2)
        unknown_payload = self._read_report(unknown_report)
        self.assertEqual(unknown_payload["counts"]["selected"], 0)
        self.assertEqual(unknown_payload["counts"]["executed"], 0)
        self.assertEqual(unknown_payload["results"], [])

        case_id = EXPECTED_CASE_IDS[0]
        duplicate_report = self.base / "duplicate.json"
        duplicate_result = self._run(
            "--run-case",
            case_id,
            "--run-case",
            case_id,
            "--report-json",
            str(duplicate_report),
            "--attempt-root",
            str(self.base / "duplicate-attempt"),
        )
        self.assertEqual(duplicate_result.returncode, 2)
        duplicate_payload = self._read_report(duplicate_report)
        self.assertEqual(duplicate_payload["counts"]["intended"], 2)
        self.assertEqual(duplicate_payload["counts"]["selected"], 0)
        self.assertEqual(duplicate_payload["counts"]["executed"], 0)

    def test_runner_failure_before_child_start_is_not_execution_evidence(self) -> None:
        case_id = PREFIX + "workspace-write-python-write-allowed"
        report = self.base / "runner-failure.json"
        completed = self._run(
            "--run-case",
            case_id,
            "--report-json",
            str(report),
            "--attempt-root",
            str(self.base / "runner-failure-attempt"),
            "--codex-bin",
            str(self._failing_codex()),
        )

        self.assertEqual(completed.returncode, 2)
        payload = self._read_report(report)
        self.assertEqual(payload["selected_case_ids"], [case_id])
        self.assertEqual(payload["executed_case_ids"], [])
        self.assertEqual(payload["counts"]["pre_result_error"], 1)
        self.assertEqual(payload["results"][0]["sandbox_launches"], 0)

    def test_missing_prerequisite_is_pre_result_error_not_skip(self) -> None:
        case_id = PREFIX + "workspace-write-ripgrep-version"
        report = self.base / "prerequisite.json"
        environment = os.environ.copy()
        environment["PATH"] = str(self.base / "empty-path")
        completed = self._run(
            "--run-case",
            case_id,
            "--report-json",
            str(report),
            "--attempt-root",
            str(self.base / "prerequisite-attempt"),
            "--codex-bin",
            str(self._fake_codex()),
            env=environment,
        )

        self.assertEqual(completed.returncode, 2)
        payload = self._read_report(report)
        self.assertEqual(payload["selected_case_ids"], [case_id])
        self.assertEqual(payload["executed_case_ids"], [])
        self.assertEqual(payload["counts"]["pre_result_error"], 1)
        self.assertEqual(payload["results"][0]["status"], "pre_result_error")
        self.assertNotIn("skip", json.dumps(payload).casefold())


if __name__ == "__main__":
    unittest.main()
