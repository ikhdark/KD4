#!/usr/bin/env python3

import hashlib
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import dotslash
from codex_package.dotslash import DotSlashArtifact
from codex_package.targets import TARGET_SPECS


class DotSlashCacheStampTest(unittest.TestCase):
    def tearDown(self) -> None:
        dotslash.clear_runtime_caches()

    def test_load_manifest_reuses_parsed_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest = Path(temp_dir) / "artifact"
            manifest.write_text(
                '#!/usr/bin/env dotslash\n{"name": "first", "platforms": {}}\n',
                encoding="utf-8",
            )

            first = dotslash.load_manifest(manifest)
            manifest.write_text(
                '{"name": "second", "platforms": {}}\n', encoding="utf-8"
            )
            second = dotslash.load_manifest(manifest)

            self.assertEqual(first["name"], "first")
            self.assertIs(first, second)

    def test_clear_runtime_caches_clears_manifest_cache(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest = Path(temp_dir) / "artifact"
            manifest.write_text(
                '#!/usr/bin/env dotslash\n{"name": "first", "platforms": {}}\n',
                encoding="utf-8",
            )

            first = dotslash.load_manifest(manifest)
            manifest.write_text(
                '{"name": "second", "platforms": {}}\n', encoding="utf-8"
            )
            dotslash.clear_runtime_caches()
            second = dotslash.load_manifest(manifest)

            self.assertEqual(first["name"], "first")
            self.assertEqual(second["name"], "second")

    def test_verified_archive_stamp_skips_warm_cache_hash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive_path = Path(temp_dir) / "rg.zip"
            content = b"archive"
            archive_path.write_bytes(content)
            artifact = DotSlashArtifact(
                size=len(content),
                digest=hashlib.sha256(content).hexdigest(),
                archive_format="zip",
                archive_member="rg",
                url="https://example.test/rg.zip",
            )
            dotslash.verify_archive(archive_path, artifact, "rg")

            with mock.patch.object(
                dotslash,
                "verify_archive",
                side_effect=AssertionError("warm cache should use stamp"),
            ):
                self.assertTrue(dotslash.archive_is_valid(archive_path, artifact, "rg"))

    def test_invalid_cached_archive_is_removed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive_path = Path(temp_dir) / "rg.zip"
            archive_path.write_bytes(b"wrong")
            artifact = DotSlashArtifact(
                size=3,
                digest="0" * 64,
                archive_format="zip",
                archive_member="rg",
                url="https://example.test/rg.zip",
            )

            self.assertFalse(dotslash.archive_is_valid(archive_path, artifact, "rg"))
            self.assertFalse(archive_path.exists())

    def test_extracted_member_stamp_validates_warm_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            dest = Path(temp_dir) / "rg"
            dest.write_text("rg", encoding="utf-8")
            artifact = DotSlashArtifact(
                size=3,
                digest="0" * 64,
                archive_format="zip",
                archive_member="rg",
                url="https://example.test/rg.zip",
            )
            dotslash.write_extracted_member_stamp(dest, artifact)

            self.assertTrue(dotslash.extracted_member_is_valid(dest, artifact))

    def test_fetch_uses_extracted_stamp_before_archive_validation(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            manifest = root / "rg"
            digest = "0" * 64
            manifest.write_text(
                json.dumps(
                    {
                        "platforms": {
                            spec.dotslash_platform: {
                                "providers": [{"url": "https://example.test/rg.zip"}],
                                "hash": "sha256",
                                "size": 10,
                                "digest": digest,
                                "format": "zip",
                                "path": spec.rg_name,
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            artifact = DotSlashArtifact(
                size=10,
                digest=digest,
                archive_format="zip",
                archive_member=spec.rg_name,
                url="https://example.test/rg.zip",
            )
            dest = root / "cache" / "rg-cache" / spec.rg_name
            dest.parent.mkdir(parents=True)
            dest.write_text("rg", encoding="utf-8")
            dotslash.write_extracted_member_stamp(dest, artifact)

            with (
                mock.patch.object(
                    dotslash, "default_cache_root", return_value=root / "cache"
                ),
                mock.patch.object(
                    dotslash,
                    "archive_is_valid",
                    side_effect=AssertionError(
                        "warm extracted member should skip archive"
                    ),
                ),
            ):
                actual = dotslash.fetch_dotslash_executable(
                    spec,
                    manifest_path=manifest,
                    artifact_label="ripgrep",
                    cache_key="rg-cache",
                    dest_name=spec.rg_name,
                )

            self.assertEqual(actual, dest)

    def test_fetch_revalidates_cached_result_before_reusing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            manifest = root / "rg"
            digest = "0" * 64
            manifest.write_text(
                json.dumps(
                    {
                        "platforms": {
                            spec.dotslash_platform: {
                                "providers": [{"url": "https://example.test/rg.zip"}],
                                "hash": "sha256",
                                "size": 10,
                                "digest": digest,
                                "format": "zip",
                                "path": spec.rg_name,
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            artifact = DotSlashArtifact(
                size=10,
                digest=digest,
                archive_format="zip",
                archive_member=spec.rg_name,
                url="https://example.test/rg.zip",
            )
            dest = root / "cache" / "rg-cache" / spec.rg_name
            dest.parent.mkdir(parents=True)
            dest.write_text("rg", encoding="utf-8")
            dotslash.write_extracted_member_stamp(dest, artifact)

            with mock.patch.object(
                dotslash, "default_cache_root", return_value=root / "cache"
            ):
                first = dotslash.fetch_dotslash_executable(
                    spec,
                    manifest_path=manifest,
                    artifact_label="ripgrep",
                    cache_key="rg-cache",
                    dest_name=spec.rg_name,
                )
                dest.unlink()

                def fake_extract(
                    _archive_path: Path,
                    artifact: DotSlashArtifact,
                    extract_dest: Path,
                    _artifact_label: str,
                ) -> None:
                    extract_dest.write_text("rg", encoding="utf-8")

                with (
                    mock.patch.object(dotslash, "archive_is_valid", return_value=True),
                    mock.patch.object(
                        dotslash,
                        "extract_archive_member",
                        side_effect=fake_extract,
                    ) as extract,
                ):
                    second = dotslash.fetch_dotslash_executable(
                        spec,
                        manifest_path=manifest,
                        artifact_label="ripgrep",
                        cache_key="rg-cache",
                        dest_name=spec.rg_name,
                    )

                with mock.patch.object(
                    dotslash,
                    "artifact_for_target",
                    side_effect=AssertionError(
                        "valid cached fetch should skip manifest resolution"
                    ),
                ):
                    third = dotslash.fetch_dotslash_executable(
                        spec,
                        manifest_path=manifest,
                        artifact_label="ripgrep",
                        cache_key="rg-cache",
                        dest_name=spec.rg_name,
                    )

            self.assertEqual(first, dest)
            self.assertEqual(second, dest)
            self.assertEqual(third, dest)
            extract.assert_called_once()

    def test_json_stamp_reads_are_memoized_until_file_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            stamp = Path(temp_dir) / "stamp.json"
            stamp.write_text('{"ok": true}\n', encoding="utf-8")

            with mock.patch.object(dotslash.json, "loads", wraps=json.loads) as loads:
                self.assertEqual(dotslash.read_json_stamp(stamp), {"ok": True})
                self.assertEqual(dotslash.read_json_stamp(stamp), {"ok": True})

            self.assertEqual(loads.call_count, 1)

    def test_manifest_rejects_unsafe_archive_member(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest = Path(temp_dir) / "artifact"
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            manifest.write_text(
                json.dumps(
                    {
                        "platforms": {
                            spec.dotslash_platform: {
                                "providers": [{"url": "https://example.test/rg.zip"}],
                                "hash": "sha256",
                                "size": 1,
                                "digest": "0" * 64,
                                "format": "zip",
                                "path": "../rg.exe",
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(RuntimeError, "Unsafe.*archive member"):
                dotslash.artifact_for_target(
                    spec,
                    manifest,
                    artifact_label="ripgrep",
                )

    def test_manifest_rejects_invalid_digest_and_format(self) -> None:
        spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
        for field, value, expected in [
            ("digest", "not-a-digest", "sha256 digest"),
            ("format", "tar.xz", "archive format"),
        ]:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as temp_dir:
                platform_info = {
                    "providers": [{"url": "https://example.test/rg.zip"}],
                    "hash": "sha256",
                    "size": 1,
                    "digest": "0" * 64,
                    "format": "zip",
                    "path": "rg.exe",
                }
                platform_info[field] = value
                manifest = Path(temp_dir) / "artifact"
                manifest.write_text(
                    json.dumps({"platforms": {spec.dotslash_platform: platform_info}}),
                    encoding="utf-8",
                )

                with self.assertRaisesRegex(RuntimeError, expected):
                    dotslash.artifact_for_target(
                        spec,
                        manifest,
                        artifact_label="ripgrep",
                    )

    def test_failed_extraction_preserves_existing_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            archive_path = root / "artifact.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                member = tarfile.TarInfo("bin/rg")
                member.type = tarfile.DIRTYPE
                archive.addfile(member, io.BytesIO())
            dest = root / "rg"
            dest.write_bytes(b"previous")
            artifact = DotSlashArtifact(
                size=archive_path.stat().st_size,
                digest=hashlib.sha256(archive_path.read_bytes()).hexdigest(),
                archive_format="tar.gz",
                archive_member="bin/rg",
                url=archive_path.as_uri(),
            )

            with self.assertRaisesRegex(RuntimeError, "not a regular file"):
                dotslash.extract_archive_member(
                    archive_path,
                    artifact,
                    dest,
                    "ripgrep",
                )

            self.assertEqual(dest.read_bytes(), b"previous")
            self.assertEqual(list(root.glob("rg.*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
