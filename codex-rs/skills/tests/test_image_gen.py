"""Offline regressions for the bundled image CLI's output preflight."""

import base64
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch


SOURCE = (
    Path(__file__).resolve().parents[1]
    / "src/assets/samples/imagegen/scripts/image_gen.py"
)
SPEC = importlib.util.spec_from_file_location("image_gen", SOURCE)
image_gen = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(image_gen)


class OutputPreflightTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.result = SimpleNamespace(
            data=[SimpleNamespace(b64_json=base64.b64encode(b"image").decode())]
        )

    def run_cli(self, argv):
        with (
            patch.object(sys, "argv", [str(SOURCE), *argv]),
            patch.object(image_gen, "_ensure_api_key"),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            return image_gen.main()

    def test_generate_and_edit_reject_original_and_derived_collisions(self):
        source = self.root / "input.png"
        source.write_bytes(b"input")
        for command in ("generate", "edit"):
            for derived in (False, True):
                with self.subTest(command=command, derived=derived):
                    output = self.root / f"{command}-{derived}.png"
                    collision = (
                        image_gen._derive_downscale_path(output, "-web")
                        if derived else output
                    )
                    collision.write_bytes(b"preserve")
                    argv = [command, "--prompt", "test", "--out", str(output)]
                    if command == "edit":
                        argv += ["--image", str(source)]
                    if derived:
                        argv += ["--downscale-max-dim", "16"]
                    with patch.object(image_gen, "_create_client") as client:
                        with self.assertRaises(SystemExit) as error:
                            self.run_cli(argv)
                        self.assertEqual(error.exception.code, 1)
                        client.assert_not_called()
                    self.assertEqual(collision.read_bytes(), b"preserve")
                    if derived:
                        self.assertFalse(output.exists())

    def test_checks_all_numbered_outputs(self):
        output = self.root / "image.png"
        (self.root / "image-2.png").write_bytes(b"preserve")
        with patch.object(image_gen, "_create_client") as client:
            with self.assertRaises(SystemExit):
                self.run_cli(["generate", "--prompt", "test", "--n", "2",
                              "--out", str(output)])
            client.assert_not_called()
        self.assertFalse((self.root / "image-1.png").exists())

    def test_batch_checks_later_job_overrides_before_any_request(self):
        jobs = self.root / "jobs.jsonl"
        jobs.write_text(
            json.dumps({"prompt": "first"}) + "\n"
            + json.dumps({"prompt": "second", "out": "custom", "n": 2,
                          "output_format": "webp"}) + "\n", encoding="utf-8"
        )
        collision = self.root / "custom-2-web.webp"
        collision.write_bytes(b"preserve")
        with patch.object(image_gen, "_create_async_client") as client:
            with self.assertRaises(SystemExit):
                self.run_cli(["generate-batch", "--input", str(jobs),
                              "--out-dir", str(self.root),
                              "--downscale-max-dim", "16"])
            client.assert_not_called()
        self.assertEqual(collision.read_bytes(), b"preserve")
        self.assertFalse((self.root / "001-first.png").exists())

    def test_force_and_dry_run_preserve_their_contracts(self):
        output = self.root / "existing.png"
        output.write_bytes(b"preserve")
        argv = ["generate", "--prompt", "test", "--out", str(output)]
        with patch.object(image_gen, "_create_client") as client:
            self.assertEqual(self.run_cli([*argv, "--dry-run"]), 0)
            client.assert_not_called()
            self.assertEqual(output.read_bytes(), b"preserve")
            client.return_value.images.generate.return_value = self.result
            self.assertEqual(self.run_cli([*argv, "--force"]), 0)
            client.return_value.images.generate.assert_called_once()
        self.assertEqual(output.read_bytes(), b"image")

    def test_successful_batch_still_writes_output(self):
        jobs = self.root / "jobs.jsonl"
        jobs.write_text('{"prompt":"first"}\n', encoding="utf-8")
        with patch.object(image_gen, "_create_async_client") as client:
            client.return_value.images.generate = AsyncMock(return_value=self.result)
            self.assertEqual(self.run_cli([
                "generate-batch", "--input", str(jobs), "--out-dir", str(self.root)
            ]), 0)
            client.return_value.images.generate.assert_awaited_once()
        self.assertEqual((self.root / "001-first.png").read_bytes(), b"image")

    def test_write_time_check_remains_after_preflight(self):
        output = self.root / "raced.png"
        with patch.object(image_gen, "_create_client") as client:
            def request(**kwargs):
                output.write_bytes(b"concurrent")
                return self.result
            client.return_value.images.generate.side_effect = request
            with self.assertRaises(SystemExit):
                self.run_cli(["generate", "--prompt", "test", "--out", str(output)])
        self.assertEqual(output.read_bytes(), b"concurrent")


if __name__ == "__main__":
    unittest.main()
