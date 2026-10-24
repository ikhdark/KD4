"""Focused failure injection for package publication and vendor installation.

These exercise the real owners with temporary files and stdlib mocks only;
no Cargo, gh, npm, compressor, or installed Codex process is started.
"""

from contextlib import nullcontext
import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from scripts.codex_package import archive
from scripts.codex_package import stage_npm_archives as vendor_install
from scripts.codex_package import stage_npm_packages as staging


TARGET = "x86_64-pc-windows-msvc"


class PackagePublicationBoundaryTest(unittest.TestCase):
    def test_digest_mismatch_never_extracts_or_replaces_cached_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            artifacts = Path(directory) / "artifacts"
            existing = artifacts / TARGET
            existing.mkdir(parents=True)
            sentinel = existing / "keep.txt"
            sentinel.write_bytes(b"previous cache")
            trusted = b"trusted"
            downloaded = b"changed"  # Same length: size alone cannot authorize it.
            artifact = staging.WorkflowArtifact(
                name=TARGET,
                size_in_bytes=len(trusted),
                artifact_id=42,
                archive_sha256=hashlib.sha256(trusted).hexdigest(),
            )

            def download(_command, **kwargs):
                kwargs["stdout"].write(downloaded)

            with (
                mock.patch.object(staging, "run_owned", side_effect=download) as fetch,
                mock.patch.object(
                    staging, "exclusive_file_lock", return_value=nullcontext()
                ),
                mock.patch.object(staging, "extract_artifact_zip") as extract,
                mock.patch.object(staging, "replace_directory") as replace,
            ):
                with self.assertRaisesRegex(RuntimeError, "sha256 mismatch"):
                    staging.download_single_artifact("7", "fixture/fork", artifacts, artifact)

            fetch.assert_called_once()
            extract.assert_not_called()
            replace.assert_not_called()
            self.assertEqual(sentinel.read_bytes(), b"previous cache")
            self.assertEqual(set(artifacts.iterdir()), {existing})

    def test_failed_archive_writer_keeps_existing_forced_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / "package"
            package.mkdir()
            output = root / "package.tar.gz"
            output.write_bytes(b"previous archive")

            def fail_writer(_package, temporary, **_kwargs):
                temporary.write_bytes(b"partial archive")
                raise OSError("fixture writer failed")

            with (
                mock.patch.object(archive, "write_tar_archive", side_effect=fail_writer),
                mock.patch.object(archive, "activate_archive") as activate,
            ):
                with self.assertRaisesRegex(OSError, "fixture writer failed"):
                    archive.write_archive(package, output, force=True)

            activate.assert_not_called()
            self.assertEqual(output.read_bytes(), b"previous archive")
            self.assertEqual(set(root.iterdir()), {package, output})

    def test_late_archive_collision_preserves_both_writers_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            staged = root / "staged.tmp"
            output = root / "package.zip"
            staged.write_bytes(b"our staged archive")
            output.write_bytes(b"concurrent winner")
            with mock.patch.object(archive.os, "link", side_effect=FileExistsError) as link:
                with self.assertRaisesRegex(RuntimeError, "already exists"):
                    archive.activate_archive(staged, output, force=False)

            link.assert_called_once_with(staged, output)
            self.assertEqual(staged.read_bytes(), b"our staged archive")
            self.assertEqual(output.read_bytes(), b"concurrent winner")

    def test_failed_vendor_activation_restores_previous_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            artifact_dir = root / "artifacts" / TARGET
            artifact_dir.mkdir(parents=True)
            (artifact_dir / f"codex-package-{TARGET}.tar.gz").write_bytes(b"fixture")
            vendor = root / "vendor"
            destination = vendor / TARGET
            destination.mkdir(parents=True)
            (destination / "keep.txt").write_bytes(b"previous installation")
            real_replace = Path.replace

            def extract(_archive, temporary):
                (temporary / "candidate.txt").write_bytes(b"candidate installation")

            def replace(source, target):
                if source.name.endswith(".tmp") and Path(target) == destination:
                    raise OSError("fixture activation denied")
                return real_replace(source, target)

            with (
                mock.patch.object(vendor_install, "extract_tar_data", side_effect=extract),
                mock.patch.object(Path, "replace", autospec=True, side_effect=replace),
            ):
                with self.assertRaisesRegex(OSError, "fixture activation denied"):
                    vendor_install.install_single_codex_package_archive(
                        root / "artifacts", vendor, TARGET
                    )

            self.assertEqual((destination / "keep.txt").read_bytes(), b"previous installation")
            self.assertEqual(set(destination.iterdir()), {destination / "keep.txt"})
            self.assertEqual(set(vendor.iterdir()), {destination})


if __name__ == "__main__":
    unittest.main()
