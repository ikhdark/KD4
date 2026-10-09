from __future__ import annotations

import contextlib
import io
import json
import os
import shlex
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import git_doctor


def completed(returncode: int, *, stdout: str = "", stderr: str = ""):
    return subprocess.CompletedProcess(["git"], returncode, stdout, stderr)


class GitDoctorTest(unittest.TestCase):
    def test_real_git_success_diagnostics_reach_the_human_report(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            subprocess.run(
                ["git", "init", "--quiet"], cwd=repo,
                check=True, capture_output=True, timeout=30,
            )
            (repo / "tracked.txt").write_text("tracked\n", encoding="utf-8")
            subprocess.run(
                ["git", "add", "tracked.txt"], cwd=repo,
                check=True, capture_output=True, timeout=30,
            )
            hook = (repo / "missing-fsmonitor").as_posix()
            with mock.patch.object(git_doctor, "REPO_ROOT", repo), mock.patch.dict(
                os.environ,
                {
                    "GIT_CONFIG_COUNT": "1",
                    "GIT_CONFIG_KEY_0": "core.fsmonitor",
                    "GIT_CONFIG_VALUE_0": hook,
                    "GIT_OPTIONAL_LOCKS": "0",
                },
            ):
                report = git_doctor.build_report(8)
        self.assertEqual(report.status_return_code, 0)
        self.assertFalse(report.status_failed)
        self.assertTrue(report.status_error)
        self.assertIn("missing-fsmonitor", report.status_error)
        with (
            mock.patch.object(git_doctor, "build_report", return_value=report),
            contextlib.redirect_stdout(io.StringIO()) as stdout,
        ):
            self.assertEqual(git_doctor.main([]), 0)
        self.assertIn("succeeded with diagnostics", stdout.getvalue())
        self.assertIn(report.status_error, stdout.getvalue())

    def test_config_failure_is_not_an_unset_setting(self):
        with mock.patch.object(
            git_doctor, "run_git", return_value=completed(128, stderr="bad config")
        ):
            with self.assertRaisesRegex(git_doctor.RepositoryProbeError, "bad config"):
                git_doctor.git_config("core.fsmonitor")
        with mock.patch.object(git_doctor, "run_git", return_value=completed(1)):
            self.assertIsNone(git_doctor.git_config("core.fsmonitor"))

    def test_timed_status_discards_stdout(self):
        with mock.patch.object(
            git_doctor, "run_owned", return_value=completed(0)
        ) as run:
            self.assertFalse(git_doctor.timed_status(1).failed)
        self.assertIs(run.call_args.kwargs["stdout"], subprocess.DEVNULL)
        self.assertIs(run.call_args.kwargs["stderr"], subprocess.PIPE)
        self.assertEqual(run.call_args.kwargs["timeout"], 1)
        self.assertEqual(
            run.call_args.args[0],
            ["git", "status", "--short", "--untracked-files=all"],
        )

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
        def run_git(args, *, timeout=5.0, discard_stdout=False):
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

    def test_status_timeout_bounds_a_real_git_descendant(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)

            def git(*args: str) -> None:
                subprocess.run(
                    ["git", *args],
                    cwd=repo,
                    check=True,
                    capture_output=True,
                    timeout=30,
                )

            git("init", "--quiet")
            (repo / "tracked.txt").write_text("tracked\n", encoding="utf-8")
            git("add", "tracked.txt")
            started_marker = repo / ".git" / "fsmonitor-started"
            hook = repo / ".git" / "slow-fsmonitor"
            hook.write_text(
                "#!/bin/sh\n"
                f"printf started > {shlex.quote(started_marker.as_posix())}\n"
                "sleep 3\n"
                "printf 'token\\0/\\0'\n",
                encoding="utf-8",
            )
            hook.chmod(0o755)
            git("config", "core.fsmonitor", shlex.quote(hook.as_posix()))
            started = time.monotonic()
            with mock.patch.object(git_doctor, "REPO_ROOT", repo):
                result = git_doctor.timed_status(0.5)
            elapsed = time.monotonic() - started

            self.assertTrue(started_marker.exists(), "the real hook must have started")
            self.assertTrue(result.timed_out)
            self.assertFalse(result.failed)
            self.assertLess(elapsed, 2.0, "timeout must not wait for the 3s child")

    def test_cleanup_failure_is_reported_as_a_failed_status(self) -> None:
        with mock.patch.object(
            git_doctor,
            "run_owned",
            side_effect=git_doctor.CleanupFailed("cleanup failed"),
        ):
            result = git_doctor.timed_status(1.0)
        self.assertTrue(result.failed)
        self.assertIn("cleanup failed", result.error)

    @unittest.skipUnless(os.name == "nt", "Windows FSMonitor daemon lifecycle")
    def test_successful_status_preserves_the_fsmonitor_daemon(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            repo = Path(temp)

            def git(*args: str) -> subprocess.CompletedProcess:
                return subprocess.run(
                    ["git", *args],
                    cwd=repo,
                    check=False,
                    capture_output=True,
                    timeout=30,
                )

            git("init", "--quiet").check_returncode()
            (repo / "tracked.txt").write_text("tracked\n", encoding="utf-8")
            git("add", "tracked.txt").check_returncode()
            git("config", "core.fsmonitor", "true").check_returncode()
            try:
                with mock.patch.object(git_doctor, "REPO_ROOT", repo):
                    result = git_doctor.timed_status(5)
                self.assertEqual(result.return_code, 0)
                self.assertEqual(git("fsmonitor--daemon", "status").returncode, 0)
            finally:
                git("fsmonitor--daemon", "stop")

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
