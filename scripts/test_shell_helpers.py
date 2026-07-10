#!/usr/bin/env python3

import shutil
import subprocess
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]


def script_text(name: str) -> str:
    return (REPO_ROOT / "scripts" / name).read_text(encoding="utf-8")


class ShellHelperScriptsTest(unittest.TestCase):
    def test_bazel_lock_check_preserves_command_and_remediation(self) -> None:
        text = script_text("check-module-bazel-lock.sh")

        self.assertIn("bazel mod deps --lockfile_mode=error", text)
        self.assertIn("MODULE.bazel.lock is out of date.", text)
        self.assertIn("just bazel-lock-update", text)

    def test_debug_codex_preserves_existing_binary_and_cargo_paths(self) -> None:
        text = script_text("debug-codex.sh")

        self.assertIn("CODEX_DEBUG_USE_EXISTING_BINARY", text)
        self.assertIn('"$CODEX_BIN" "$@"', text)
        self.assertIn('cargo run --quiet --bin codex -- "$@"', text)

    def test_clippy_target_helper_resolves_query_before_output(self) -> None:
        text = script_text("list-bazel-clippy-targets.sh")

        query_index = text.index('manual_rust_test_targets="$(')
        output_index = text.index("printf '%s\\n' \\")
        self.assertLess(query_index, output_index)
        self.assertIn("--windows-cross-compile", text)
        self.assertIn("-//codex-rs/v8-poc:all", text)
        self.assertIn("-windows-cross-bin$", text)

    def test_release_target_helper_emits_the_expected_bounded_targets(self) -> None:
        bash = shutil.which("bash")
        if bash is None:
            self.skipTest("bash is not available")

        result = subprocess.run(
            [bash, "-lc", "scripts/list-bazel-release-targets.sh"],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.splitlines(),
            ["//codex-rs/...", "-//codex-rs/v8-poc:all"],
        )


if __name__ == "__main__":
    unittest.main()
