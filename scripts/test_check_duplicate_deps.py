from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class CheckDuplicateDepsTest(unittest.TestCase):
    def run_cli(
        self,
        *,
        duplicate_versions: bool = False,
        malformed_manifest: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        script = Path(__file__).with_name("check_duplicate_deps.py").resolve()
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "src").mkdir()
            (root / "src" / "main.rs").write_text("fn main() {}\n", encoding="utf-8")
            manifest = (
                "not valid TOML\n"
                if malformed_manifest
                else (
                    '[package]\nname = "codex-cli"\nversion = "0.1.0"\nedition = "2021"\n'
                )
            )
            if duplicate_versions:
                manifest += (
                    "\n[dependencies]\n"
                    'foo_v1 = { package = "foo", path = "foo-v1" }\n'
                    'foo_v2 = { package = "foo", path = "foo-v2" }\n'
                )
                for name, version in (("foo-v1", "1.0.0"), ("foo-v2", "2.0.0")):
                    crate = root / name
                    (crate / "src").mkdir(parents=True)
                    (crate / "Cargo.toml").write_text(
                        f'[package]\nname = "foo"\nversion = "{version}"\nedition = "2021"\n',
                        encoding="utf-8",
                    )
                    (crate / "src" / "lib.rs").write_text(
                        "pub fn marker() {}\n", encoding="utf-8"
                    )
            (root / "Cargo.toml").write_text(manifest, encoding="utf-8")
            return subprocess.run(
                [sys.executable, str(script)],
                cwd=root,
                check=False,
                capture_output=True,
                text=True,
            )

    def test_cli_passes_when_child_cargo_reports_no_duplicates(self) -> None:
        completed = self.run_cli()

        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "")

    def test_cli_fails_when_child_cargo_reports_duplicate_versions(self) -> None:
        completed = self.run_cli(duplicate_versions=True)

        self.assertEqual(completed.returncode, 1)
        self.assertIn("foo v1.0.0", completed.stdout)
        self.assertIn("foo v2.0.0", completed.stdout)
        self.assertIn("duplicate dependency versions detected", completed.stderr)

    def test_cli_preserves_child_cargo_failure(self) -> None:
        completed = self.run_cli(malformed_manifest=True)

        self.assertEqual(completed.returncode, 101)
        self.assertEqual(completed.stdout, "")
        self.assertIn("Cargo.toml", completed.stderr)
        self.assertIn("expected `=`", completed.stderr)


if __name__ == "__main__":
    unittest.main()
