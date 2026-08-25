#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from typing import Never, get_type_hints

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
    def test_defaults_to_workspace_and_all_targets(self) -> None:
        manifest = Path("/repo/codex-rs/Cargo.toml")
        parsed = wrapper_common.parse_wrapper_args([])
        final_args = wrapper_common.build_final_args(parsed, manifest)

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                str(manifest),
                "--workspace",
                "--no-deps",
                "--",
                "--all-targets",
            ],
        )

    def test_forwarded_cargo_args_keep_single_separator(self) -> None:
        manifest = Path("/repo/codex-rs/Cargo.toml")
        parsed = wrapper_common.parse_wrapper_args(
            ["-p", "codex-core", "--", "--tests"]
        )
        final_args = wrapper_common.build_final_args(parsed, manifest)

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                str(manifest),
                "--no-deps",
                "-p",
                "codex-core",
                "--",
                "--tests",
            ],
        )

    def test_fix_does_not_add_all_targets(self) -> None:
        manifest = Path("/repo/codex-rs/Cargo.toml")
        parsed = wrapper_common.parse_wrapper_args(["--fix", "-p", "codex-core"])
        final_args = wrapper_common.build_final_args(parsed, manifest)

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                str(manifest),
                "--no-deps",
                "--fix",
                "-p",
                "codex-core",
            ],
        )

    def test_explicit_manifest_and_workspace_are_preserved(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--workspace",
                "--no-deps",
                "--",
                "--bins",
            ]
        )
        final_args = wrapper_common.build_final_args(
            parsed, Path("/repo/codex-rs/Cargo.toml")
        )

        self.assertEqual(
            final_args,
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--workspace",
                "--no-deps",
                "--",
                "--bins",
            ],
        )

    def test_explicit_package_manifest_does_not_force_workspace(self) -> None:
        parsed = wrapper_common.parse_wrapper_args(
            [
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
            ]
        )
        final_args = wrapper_common.build_final_args(
            parsed, Path("/repo/codex-rs/Cargo.toml")
        )

        self.assertEqual(
            final_args,
            [
                "--no-deps",
                "--manifest-path",
                "/tmp/custom/Cargo.toml",
                "--",
                "--all-targets",
            ],
        )

    def test_default_lint_env_promotes_both_strict_lints(self) -> None:
        env: dict[str, str] = {}

        wrapper_common.set_default_lint_env(env)

        self.assertEqual(
            env["DYLINT_RUSTFLAGS"],
            "-D argument-comment-mismatch "
            "-D uncommented-anonymous-literal-argument "
            "-A unknown_lints",
        )
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")

    def test_nonreturning_annotations_resolve(self) -> None:
        self.assertIs(get_type_hints(wrapper_common.die)["return"], Never)
        self.assertIs(get_type_hints(wrapper_common.exec_command)["return"], Never)
        for filename in ("run.py", "run-prebuilt-linter.py"):
            with self.subTest(filename=filename):
                module = load_entrypoint(filename)
                self.assertIs(get_type_hints(module.main)["return"], Never)


if __name__ == "__main__":
    unittest.main()
