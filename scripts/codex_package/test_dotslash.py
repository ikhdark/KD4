#!/usr/bin/env python3

import hashlib
import http.server
import io
import json
import os
import zipfile
from pathlib import Path
import sys
import tarfile
import tempfile
import threading
import time
import unittest
from concurrent.futures import CancelledError
from contextlib import contextmanager
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import dotslash
from codex_package.dotslash import DotSlashArtifact
from codex_package.targets import TARGET_SPECS
from scripts.process_owner import operation


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
            dotslash.clear_runtime_caches()
            refreshed = dotslash.load_manifest(manifest)
            self.assertEqual(refreshed["name"], "second")
            self.assertIsNot(refreshed, first)

    def test_cached_archive_is_reverified(self) -> None:
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
                wraps=dotslash.verify_archive,
            ) as verify_archive:
                self.assertTrue(dotslash.archive_is_valid(archive_path, artifact, "rg"))
                verify_archive.assert_called_once_with(archive_path, artifact, "rg")

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

    def test_invalid_utf8_extracted_stamp_is_a_cache_miss(self) -> None:
        # A cache stamp is optional proof, not an input requirement. Corruption
        # must cause revalidation rather than block use of the source archive.
        with tempfile.TemporaryDirectory() as directory:
            dest = Path(directory) / "rg.exe"
            dest.write_bytes(b"previous executable")
            stamp = dotslash.extracted_member_stamp_path(dest)
            stamp.write_bytes(b"\xff")
            artifact = DotSlashArtifact(1, "0" * 64, "zip", "rg.exe", "file:///rg.zip")
            self.assertFalse(dotslash.extracted_member_is_valid(dest, artifact))
            self.assertEqual(dest.read_bytes(), b"previous executable")

    def test_same_size_corrupt_archive_is_rejected_and_removed(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            archive_path = Path(temp_dir) / "rg.zip"
            original = b"original archive bytes"
            archive_path.write_bytes(b"X" + original[1:])
            artifact = DotSlashArtifact(
                size=len(original),
                digest=hashlib.sha256(original).hexdigest(),
                archive_format="zip",
                archive_member="rg.exe",
                url=archive_path.as_uri(),
            )

            with self.assertRaisesRegex(RuntimeError, "sha256"):
                dotslash.verify_archive(archive_path, artifact, "rg")
            self.assertFalse(dotslash.archive_is_valid(archive_path, artifact, "rg"))
            self.assertFalse(archive_path.exists())
            self.assertFalse(dotslash.verified_archive_stamp_path(archive_path).exists())

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
            identity = hashlib.sha256(
                json.dumps(
                    [digest, "zip", spec.rg_name], separators=(",", ":")
                ).encode()
            ).hexdigest()
            dest = root / "cache" / "rg-cache" / identity / spec.rg_name
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
            identity = hashlib.sha256(
                json.dumps(
                    [digest, "zip", spec.rg_name], separators=(",", ":")
                ).encode()
            ).hexdigest()
            dest = root / "cache" / "rg-cache" / identity / spec.rg_name
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
                before = dest.stat()
                dest.write_bytes(b"xx")
                os.utime(dest, ns=(before.st_atime_ns, before.st_mtime_ns))

                def fake_extract(
                    _archive_path: Path,
                    artifact: DotSlashArtifact,
                    extract_dest: Path,
                    _artifact_label: str,
                ) -> None:
                    extract_dest.write_text("rg", encoding="utf-8")
                    dotslash.write_extracted_member_stamp(extract_dest, artifact)

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

    def test_concurrent_artifact_revisions_keep_their_own_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            manifests = []
            artifacts = []
            for label in ["A", "B"]:
                archive_path = root / f"{label}.zip"
                with zipfile.ZipFile(archive_path, "w") as zipped:
                    zipped.writestr("rg.exe", label.encode())
                artifact = DotSlashArtifact(
                    archive_path.stat().st_size,
                    hashlib.sha256(archive_path.read_bytes()).hexdigest(),
                    "zip",
                    "rg.exe",
                    archive_path.as_uri(),
                )
                artifacts.append(artifact)
                manifest = root / f"{label}.json"
                manifest.write_text(
                    json.dumps(
                        {
                            "platforms": {
                                spec.dotslash_platform: {
                                    "providers": [{"url": artifact.url}],
                                    "hash": "sha256",
                                    "size": artifact.size,
                                    "digest": artifact.digest,
                                    "format": "zip",
                                    "path": "rg.exe",
                                }
                            }
                        }
                    )
                )
                manifests.append(manifest)

            def fetch(manifest):
                return dotslash.fetch_dotslash_executable(
                    spec,
                    manifest_path=manifest,
                    artifact_label="rg",
                    cache_key="shared",
                    dest_name="rg.exe",
                )

            original_extract = dotslash.extract_archive_member
            second = []

            def interleaved_extract(path, artifact, dest, label):
                original_extract(path, artifact, dest, label)
                if artifact == artifacts[0]:
                    second.append(fetch(manifests[1]))

            with (
                mock.patch.object(
                    dotslash, "default_cache_root", return_value=root / "cache"
                ),
                mock.patch.object(
                    dotslash, "extract_archive_member", side_effect=interleaved_extract
                ),
            ):
                first = fetch(manifests[0])
            self.assertNotEqual(first, second[0])
            self.assertEqual(first.read_bytes(), b"A")
            self.assertEqual(second[0].read_bytes(), b"B")
            self.assertTrue(dotslash.extracted_member_is_valid(first, artifacts[0]))
            self.assertFalse(
                dotslash.extracted_member_is_valid(second[0], artifacts[0])
            )

    def test_stamp_publication_failure_preserves_previous_json(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            stamp = Path(temp_dir) / "stamp.json"
            stamp.write_text('{"old": true}')
            with mock.patch.object(
                Path, "replace", side_effect=OSError("publish failed")
            ):
                with self.assertRaisesRegex(OSError, "publish failed"):
                    dotslash.write_json_stamp(stamp, {"new": True})
            self.assertEqual(dotslash.read_json_stamp(stamp), {"old": True})
            self.assertEqual(list(stamp.parent.glob("*.tmp")), [])

    def test_json_stamp_reads_are_memoized_until_file_changes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            stamp = Path(temp_dir) / "stamp.json"
            stamp.write_text('{"ok": true}\n', encoding="utf-8")

            with mock.patch.object(dotslash.json, "loads", wraps=json.loads) as loads:
                self.assertEqual(dotslash.read_json_stamp(stamp), {"ok": True})
                self.assertEqual(dotslash.read_json_stamp(stamp), {"ok": True})

                self.assertEqual(loads.call_count, 1)
                stamp.write_text('{"changed": true}\n', encoding="utf-8")
                self.assertEqual(dotslash.read_json_stamp(stamp), {"changed": True})
                self.assertEqual(loads.call_count, 2)

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


@contextmanager
def serve_archive(body: bytes, *, header_delay=0, chunk_delay=0, status=200):
    stop = threading.Event()
    requested = threading.Event()
    body_started = threading.Event()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            requested.set()
            if stop.wait(header_delay):
                return
            try:
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                for offset in range(0, len(body), 8192):
                    if stop.wait(chunk_delay):
                        return
                    self.wfile.write(body[offset : offset + 8192])
                    self.wfile.flush()
                    body_started.set()
            except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = False
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}/rg.zip", requested, body_started
    finally:
        stop.set()
        server.shutdown()
        server.server_close()
        thread.join()


class DotSlashDownloadTest(unittest.TestCase):
    def tearDown(self) -> None:
        dotslash.clear_runtime_caches()

    def test_delayed_headers_within_transfer_budget_succeed(self) -> None:
        body = b"archive payload" * 10000
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            serve_archive(body, header_delay=5.6) as (url, _, _),
        ):
            archive = Path(temp_dir) / "rg.zip"
            archive.write_bytes(b"previous")
            dotslash.download_archive(url, archive)
            self.assertEqual(archive.read_bytes(), body)
            self.assertEqual(list(Path(temp_dir).glob("*.tmp")), [])

    def test_transfer_deadline_stops_progressing_body_and_preserves_output(
        self,
    ) -> None:
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            serve_archive(b"x" * (20 * 8192), chunk_delay=0.2) as (url, requested, _),
            mock.patch.object(dotslash, "DOWNLOAD_TIMEOUT_SECS", 1),
        ):
            archive = Path(temp_dir) / "rg.zip"
            archive.write_bytes(b"previous")
            start = time.monotonic()
            with self.assertRaisesRegex(TimeoutError, "transfer deadline"):
                dotslash.download_archive(url, archive)
            self.assertLess(time.monotonic() - start, 2.5)
            self.assertTrue(requested.is_set())
            self.assertEqual(archive.read_bytes(), b"previous")
            self.assertEqual(list(Path(temp_dir).glob("*.tmp")), [])

    def test_cancellation_stops_header_and_body_waits_without_publication(
        self,
    ) -> None:
        for phase in ["headers", "body"]:
            with (
                self.subTest(phase=phase),
                tempfile.TemporaryDirectory() as temp_dir,
                serve_archive(
                    b"x" * (20 * 8192),
                    header_delay=5.6 if phase == "headers" else 0,
                    chunk_delay=0.2,
                ) as (url, requested, body_started),
                operation() as owned,
            ):
                archive = Path(temp_dir) / "rg.zip"
                archive.write_bytes(b"previous")
                ready = requested if phase == "headers" else body_started

                def cancel():
                    if ready.wait(3):
                        owned.cancelled.set()

                canceller = threading.Thread(target=cancel)
                canceller.start()
                start = time.monotonic()
                try:
                    with self.assertRaises(CancelledError):
                        dotslash.download_archive(url, archive)
                finally:
                    canceller.join()
                self.assertLess(time.monotonic() - start, 2.5)
                self.assertTrue(ready.is_set())
                self.assertEqual(archive.read_bytes(), b"previous")
                self.assertEqual(list(Path(temp_dir).glob("*.tmp")), [])

    def test_http_error_preserves_output_and_removes_partial_file(self) -> None:
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            serve_archive(b"unavailable", status=503) as (url, _, _),
        ):
            archive = Path(temp_dir) / "rg.zip"
            archive.write_bytes(b"previous")
            with self.assertRaisesRegex(RuntimeError, "503"):
                dotslash.download_archive(url, archive)
            self.assertEqual(archive.read_bytes(), b"previous")
            self.assertEqual(list(Path(temp_dir).glob("*.tmp")), [])

    def test_corrupt_download_is_removed_before_extraction(self) -> None:
        original = io.BytesIO()
        with zipfile.ZipFile(original, "w") as zipped:
            zipped.writestr("rg.exe", b"executable")
        body = original.getvalue()
        corrupt = bytearray(body)
        corrupt[len(corrupt) // 2] ^= 1
        with (
            tempfile.TemporaryDirectory() as temp_dir,
            serve_archive(bytes(corrupt)) as (url, _, _),
        ):
            root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-pc-windows-msvc"]
            manifest = root / "rg.json"
            manifest.write_text(
                json.dumps(
                    {
                        "platforms": {
                            spec.dotslash_platform: {
                                "providers": [{"url": url}],
                                "hash": "sha256",
                                "digest": hashlib.sha256(body).hexdigest(),
                                "size": len(body),
                                "format": "zip",
                                "path": "rg.exe",
                            }
                        }
                    }
                ),
                encoding="utf-8",
            )
            cache = root / "cache"
            with (
                mock.patch.object(dotslash, "default_cache_root", return_value=cache),
                mock.patch.object(dotslash, "extract_archive_member") as extract,
                self.assertRaisesRegex(RuntimeError, "sha256"),
            ):
                dotslash.fetch_dotslash_executable(
                    spec,
                    manifest_path=manifest,
                    artifact_label="ripgrep",
                    cache_key="rg",
                    dest_name="rg.exe",
                )
            extract.assert_not_called()
            self.assertEqual([path for path in cache.rglob("*") if path.is_file()], [])


if __name__ == "__main__":
    unittest.main()
