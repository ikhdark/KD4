#!/usr/bin/env python3

from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import ripgrep
from codex_package.dotslash import artifact_for_target
from codex_package.targets import TARGET_SPECS


class RipgrepResolverTest(unittest.TestCase):
    def test_explicit_rg_bin_skips_canonicalization(self) -> None:
        explicit = Path("local-rg")
        expected = Path("local-rg")

        with mock.patch.object(
            ripgrep,
            "resolve_input_path",
            return_value=expected,
        ) as resolve_input_path:
            actual = ripgrep.resolve_rg_bin(
                TARGET_SPECS["x86_64-apple-darwin"],
                explicit,
            )

        self.assertEqual(actual, expected)
        resolve_input_path.assert_called_once_with(
            explicit,
            "ripgrep executable",
            "--rg-bin",
            canonicalize=False,
        )

    def test_fetch_rg_uses_target_specific_cache_and_filename(self) -> None:
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
        expected = Path("rg.exe")

        with mock.patch.object(
            ripgrep,
            "fetch_dotslash_executable",
            return_value=expected,
        ) as fetch:
            actual = ripgrep.fetch_rg(spec)

        self.assertEqual(actual, expected)
        fetch.assert_called_once_with(
            spec,
            manifest_path=ripgrep.RG_MANIFEST,
            artifact_label="ripgrep",
            cache_key="x86_64-pc-windows-msvc-rg",
            dest_name="rg.exe",
        )

    def test_fetch_rg_rejects_missing_artifact(self) -> None:
        spec = TARGET_SPECS["x86_64-apple-darwin"]

        with (
            mock.patch.object(
                ripgrep,
                "fetch_dotslash_executable",
                return_value=None,
            ),
            self.assertRaises(AssertionError) as cm,
        ):
            ripgrep.fetch_rg(spec)

        self.assertIn("ripgrep is required", str(cm.exception))

    def test_checked_in_rg_manifest_covers_package_targets(self) -> None:
        for target, spec in TARGET_SPECS.items():
            with self.subTest(target=target):
                artifact = artifact_for_target(
                    spec,
                    ripgrep.RG_MANIFEST,
                    artifact_label="ripgrep",
                )

                self.assertEqual(Path(artifact.archive_member).name, spec.rg_name)
                if spec.is_windows:
                    self.assertEqual(artifact.archive_format, "zip")
                else:
                    self.assertEqual(artifact.archive_format, "tar.gz")


if __name__ == "__main__":
    unittest.main()
