#!/usr/bin/env python3

from pathlib import Path
import io
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import archive
from codex_package.archive import resolve_zstd_command


class ResolveZstdCommandTest(unittest.TestCase):
    def test_accepts_explicit_zstd_with_matching_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            executable = Path(temp_dir) / "zstd"
            executable.write_bytes(b"pinned zstd")
            digest = __import__("hashlib").sha256(executable.read_bytes()).hexdigest()

            self.assertEqual(
                resolve_zstd_command(
                    environ={
                        archive.ZSTD_PATH_ENV: str(executable),
                        archive.ZSTD_SHA256_ENV: digest,
                    }
                ),
                [str(executable.resolve())],
            )

    def test_rejects_unpinned_zstd(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "pinned compressor"):
            resolve_zstd_command(environ={})

    def test_rejects_zstd_digest_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            executable = Path(temp_dir) / "zstd"
            executable.write_bytes(b"unexpected")
            with self.assertRaisesRegex(RuntimeError, "digest mismatch"):
                resolve_zstd_command(
                    environ={
                        archive.ZSTD_PATH_ENV: str(executable),
                        archive.ZSTD_SHA256_ENV: "0" * 64,
                    }
                )

    def test_tar_zst_streams_tar_to_zstd_stdin(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = root / "package.tar.zst"
            entries = [package_dir / "codex-package.json"]

            process = mock.Mock()
            process.stdin = io.BytesIO()
            process.wait.return_value = 0

            with (
                mock.patch.object(
                    archive, "resolve_zstd_command", return_value=["zstd"]
                ),
                mock.patch.object(
                    archive.subprocess, "Popen", return_value=process
                ) as popen,
                mock.patch.object(archive, "write_tar_stream") as write_tar_stream,
            ):
                archive.write_tar_zst_archive(
                    package_dir,
                    output,
                    entries=entries,
                    compression="fast",
                )

            cmd = popen.call_args.args[0]
            self.assertEqual(cmd, ["zstd", "-T0", "-1", "-f", "-", "-o", str(output)])
            write_tar_stream.assert_called_once_with(
                package_dir,
                process.stdin,
                entries=entries,
            )
            process.kill.assert_not_called()

    def test_tar_zst_none_uses_zstd_level_zero(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = root / "package.tar.zst"
            process = mock.Mock()
            process.stdin = io.BytesIO()
            process.wait.return_value = 0

            with (
                mock.patch.object(
                    archive, "resolve_zstd_command", return_value=["zstd"]
                ),
                mock.patch.object(
                    archive.subprocess, "Popen", return_value=process
                ) as popen,
                mock.patch.object(archive, "write_tar_stream"),
            ):
                archive.write_tar_zst_archive(
                    package_dir,
                    output,
                    entries=[],
                    compression="none",
                )

            cmd = popen.call_args.args[0]
            self.assertEqual(cmd, ["zstd", "-T0", "-0", "-f", "-", "-o", str(output)])


class WriteArchiveSafetyTest(unittest.TestCase):
    def test_rejects_archive_output_that_resolves_inside_package_dir(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = package_dir / ".." / "package" / "nested.zip"

            with self.assertRaisesRegex(
                RuntimeError,
                "Archive output must be outside the package directory",
            ):
                archive.write_archive(package_dir, output, force=True)

    def test_force_replaces_resolved_archive_output(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            (package_dir / "codex-package.json").write_text("{}", encoding="utf-8")
            output = root / "archives" / ".." / "package.zip"
            resolved_output = output.resolve()
            resolved_output.write_text("stale", encoding="utf-8")

            archive.write_archive(
                package_dir,
                output,
                force=True,
                entries=[package_dir / "codex-package.json"],
                compression="none",
            )

            with zipfile.ZipFile(resolved_output) as zip_archive:
                self.assertEqual(zip_archive.namelist(), ["codex-package.json"])

    def test_failed_force_write_preserves_existing_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = root / "package.zip"
            output.write_bytes(b"previous archive")

            with (
                mock.patch.object(
                    archive,
                    "write_zip_archive",
                    side_effect=RuntimeError("write failed"),
                ),
                self.assertRaisesRegex(RuntimeError, "write failed"),
            ):
                archive.write_archive(package_dir, output, force=True)

            self.assertEqual(output.read_bytes(), b"previous archive")
            self.assertEqual(list(root.glob("package.zip.*.tmp")), [])

    def test_non_force_write_preserves_concurrently_created_archive(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = root / "package.zip"

            def write_then_race(
                _package_dir: Path,
                staged_path: Path,
                **_kwargs: object,
            ) -> None:
                staged_path.write_bytes(b"new archive")
                output.write_bytes(b"concurrent archive")

            with (
                mock.patch.object(
                    archive,
                    "write_zip_archive",
                    side_effect=write_then_race,
                ),
                self.assertRaisesRegex(RuntimeError, "already exists"),
            ):
                archive.write_archive(package_dir, output, force=False)

            self.assertEqual(output.read_bytes(), b"concurrent archive")
            self.assertEqual(list(root.glob("package.zip.*.tmp")), [])

    def test_incompatible_compression_is_rejected_before_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            output = root / "package.tar.gz"
            output.write_bytes(b"previous archive")

            with self.assertRaisesRegex(RuntimeError, "compression 'none'"):
                archive.write_archive(
                    package_dir,
                    output,
                    force=True,
                    compression="none",
                )

            self.assertEqual(output.read_bytes(), b"previous archive")


class ArchiveMemberNameTest(unittest.TestCase):
    def test_tar_gz_and_zip_are_byte_reproducible(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            package_dir.mkdir()
            payload = package_dir / "payload.bin"
            payload.write_bytes(b"stable")
            entries = [payload]
            first_tgz = root / "first.tar.gz"
            second_tgz = root / "second.tar.gz"
            first_zip = root / "first.zip"
            second_zip = root / "second.zip"

            archive.write_tar_archive(
                package_dir, first_tgz, mode="w:gz", entries=entries
            )
            archive.write_tar_archive(
                package_dir, second_tgz, mode="w:gz", entries=entries
            )
            archive.write_zip_archive(package_dir, first_zip, entries=entries)
            archive.write_zip_archive(package_dir, second_zip, entries=entries)

            self.assertEqual(first_tgz.read_bytes(), second_tgz.read_bytes())
            self.assertEqual(first_zip.read_bytes(), second_zip.read_bytes())

    def test_package_entries_exclude_unmanaged_reuse_contents(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            package_dir = Path(temp_dir) / "package"
            (package_dir / "bin").mkdir(parents=True)
            (package_dir / "bin" / "codex.exe").write_bytes(b"codex")
            (package_dir / "codex-resources").mkdir()
            (package_dir / "codex-path").mkdir()
            (package_dir / "codex-package.json").write_text("{}", encoding="utf-8")
            unmanaged = package_dir / "custom-cache" / "secret"
            unmanaged.parent.mkdir()
            unmanaged.write_bytes(b"do not archive")

            members = {
                path.relative_to(package_dir).as_posix()
                for path in archive.package_entries(package_dir)
            }

            self.assertIn("bin/codex.exe", members)
            self.assertIn("codex-package.json", members)
            self.assertNotIn("custom-cache", members)
            self.assertNotIn("custom-cache/secret", members)

    def test_zip_archive_uses_posix_member_names(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            nested_dir = package_dir / "nested"
            nested_dir.mkdir(parents=True)
            nested_file = nested_dir / "codex.exe"
            nested_file.write_text("binary", encoding="utf-8")
            output = root / "package.zip"

            archive.write_zip_archive(
                package_dir,
                output,
                entries=[nested_dir, nested_file],
                compression="none",
            )

            with zipfile.ZipFile(output) as zip_archive:
                self.assertEqual(
                    zip_archive.namelist(), ["nested/", "nested/codex.exe"]
                )

    def test_tar_archive_uses_posix_member_names(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            package_dir = root / "package"
            nested_dir = package_dir / "nested"
            nested_dir.mkdir(parents=True)
            nested_file = nested_dir / "codex.exe"
            nested_file.write_text("binary", encoding="utf-8")
            output = root / "package.tar"

            archive.write_tar_archive(
                package_dir,
                output,
                mode="w",
                entries=[nested_dir, nested_file],
                compression="none",
            )

            with tarfile.open(output) as tar_archive:
                self.assertEqual(tar_archive.getnames(), ["nested", "nested/codex.exe"])


if __name__ == "__main__":
    unittest.main()
