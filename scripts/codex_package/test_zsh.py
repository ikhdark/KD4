#!/usr/bin/env python3

from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import zsh
from codex_package.dotslash import artifact_for_target
from codex_package.targets import TARGET_SPECS


class ZshResolverTest(unittest.TestCase):
    def tearDown(self) -> None:
        zsh.resolve_zsh_bin_for_target.cache_clear()

    def test_windows_targets_skip_manifest_resolution(self) -> None:
        with mock.patch.object(
            zsh,
            "fetch_dotslash_executable",
            side_effect=AssertionError("windows packages should not resolve zsh"),
        ):
            self.assertIsNone(
                zsh.resolve_zsh_bin(TARGET_SPECS["x86_64-pc-windows-msvc"])
            )

    def test_unix_targets_use_zsh_manifest(self) -> None:
        expected = Path("zsh")
        with mock.patch.object(
            zsh,
            "fetch_dotslash_executable",
            return_value=expected,
        ) as fetch:
            actual = zsh.resolve_zsh_bin(TARGET_SPECS["x86_64-unknown-linux-musl"])

        self.assertEqual(actual, expected)
        fetch.assert_called_once()

    def test_explicit_zsh_bin_uses_standard_executable_validation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            explicit = Path(temp_dir) / "zsh"
            resolved = explicit.resolve()

            with mock.patch.object(
                zsh,
                "resolve_input_path",
                return_value=resolved,
            ) as resolve_input_path:
                actual = zsh.resolve_zsh_bin(
                    TARGET_SPECS["x86_64-unknown-linux-musl"],
                    explicit,
                )

        self.assertEqual(actual, resolved)
        resolve_input_path.assert_called_once_with(
            explicit,
            "zsh executable",
            "--zsh-bin",
        )

    def test_checked_in_zsh_manifest_covers_supported_zsh_targets(self) -> None:
        for target, spec in TARGET_SPECS.items():
            with self.subTest(target=target):
                if not zsh.supports_zsh(spec):
                    self.assertIsNone(zsh.resolve_zsh_bin(spec))
                    continue

                artifact = artifact_for_target(
                    spec,
                    zsh.ZSH_MANIFEST,
                    artifact_label=zsh.ZSH_ARTIFACT_LABEL,
                )
                self.assertEqual(Path(artifact.archive_member).name, zsh.ZSH_DEST_NAME)
                self.assertEqual(artifact.archive_format, "tar.gz")


if __name__ == "__main__":
    unittest.main()
