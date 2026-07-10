#!/usr/bin/env python3

import io
import hashlib
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from codex_package import v8
from codex_package.targets import TARGET_SPECS


class V8ArtifactCacheTest(unittest.TestCase):
    def test_resolve_env_uses_paired_v8_overrides_without_fetching(self) -> None:
        spec = TARGET_SPECS["x86_64-apple-darwin"]
        environ = {
            "RUSTY_V8_ARCHIVE": "prebuilt-v8.a.gz",
            "RUSTY_V8_SRC_BINDING_PATH": "src_binding.rs",
        }

        with mock.patch.object(
            v8,
            "fetch_codex_v8_artifacts",
            side_effect=AssertionError("paired overrides should skip fetch"),
        ):
            self.assertEqual(
                v8.resolve_codex_v8_cargo_env(spec, environ=environ),
                {},
            )

    def test_resolve_env_skips_fetch_when_v8_from_source_is_truthy(self) -> None:
        spec = TARGET_SPECS["x86_64-apple-darwin"]

        for value in [
            "1",
            "true",
            "TRUE",
            "yes",
            "YES",
            "on",
            "ON",
            " TrUe ",
        ]:
            with (
                self.subTest(value=value),
                mock.patch.object(
                    v8,
                    "fetch_codex_v8_artifacts",
                    side_effect=AssertionError("V8_FROM_SOURCE should skip fetch"),
                ),
            ):
                self.assertEqual(
                    v8.resolve_codex_v8_cargo_env(
                        spec,
                        environ={"V8_FROM_SOURCE": value},
                    ),
                    {},
                )

    def test_resolve_env_rejects_unpaired_v8_override(self) -> None:
        spec = TARGET_SPECS["x86_64-apple-darwin"]

        with self.assertRaisesRegex(RuntimeError, "set together"):
            v8.resolve_codex_v8_cargo_env(
                spec,
                environ={"RUSTY_V8_ARCHIVE": "prebuilt-v8.a.gz"},
            )

    def test_verified_checksum_stamp_skips_warm_cache_hash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            artifact = Path(temp_dir) / "artifact"
            content = b"v8"
            artifact.write_bytes(content)
            digest = hashlib.sha256(content).hexdigest()

            self.assertTrue(v8.has_checksum(artifact, digest))
            with mock.patch.object(
                v8.hashlib,
                "sha256",
                side_effect=AssertionError("warm cache should use stamp"),
            ):
                self.assertTrue(v8.has_checksum(artifact, digest))

    def test_ensure_valid_artifact_removes_invalid_download(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            artifact = Path(temp_dir) / "artifact"

            def fake_download(_url: str, dest: Path) -> None:
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_bytes(b"wrong")

            with mock.patch.object(v8, "download_file", side_effect=fake_download):
                with self.assertRaisesRegex(RuntimeError, "failed checksum"):
                    v8.ensure_valid_artifact(
                        artifact,
                        "0" * 64,
                        "https://example.test/artifact",
                    )

            self.assertFalse(artifact.exists())

    def test_fetch_artifacts_skips_manifest_download_when_cached_artifacts_are_verified(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cache_root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-apple-darwin"]
            cache_dir = cache_root / f"rusty-v8-1.2.3-{spec.target}"
            archive = cache_dir / f"librusty_v8_release_{spec.target}.a.gz"
            binding = cache_dir / f"src_binding_release_{spec.target}.rs"
            checksums = cache_dir / f"rusty_v8_release_{spec.target}.sha256"
            archive.parent.mkdir(parents=True)
            archive.write_bytes(b"archive")
            binding.write_bytes(b"binding")
            archive_digest = hashlib.sha256(b"archive").hexdigest()
            binding_digest = hashlib.sha256(b"binding").hexdigest()
            checksums.write_text(
                "\n".join(
                    [
                        f"{archive_digest} {archive.name}",
                        f"{binding_digest} {binding.name}",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            self.assertTrue(v8.has_checksum(archive, archive_digest))
            self.assertTrue(v8.has_checksum(binding, binding_digest))

            with (
                mock.patch.object(
                    v8, "resolved_v8_crate_version", return_value="1.2.3"
                ),
                mock.patch.object(
                    v8,
                    "download_file",
                    side_effect=AssertionError("manifest should be cached"),
                ),
                mock.patch.object(v8, "ensure_valid_artifact") as ensure_valid,
            ):
                pair = v8.fetch_codex_v8_artifacts(spec, cache_root=cache_root)

            self.assertEqual(pair.archive, archive)
            self.assertEqual(pair.binding, binding)
            ensure_valid.assert_not_called()

    def test_fetch_artifacts_validates_archive_and_binding(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            cache_root = Path(temp_dir)
            spec = TARGET_SPECS["x86_64-apple-darwin"]

            def fake_download(_url: str, dest: Path) -> None:
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_text(
                    "\n".join(
                        [
                            f"{'1' * 64} librusty_v8_release_{spec.target}.a.gz",
                            f"{'2' * 64} src_binding_release_{spec.target}.rs",
                        ]
                    )
                    + "\n",
                    encoding="utf-8",
                )

            with (
                mock.patch.object(
                    v8, "resolved_v8_crate_version", return_value="1.2.3"
                ),
                mock.patch.object(v8, "download_file", side_effect=fake_download),
                mock.patch.object(v8, "ensure_valid_artifact") as ensure_valid,
            ):
                pair = v8.fetch_codex_v8_artifacts(spec, cache_root=cache_root)

            self.assertEqual(
                pair.archive.name, f"librusty_v8_release_{spec.target}.a.gz"
            )
            self.assertEqual(pair.binding.name, f"src_binding_release_{spec.target}.rs")
            self.assertEqual(ensure_valid.call_count, 2)

    def test_checksum_manifest_accepts_binary_mode_marker(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            checksums = Path(temp_dir) / "checksums"
            checksums.write_text(f"{'1' * 64} *artifact.a.gz\n", encoding="utf-8")

            self.assertEqual(
                v8.load_checksums(checksums, {"artifact.a.gz"}),
                {"artifact.a.gz": "1" * 64},
            )

    def test_download_file_leaves_stale_deterministic_temp_file_alone(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            dest = Path(temp_dir) / "artifact.gz"
            stale_temp = dest.with_suffix(f"{dest.suffix}.tmp")
            stale_temp.write_text("keep", encoding="utf-8")

            class FakeResponse(io.BytesIO):
                def __enter__(self) -> "FakeResponse":
                    return self

                def __exit__(self, *args: object) -> None:
                    self.close()

            with mock.patch.object(
                v8, "urlopen", return_value=FakeResponse(b"artifact")
            ):
                v8.download_file("https://example.test/artifact.gz", dest)

            self.assertEqual(dest.read_bytes(), b"artifact")
            self.assertEqual(stale_temp.read_text(encoding="utf-8"), "keep")


if __name__ == "__main__":
    unittest.main()
