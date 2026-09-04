from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class GitDoctorTest(unittest.TestCase):
    def initialize_repository(self, root: Path) -> dict[str, str]:
        initialized = subprocess.run(
            ["git", "init", "--quiet", str(root)],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(initialized.returncode, 0, initialized.stderr)
        configured = subprocess.run(
            ["git", "-C", str(root), "config", "core.fsmonitor", "true"],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(configured.returncode, 0, configured.stderr)
        env = os.environ.copy()
        env.update(
            {
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_DIR": str(root / ".git"),
                "GIT_WORK_TREE": str(root),
            }
        )
        return env

    def run_cli(
        self,
        env: dict[str, str],
        *args: str,
    ) -> subprocess.CompletedProcess[str]:
        script = Path(__file__).with_name("git_doctor.py").resolve()
        return subprocess.run(
            [sys.executable, str(script), *args],
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_cli_reports_repository_root_probe_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = os.environ.copy()
            env.update(
                {
                    "GIT_CONFIG_NOSYSTEM": "1",
                    "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_DIR": str(root / "missing.git"),
                    "GIT_WORK_TREE": str(root),
                }
            )
            completed = self.run_cli(env, "--json")

        self.assertEqual(completed.returncode, 2)
        self.assertIn("git doctor failed", completed.stderr)
        self.assertIn("not a git repository", completed.stderr)

    def test_cli_reports_corrupt_index_status_failure_as_json(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = self.initialize_repository(root)
            tracked = root / "tracked.txt"
            tracked.write_text("tracked\n", encoding="utf-8")
            added = subprocess.run(
                ["git", "-C", str(root), "add", "tracked.txt"],
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(added.returncode, 0, added.stderr)
            (root / ".git" / "index").write_bytes(b"broken index")
            completed = self.run_cli(env, "--json", "--timeout", "5")

        self.assertEqual(completed.returncode, 1, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertTrue(report["status_failed"])
        self.assertFalse(report["status_timed_out"])
        self.assertNotEqual(report["status_return_code"], 0)
        self.assertIn("index", report["status_error"].lower())

    def test_cli_reports_zero_second_status_timeout_distinctly(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = self.initialize_repository(root)
            completed = self.run_cli(env, "--json", "--timeout", "0")

        self.assertEqual(completed.returncode, 1, completed.stderr)
        report = json.loads(completed.stdout)
        self.assertTrue(report["status_timed_out"])
        self.assertFalse(report["status_failed"])
        self.assertIsNone(report["status_return_code"])

    def test_cli_treats_git_boolean_spellings_equivalently(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            env = self.initialize_repository(root)
            cases = (
                ("true", False),
                ("yes", False),
                ("on", False),
                ("1", False),
                ("TRUE", False),
                (" Yes ", False),
                ("false", True),
                ("no", True),
                ("off", True),
                ("0", True),
                (None, True),
                ("invalid", True),
            )
            for value, expects_recommendation in cases:
                with self.subTest(value=value):
                    command = [
                        "git",
                        "-C",
                        str(root),
                        "config",
                    ]
                    if value is None:
                        subprocess.run(
                            [*command, "--unset-all", "core.untrackedCache"],
                            env=env,
                            check=False,
                            capture_output=True,
                            text=True,
                        )
                    else:
                        configured = subprocess.run(
                            [*command, "core.untrackedCache", value],
                            env=env,
                            check=False,
                            capture_output=True,
                            text=True,
                        )
                        self.assertEqual(configured.returncode, 0, configured.stderr)
                    completed = self.run_cli(env, "--json", "--timeout", "5")
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    recommendations = json.loads(completed.stdout)["recommendations"]
                    has_recommendation = any(
                        "untracked cache" in item for item in recommendations
                    )
                    self.assertEqual(has_recommendation, expects_recommendation)


if __name__ == "__main__":
    unittest.main()
