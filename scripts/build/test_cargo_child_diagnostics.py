"""Real OS environment observations and non-compiling runner diagnostics."""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts.build import cargo_child_diagnostics as diagnostics
from scripts.build import rust_test_runner as runner
from scripts.common.process_owner import owned_process


class CargoChildDiagnosticsTest(unittest.TestCase):
    def test_opt_in_changes_only_diagnostics(self):
        env = {
            "RUSTFLAGS": "-C debuginfo=1",
            "RUSTC_WRAPPER": "sccache",
            "CARGO_TARGET_DIR": "target/lanes/core-tests",
        }
        command = ["cargo", "nextest", "list", "-p", "codex-core"]
        self.assertEqual(
            diagnostics.diagnostic_command(command, env), (command, env, False)
        )
        enabled = {**env, "CODEX_CARGO_DIAGNOSTICS": "1", "CARGO_LOG": "warn"}
        args, child, active = diagnostics.diagnostic_command(command, enabled)
        self.assertTrue(active)
        self.assertEqual(
            args,
            [
                "cargo",
                "nextest",
                "list",
                "--cargo-verbose",
                "--cargo-verbose",
                "-p",
                "codex-core",
            ],
        )
        self.assertEqual(
            {k: child[k] for k in enabled if k != "CARGO_LOG"},
            {k: v for k, v in enabled.items() if k != "CARGO_LOG"},
        )
        self.assertEqual(
            child["CARGO_LOG"], "warn,cargo::core::compiler::fingerprint=trace"
        )
        # Compilation inputs remain independently specified by the fixture.
        for key in env:
            self.assertEqual(child[key], env[key])
        test_only = ["cargo", "nextest", "run", "--binaries-metadata", "bins.json"]
        self.assertEqual(
            diagnostics.diagnostic_command(test_only, enabled),
            (test_only, enabled, False),
        )
        self.assertNotIn("--cargo-verbose", command)
        self.assertEqual(enabled["CARGO_LOG"], "warn")

    def test_observes_actual_child_not_parent_environment(self):
        # This is a real child, not a mocked env dictionary passed to an executor.
        child_env = {
            **os.environ,
            "RUSTFLAGS": "child-observed-sentinel",
            "UNRELATED_API_TOKEN": "must-not-be-retained",
        }
        with owned_process(
            [
                sys.executable,
                "-I",
                "-B",
                "-c",
                "import time; print('ready', flush=True); time.sleep(30)",
            ],
            env=child_env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        ) as process:
            self.assertEqual(
                process.stdout.readline(),
                b"ready\r\n" if os.name == "nt" else b"ready\n",
            )
            process.stdout.close()
            observed = diagnostics.process_snapshot(process.pid)
            self.assertEqual(observed["pid"], process.pid)
            self.assertEqual(
                observed["environment"]["RUSTFLAGS"], "child-observed-sentinel"
            )
            self.assertNotIn("UNRELATED_API_TOKEN", observed["environment"])
            self.assertIn("time.sleep", observed["command_line"])
            self.assertEqual(
                Path(observed["image"]).resolve().name.lower(),
                Path(sys.executable).resolve().name.lower(),
            )
            self.assertIn(
                process.pid, {pid for pid, _, _ in diagnostics.process_tree()}
            )

    def test_exited_child_is_not_reported_as_empty_environment(self):
        with subprocess.Popen([sys.executable, "-I", "-B", "-c", "pass"]) as process:
            process.wait(timeout=10)
            with self.assertRaises(OSError):
                diagnostics.process_snapshot(process.pid)

    def test_real_executor_observes_child_and_preserves_exit(self):
        # Use a Python stand-in instead of compiling: only the candidate-name
        # filter is replaced. Popen, process ownership and OS reads are real.
        inventory = diagnostics.process_tree

        def candidates():
            return [(pid, parent, "cargo.exe") for pid, parent, _ in inventory()]

        with tempfile.TemporaryDirectory() as directory:
            env = {
                **os.environ,
                "CODEX_RUST_TEST_LOG_DIR": directory,
                "RUSTFLAGS": "actual-child-input",
            }
            command = [
                sys.executable,
                "-I",
                "-B",
                "-c",
                "import time; print('child output', flush=True); time.sleep(.6); raise SystemExit(7)",
            ]
            timings = {}
            with (
                mock.patch.object(
                    runner, "diagnostic_command", return_value=(command, env, True)
                ),
                mock.patch.object(diagnostics, "process_tree", side_effect=candidates),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                result = runner._default_executor(
                    command,
                    cwd=Path(directory),
                    env=env,
                    capture=runner.CAPTURE_BOTH,
                    timings=timings,
                )
            self.assertEqual(result.returncode, 7)
            self.assertEqual(result.stdout, "child output\n")
            self.assertEqual(timings["launch_argv"], command)
            rows = [
                json.loads(line)
                for line in Path(timings["child_environment_observations"])
                .read_text()
                .splitlines()
            ]
            observed = [row for row in rows if row["kind"] == "observed_child"]
            self.assertTrue(observed, rows)
            self.assertTrue(
                all(
                    row["environment"]["RUSTFLAGS"] == "actual-child-input"
                    for row in observed
                )
            )

    def test_sampling_records_unverified_children_without_inventing_env(self):
        process = mock.Mock(pid=123)
        process.wait.return_value = 0
        with (
            tempfile.TemporaryDirectory() as directory,
            mock.patch.object(
                diagnostics,
                "process_tree",
                return_value=[
                    (123, 1, "cargo.exe"),
                    (456, 123, "cargo.exe"),
                    (789, 1, "cargo.exe"),
                ],
            ),
            mock.patch.object(
                diagnostics, "process_snapshot", side_effect=OSError("exited")
            ) as snapshot,
        ):
            path = Path(directory) / "children.jsonl"
            self.assertEqual(
                diagnostics.wait_with_diagnostics(process, timeout=2, log_path=path), 0
            )
            rows = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(snapshot.call_count, 2)
            self.assertEqual(
                [r["pid"] for r in rows if r["kind"] == "unverified_child"], [123, 456]
            )
            self.assertFalse(any("environment" in r for r in rows))
            self.assertFalse(rows[-1]["complete"])

    def test_real_executor_keeps_timeout_cleanup_when_observation_storage_fails(self):
        # Keep the real owner/child/timeout path; no Cargo is launched.
        with tempfile.TemporaryDirectory() as directory:
            env = {
                **os.environ,
                "CODEX_RUST_TEST_LOG_DIR": directory,
                "CODEX_RUST_TEST_TIMEOUT_SECS": "0.2",
            }
            command = [sys.executable, "-I", "-B", "-c", "import time; time.sleep(30)"]
            with (
                mock.patch.object(
                    runner, "diagnostic_command", return_value=(command, env, True)
                ),
                mock.patch.object(
                    runner,
                    "wait_with_diagnostics",
                    side_effect=OSError("storage unavailable"),
                ),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                with self.assertRaises(runner.RunnerError) as error:
                    runner._default_executor(
                        command,
                        cwd=Path(directory),
                        env=env,
                        capture=runner.CAPTURE_BOTH,
                    )
            self.assertEqual(error.exception.outcome, "timed_out")
            self.assertIn("UNVERIFIED", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
