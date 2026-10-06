"""Process/output lifetime regressions using real child processes."""

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from scripts import process_owner, rust_tool_env, vscode_runtime_proof


class ProcessOwnerTest(unittest.TestCase):
    def test_cli_below_normal_priority_is_opt_in(self):
        for flags in ([], ["--below-normal-priority"]):
            with self.subTest(flags=flags), mock.patch.object(
                process_owner, "run_owned",
                return_value=subprocess.CompletedProcess(["fixture"], 7),
            ) as run:
                self.assertEqual(process_owner.main([*flags, "--", "fixture"]), 7)
                self.assertEqual(run.call_args.args[0], ["fixture"])
                self.assertEqual(
                    run.call_args.kwargs["creationflags"],
                    getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0) if flags else 0,
                )

    def test_timeout_preserves_flushed_short_diagnostic(self):
        result = process_owner.run_finite(
            [
                sys.executable,
                "-c",
                "import time; print('diagnostic', flush=True); time.sleep(60)",
            ],
            timeout=0.5,
        )
        self.assertEqual((result.status, result.returncode), ("timed_out", 124))
        self.assertEqual(result.stdout, "diagnostic\n")
        self.assertFalse(result.output_truncated)
        self.assertLess(result.elapsed, 3)

    def test_cancellation_preserves_observed_output(self):
        observed = []
        with process_owner.operation() as operation:

            def observe(chunk):
                observed.append(chunk)
                if "diagnostic" in chunk:
                    operation.cancelled.set()

            result = process_owner.run_finite(
                [
                    sys.executable,
                    "-c",
                    "import time; print('diagnostic', flush=True); time.sleep(60)",
                ],
                timeout=5,
                observe=observe,
            )
        self.assertEqual((result.status, result.returncode), ("cancelled", 130))
        self.assertEqual(result.stdout, "diagnostic\n")
        self.assertEqual("".join(observed).replace("\r\n", "\n"), result.stdout)

    def test_observer_failure_is_not_repeated_during_cleanup(self):
        with self.assertRaisesRegex(ValueError, "observer failed"):
            process_owner.run_finite(
                [sys.executable, "-c", "import sys; sys.stdout.write('x'*2097152)"],
                timeout=5,
                observe=mock.Mock(side_effect=ValueError("observer failed")),
            )

    def test_version_probe_uses_owned_timeout_and_reaps_shim_child(self):
        if os.name != "nt":
            self.skipTest("Windows command shim")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root / "orphan"
            child = root / "child.py"
            child.write_text(
                "import time\nfrom pathlib import Path\n"
                f"time.sleep(1); Path({str(marker)!r}).touch(); time.sleep(60)\n"
            )
            shim = root / "codex.cmd"
            shim.write_text(f'@echo off\n"{sys.executable}" "{child}"\n')

            def bounded(args, **kwargs):
                self.assertEqual(kwargs.pop("timeout"), 10)
                return process_owner.run_finite(args, timeout=0.3, **kwargs)

            with mock.patch.object(
                vscode_runtime_proof, "run_finite", side_effect=bounded
            ):
                self.assertIsNone(
                    vscode_runtime_proof.run_version(str(shim), enabled=True)
                )
            time.sleep(1)
            self.assertFalse(marker.exists(), "version-probe child survived cleanup")
        self.assertTrue(
            vscode_runtime_proof.run_version(sys.executable, enabled=True).startswith(
                "Python "
            )
        )

    def test_shared_cache_setup_is_opt_in_and_failure_does_not_launch_work(self):
        with mock.patch.object(rust_tool_env.subprocess, "run") as run:
            for env in ({}, {"RUSTC_WRAPPER": ""}, {"RUSTC_WRAPPER": "custom-wrapper"}):
                rust_tool_env.prepare_shared_sccache(env=env)
            run.assert_not_called()
        with (
            mock.patch.object(
                process_owner,
                "prepare_shared_sccache",
                side_effect=OSError("cache unavailable"),
            ),
            mock.patch.object(process_owner, "owned_process") as launch,
            self.assertRaisesRegex(OSError, "cache unavailable"),
        ):
            process_owner.run_owned(["cargo", "build"], prepare_sccache=True)
        launch.assert_not_called()

    def test_existing_shared_cache_is_not_restarted_and_other_errors_propagate(self):
        for code, error in ((2, "Address in use"), (2, "permission denied")):
            with (
                self.subTest(error=error),
                mock.patch.object(
                    rust_tool_env.subprocess,
                    "run",
                    return_value=subprocess.CompletedProcess([], code, stderr=error),
                ) as run,
            ):
                if error == "Address in use":
                    rust_tool_env.prepare_shared_sccache(
                        env={"RUSTC_WRAPPER": "sccache"}
                    )
                else:
                    with self.assertRaises(subprocess.CalledProcessError):
                        rust_tool_env.prepare_shared_sccache(
                            env={"RUSTC_WRAPPER": "sccache"}
                        )
                self.assertEqual(run.call_count, 1)
                self.assertEqual(run.call_args.args[0][1:], ["--start-server"])

    @unittest.skipUnless(os.name == "nt", "Windows job lifetime")
    def test_shared_sccache_survives_owned_command_without_restart(self):
        sccache = shutil.which("sccache")
        if sccache is None:
            self.skipTest("sccache is not installed")
        with tempfile.TemporaryDirectory() as directory:
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            env = {
                **os.environ,
                "RUSTC_WRAPPER": sccache,
                "SCCACHE_DIR": directory,
                "SCCACHE_SERVER_PORT": str(port),
                "SCCACHE_IDLE_TIMEOUT": "30",
            }
            code = (
                "from scripts.process_owner import run_owned\n"
                f"result = run_owned([{sccache!r}, '--show-stats'], prepare_sccache=True, capture_output=True)\n"
                "assert result.returncode == 0, result.stderr\n"
                f"result = run_owned([{sccache!r}, '--show-stats'], prepare_sccache=True, capture_output=True)\n"
                "assert result.returncode == 0, result.stderr\n"
            )
            with process_owner.owned_process(
                [sys.executable, "-c", code],
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                cwd=Path(__file__).resolve().parents[1],
            ) as outer:
                _, errors = outer.communicate(timeout=15)
                self.assertEqual(outer.returncode, 0, errors)
                time.sleep(0.3)
                with socket.create_connection(("127.0.0.1", port), timeout=1):
                    pass
            time.sleep(0.3)
            with socket.socket() as sock:
                sock.settimeout(1)
                self.assertNotEqual(sock.connect_ex(("127.0.0.1", port)), 0)


if __name__ == "__main__":
    unittest.main()
