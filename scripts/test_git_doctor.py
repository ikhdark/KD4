from __future__ import annotations

import contextlib
import io
import json
import subprocess
import unittest
from unittest import mock

from scripts import git_doctor


def completed(returncode: int, *, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(["git"], returncode, stdout, stderr)


class GitDoctorTest(unittest.TestCase):
    def test_repository_root_probe_failure_is_fatal(self) -> None:
        with mock.patch.object(
            git_doctor,
            "run_git",
            return_value=completed(128, stderr="fatal: not a git repository\n"),
        ):
            with self.assertRaisesRegex(
                git_doctor.RepositoryProbeError, "not a git repository"
            ):
                git_doctor.build_report(1.0)

    def test_nonzero_status_is_reported_and_main_fails(self) -> None:
        def run_git(args, *, timeout=5.0):
            del timeout
            if args[0] == "rev-parse":
                return completed(0, stdout="/repo\n")
            if args[0] == "config":
                return completed(1)
            return completed(128, stderr="fatal: broken index\n")

        with (
            mock.patch.object(git_doctor, "run_git", side_effect=run_git),
            mock.patch.object(git_doctor, "path_kind", return_value="windows"),
            mock.patch.object(
                git_doctor, "unreadable_pytest_cache_dirs", return_value=()
            ),
        ):
            report = git_doctor.build_report(1.0)

        self.assertTrue(report.status_failed)
        self.assertEqual(report.status_return_code, 128)
        self.assertEqual(report.status_error, "fatal: broken index")

        output = io.StringIO()
        with (
            mock.patch.object(git_doctor, "build_report", return_value=report),
            contextlib.redirect_stdout(output),
        ):
            self.assertEqual(git_doctor.main(["--json"]), 1)
        self.assertTrue(json.loads(output.getvalue())["status_failed"])

    def test_status_timeout_is_distinct_from_command_failure(self) -> None:
        with mock.patch.object(
            git_doctor,
            "run_git",
            side_effect=subprocess.TimeoutExpired(["git", "status"], 1.0),
        ):
            result = git_doctor.timed_status(1.0)
        self.assertTrue(result.timed_out)
        self.assertFalse(result.failed)
        self.assertIsNone(result.return_code)

    def test_git_boolean_spellings_are_equivalent(self) -> None:
        for value in ("true", "yes", "on", "1", "TRUE", " Yes "):
            with self.subTest(value=value):
                self.assertTrue(git_doctor.git_boolean_enabled(value))
                self.assertFalse(
                    any(
                        "untracked cache" in item
                        for item in git_doctor.recommendations("windows", "true", value)
                    )
                )
        for value in ("false", "no", "off", "0", None, "invalid"):
            with self.subTest(value=value):
                self.assertFalse(git_doctor.git_boolean_enabled(value))
                self.assertTrue(
                    any(
                        "untracked cache" in item
                        for item in git_doctor.recommendations("windows", "true", value)
                    )
                )


if __name__ == "__main__":
    unittest.main()
