import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import rust_build_status as owner


class TargetedRestorationTests(unittest.TestCase):
    def test_finite_coordination_timeout_does_not_become_a_busy_lane_wait(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(owner, "initialize_cargo_lanes_root", return_value=root):
                with owner.cargo_lane_coordination_lock(root, timeout_seconds=0.1):
                    with self.assertRaises(TimeoutError):
                        with owner.reserve_cargo_lane(
                            repo_root=root, requested_lane="focused", command=["cargo", "check"],
                            lane_root=root, lock_timeout_seconds=0.01,
                        ):
                            self.fail("contended coordination lock was admitted")

    def test_unrelated_package_does_not_spawn_a_helper_build(self):
        with patch.object(owner, "run_owned") as run:
            self.assertEqual(owner._provision_code_mode_test_host(
                ["cargo", "nextest", "run", "-p", "codex-git-utils"], Path("unused"), {},
            ), 0)
            run.assert_not_called()

    def test_selected_core_tests_get_a_fresh_explicit_helper_in_the_owned_lane(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory)
            host = target / "debug" / ("codex-code-mode-host.exe" if owner.os.name == "nt" else "codex-code-mode-host")
            host.parent.mkdir()
            host.write_bytes(b"fixture")
            env = {}
            with patch.object(owner, "run_owned") as run:
                run.return_value.returncode = 0
                self.assertEqual(owner._provision_code_mode_test_host(
                    ["cargo", "nextest", "run", "-p", "codex-core-integration-tests", "--test", "core_thread_state"], target, env,
                ), 0)
                self.assertIn("build", run.call_args.args[0])
                self.assertEqual(env["CARGO_BIN_EXE_codex-code-mode-host"], str(host.resolve()))


if __name__ == "__main__":
    unittest.main()
