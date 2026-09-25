from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import check_duplicate_deps


class CheckDuplicateDepsTest(unittest.TestCase):
    def test_empty_duplicate_report_passes(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            self.assertEqual(
                args,
                (
                    [
                        "cargo",
                        "tree",
                        "-d",
                        "-p",
                        "codex-cli",
                        "--target",
                        "x86_64-pc-windows-msvc",
                    ],
                ),
            )
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="", stderr=""
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps(
                ["--target", "x86_64-pc-windows-msvc"], runner=runner
            ),
            0,
        )

    def test_duplicate_report_fails(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="foo v1.0.0\nfoo v2.0.0\n", stderr=""
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps([], runner=runner), 1
        )

    def test_cargo_failure_is_preserved(self) -> None:
        def runner(*args: object, **kwargs: object) -> subprocess.CompletedProcess[str]:
            return subprocess.CompletedProcess(
                args=[], returncode=7, stdout="", stderr="boom\n"
            )

        self.assertEqual(
            check_duplicate_deps.check_duplicate_deps([], runner=runner), 7
        )

    @unittest.skipUnless(shutil.which("cargo"), "cargo is required")
    def test_manifest_drift_updates_lockfile(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            (root / "src" / "main.rs").write_text("fn main() {}", encoding="utf-8")
            manifest = root / "Cargo.toml"
            manifest.write_text(
                '[package]\nname = "codex-cli"\nversion = "0.1.0"\nedition = "2021"\n',
                encoding="utf-8",
            )
            subprocess.run(
                ["cargo", "generate-lockfile", "--offline"],
                cwd=root,
                check=True,
                capture_output=True,
            )
            before = (root / "Cargo.lock").read_bytes()
            manifest.write_text(
                manifest.read_text("utf-8").replace('"0.1.0"', '"0.2.0"'),
                encoding="utf-8",
            )

            def runner(
                *args: object, **kwargs: object
            ) -> subprocess.CompletedProcess[str]:
                check = kwargs.pop("check")
                return subprocess.run(*args, cwd=root, check=check, **kwargs)

            self.assertEqual(
                check_duplicate_deps.check_duplicate_deps(["--offline"], runner=runner),
                0,
            )
            self.assertNotEqual((root / "Cargo.lock").read_bytes(), before)
            self.assertIn('version = "0.2.0"', (root / "Cargo.lock").read_text("utf-8"))


if __name__ == "__main__":
    unittest.main()
