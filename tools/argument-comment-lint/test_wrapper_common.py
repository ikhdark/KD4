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
    def test_wrappers_forward_options_environment_and_linter_exit_status(self) -> None:
        """Both CLI entrypoints must deliver the same options to the real exec boundary."""
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = str(root / "codex-rs" / "Cargo.toml")
            custom_manifest = str(root / "custom" / "Cargo.toml")
            bin_dir = root / "package" / "bin"
            library_dir = root / "package" / "lib"
            bin_dir.mkdir(parents=True)
            library_dir.mkdir()
            packaged_entrypoint = bin_dir / "entrypoint"
            cargo_dylint = bin_dir / "cargo-dylint.exe"
            cargo_dylint.write_text("")
            library = library_dir / "lint@stable.dll"
            library.write_text("")
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
                    ],
                    [
                        "--manifest-path",
                        custom_manifest,
                        "--workspace",
                        "--no-deps",
                        "--",
                        "--bins",
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
                    ],
                ),
            ]
            for filename in ("run.py", "run-prebuilt-linter.py"):
                module = load_entrypoint(filename)
                prefix = (
                    [
                        "cargo",
                        "dylint",
                        "--path",
                        str(root / "tools" / "argument-comment-lint"),
                    ]
                    if filename == "run.py"
                    else [str(cargo_dylint), "dylint", "--lib-path", str(library)]
                )
                for argv, expected in cases:
                    with self.subTest(wrapper=filename, argv=argv):
                        with (
                            mock.patch.object(module, "repo_root", return_value=root),
                            mock.patch.object(sys, "argv", [filename, *argv]),
                            mock.patch.dict(
                                os.environ,
                                {"CODEX_ARGUMENT_COMMENT_LINT_SKIP_RUSTUP_SHIMS": "1"},
                                clear=True,
                            ),
                            mock.patch.object(
                                wrapper_common,
                                "require_command",
                                side_effect=lambda name, *args: name,
                            ),
                            mock.patch.object(
                                wrapper_common,
                                "run_capture",
                                side_effect=lambda command, **kwargs: (
                                    wrapper_common.TOOLCHAIN_CHANNEL
                                    if command[0] == "rustup"
                                    else str(packaged_entrypoint)
                                ),
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
                        self.assertEqual(
                            command, [*prefix, *library_selection, *expected]
                        )
                        env = run.call_args.kwargs["env"]
                        self.assertEqual(env["CARGO_INCREMENTAL"], "0")
                        self.assertEqual(
                            env["DYLINT_RUSTFLAGS"],
                            "-D argument-comment-mismatch -D uncommented-anonymous-literal-argument -A unknown_lints",
                        )


if __name__ == "__main__":
    unittest.main()
