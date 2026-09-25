#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import wrapper_common


def load_entrypoint(filename: str):
    path = Path(__file__).resolve().parent / filename
    module_name = filename.removesuffix(".py").replace("-", "_")
    spec = importlib.util.spec_from_file_location(module_name, path)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WrapperCommonTest(unittest.TestCase):
    def test_wrapper_forwards_options_environment_and_linter_exit_status(self) -> None:
        """The CLI entrypoint must deliver the requested options to the real exec boundary."""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = str(root / "codex-rs" / "Cargo.toml")
            custom_manifest = str(root / "custom" / "Cargo.toml")
            cases = [
                (
                    [],
                    [
                        "--manifest-path",
                        manifest,
                        "--workspace",
                        "--no-deps",
                        "--",
                        "--all-targets",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["-p", "codex-core", "--", "--tests"],
                    [
                        "--manifest-path",
                        manifest,
                        "--no-deps",
                        "-p",
                        "codex-core",
                        "--",
                        "--tests",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["-pcodex-core"],
                    [
                        "--manifest-path",
                        manifest,
                        "--no-deps",
                        "-pcodex-core",
                        "--",
                        "--all-targets",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["--", "-p", "codex-core"],
                    [
                        "--manifest-path",
                        manifest,
                        "--no-deps",
                        "--",
                        "-p",
                        "codex-core",
                        "--all-targets",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["--fix", "-p", "codex-core"],
                    [
                        "--manifest-path",
                        manifest,
                        "--no-deps",
                        "--fix",
                        "-p",
                        "codex-core",
                        "--",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    [
                        "--manifest-path",
                        custom_manifest,
                        "--workspace",
                        "--no-deps",
                        "--",
                        "--bins",
                        "--ignore-rust-version",
                    ],
                    [
                        "--manifest-path",
                        custom_manifest,
                        "--workspace",
                        "--no-deps",
                        "--",
                        "--bins",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["--manifest-path", custom_manifest],
                    [
                        "--no-deps",
                        "--manifest-path",
                        custom_manifest,
                        "--",
                        "--all-targets",
                        "--ignore-rust-version",
                    ],
                ),
                (
                    ["--lib", "chosen"],
                    [
                        "--manifest-path",
                        manifest,
                        "--workspace",
                        "--no-deps",
                        "--lib",
                        "chosen",
                        "--",
                        "--all-targets",
                        "--ignore-rust-version",
                    ],
                ),
            ]
            module = load_entrypoint("run.py")
            prefix = [
                "cargo",
                "dylint",
                "--path",
                str(root / "tools" / "argument-comment-lint"),
            ]
            for argv, expected in cases:
                with self.subTest(argv=argv):
                    with (
                        mock.patch.object(module, "repo_root", return_value=root),
                        mock.patch.object(sys, "argv", ["run.py", *argv]),
                        mock.patch.dict(os.environ, {}, clear=True),
                        mock.patch.object(
                            wrapper_common,
                            "require_command",
                            side_effect=lambda name, *args: name,
                        ),
                        mock.patch.object(
                            wrapper_common,
                            "run_capture",
                            return_value=wrapper_common.TOOLCHAIN_CHANNEL,
                        ),
                        mock.patch.object(
                            wrapper_common.subprocess,
                            "run",
                            return_value=subprocess.CompletedProcess([], 7),
                        ) as run,
                        self.assertRaises(SystemExit) as exit_info,
                    ):
                        module.main()
                    self.assertEqual(exit_info.exception.code, 7)
                    run.assert_called_once()
                    command = run.call_args.args[0]
                    library_selection = [] if "--lib" in argv else ["--all"]
                    self.assertEqual(command, [*prefix, *library_selection, *expected])
                    env = run.call_args.kwargs["env"]
                    self.assertEqual(env["CARGO_INCREMENTAL"], "0")
                    self.assertEqual(
                        env["DYLINT_RUSTFLAGS"],
                        "-D argument-comment-mismatch -D uncommented-anonymous-literal-argument -A unknown_lints",
                    )

    def test_explicit_lint_levels_override_strict_defaults(self) -> None:
        """rustc applies the last level per lint, so an explicit level must not get a trailing `-D`."""
        env = {
            "DYLINT_RUSTFLAGS": "-A argument_comment_mismatch -A uncommented-anonymous-literal-argument",
            "CARGO_INCREMENTAL": "1",
        }
        wrapper_common.set_default_lint_env(env)
        self.assertEqual(
            env,
            {
                "DYLINT_RUSTFLAGS": "-A argument_comment_mismatch -A uncommented-anonymous-literal-argument -A unknown_lints",
                "CARGO_INCREMENTAL": "1",
            },
        )


if __name__ == "__main__":
    unittest.main()
