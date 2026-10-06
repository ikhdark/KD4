"""Regression checks for warm-lane admission defaults and checkout preflight."""
import contextlib
import inspect
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts import rust_build_status


class LaneLatencyTests(unittest.TestCase):
    def test_warm_wait_defaults_and_explicit_override(self):
        for function in (rust_build_status.reserve_cargo_lane, rust_build_status.run_in_cargo_lane):
            self.assertEqual(inspect.signature(function).parameters["warm_wait_seconds"].default, 600.0)
        for options, expected in (([], 600.0), (["--warm-wait-seconds", "42"], 42.0)):
            with mock.patch.object(rust_build_status, "run_in_cargo_lane", return_value=0) as run:
                self.assertEqual(rust_build_status.main([
                    "run-lane", "--lane", "core-tests", *options, "--", "cargo", "check",
                ]), 0)
            self.assertEqual(run.call_args.kwargs["warm_wait_seconds"], expected)

    def test_checkout_guard_rejects_wrong_root_without_relocating(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            rust = root / "codex-rs"
            rust.mkdir()
            (rust / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
            with contextlib.chdir(root):
                with self.assertRaisesRegex(ValueError, "No build lane was reserved"):
                    rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], root)
                rust_build_status._guard_cargo_checkout_cwd([
                    "cargo", "check", "--manifest-path", "codex-rs/Cargo.toml",
                ], root)
                self.assertEqual(Path.cwd(), root)
            with contextlib.chdir(rust):
                rust_build_status._guard_cargo_checkout_cwd(["cargo", "check"], root)


if __name__ == "__main__":
    unittest.main()
