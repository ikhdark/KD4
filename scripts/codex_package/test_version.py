#!/usr/bin/env python3

from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import version


class VersionDiscoveryTest(unittest.TestCase):
    def tearDown(self) -> None:
        version.read_workspace_version.cache_clear()

    def test_reads_workspace_package_version_from_custom_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cargo_toml = Path(temp_dir) / "Cargo.toml"
            cargo_toml.write_text(
                "\n".join(
                    [
                        "[package]",
                        'version = "9.9.9"',
                        "",
                        "[workspace.package]",
                        'name = "codex"',
                        'version = "1.2.3-alpha.4"',
                        "",
                        "[workspace.dependencies]",
                        'serde = "1"',
                    ]
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                version.read_workspace_version(cargo_toml),
                "1.2.3-alpha.4",
            )

    def test_reads_workspace_package_version_with_toml_comments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cargo_toml = Path(temp_dir) / "Cargo.toml"
            cargo_toml.write_text(
                "\n".join(
                    [
                        "[workspace.package]",
                        'version = "1.2.3" # package version',
                    ]
                ),
                encoding="utf-8",
            )

            self.assertEqual(version.read_workspace_version(cargo_toml), "1.2.3")

    def test_raises_when_workspace_package_version_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cargo_toml = Path(temp_dir) / "Cargo.toml"
            cargo_toml.write_text(
                '[workspace.package]\nname = "codex"\n',
                encoding="utf-8",
            )

            with self.assertRaisesRegex(RuntimeError, "workspace.package"):
                version.read_workspace_version(cargo_toml)

    def test_caches_manifest_reads_by_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cargo_toml = Path(temp_dir) / "Cargo.toml"
            cargo_toml.write_text(
                '[workspace.package]\nversion = "1.0.0"\n',
                encoding="utf-8",
            )

            first = version.read_workspace_version(cargo_toml)
            cargo_toml.write_text(
                '[workspace.package]\nversion = "2.0.0"\n',
                encoding="utf-8",
            )
            second = version.read_workspace_version(cargo_toml)

            self.assertEqual((first, second), ("1.0.0", "1.0.0"))

    def test_default_manifest_uses_repo_root(self) -> None:
        expected = Path("repo") / "codex-rs" / "Cargo.toml"

        with (
            mock.patch.object(version, "REPO_ROOT", Path("repo")),
            mock.patch.object(
                version,
                "_read_workspace_version_uncached",
                return_value="3.4.5",
            ) as read_uncached,
        ):
            version.read_workspace_version()

        read_uncached.assert_called_once_with(expected)

    def test_parse_version_assignment_requires_exact_key_and_quoted_value(self) -> None:
        self.assertEqual(version.parse_version_assignment('version = "1.2.3"'), "1.2.3")
        self.assertEqual(
            version.parse_version_assignment('version = "1.2.3" # comment'),
            "1.2.3",
        )
        self.assertIsNone(version.parse_version_assignment('package.version = "1.2.3"'))
        self.assertIsNone(version.parse_version_assignment("version = 1.2.3"))
        self.assertIsNone(version.parse_version_assignment('version = "1.2.3" extra'))


if __name__ == "__main__":
    unittest.main()
