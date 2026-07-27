#!/usr/bin/env python3

import contextlib
import io
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import source_map_check

REPO_ROOT = Path(__file__).resolve().parents[1]


def source_map_with_rows(*rows: str) -> str:
    return "\n".join(
        [
            "# Source Map",
            "",
            source_map_check.TOP_LEVEL_OWNERS_HEADING,
            "",
            "| Path | Owns |",
            "| --- | --- |",
            *rows,
            "",
            "## Next section",
        ]
    )


class SourceMapCheckTest(unittest.TestCase):
    def test_declared_top_level_owners_extracts_every_path_in_path_cell(self) -> None:
        owners = source_map_check.declared_top_level_owners(
            source_map_with_rows(
                "| `.codex/` | Local state |",
                "| `package.json`, `pnpm-workspace.yaml` | Root tooling |",
            )
        )

        self.assertEqual(
            [owner for _, owner in owners],
            [".codex", "package.json", "pnpm-workspace.yaml"],
        )

    def test_check_fails_when_a_declared_owner_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=root,
                capture_output=True,
                text=True,
                check=True,
            )
            (root / ".gitignore").write_text("/.github/\n", encoding="utf-8")
            generated = root / ".github" / "generated"
            generated.mkdir(parents=True)
            (generated / "local.log").write_text("local\n", encoding="utf-8")
            source_map = root / "SOURCEMAP.md"
            source_map.write_text(
                source_map_with_rows("| `.github/` | Automation |"),
                encoding="utf-8",
            )
            errors = io.StringIO()

            with contextlib.redirect_stderr(errors):
                result = source_map_check.check_source_map(source_map, repo_root=root)

            self.assertEqual(result, 1)
            self.assertIn(
                "declared owner has no repository source: .github", errors.getvalue()
            )

    def test_check_accepts_existing_declared_owners(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            (root / "scripts").mkdir()
            (root / "package.json").write_text("{}\n", encoding="utf-8")
            source_map = root / "SOURCEMAP.md"
            source_map.write_text(
                source_map_with_rows(
                    "| `scripts/` | Maintenance |",
                    "| `package.json` | Root tooling |",
                ),
                encoding="utf-8",
            )

            self.assertEqual(
                source_map_check.check_source_map(
                    source_map,
                    repo_root=root,
                    source_paths={"scripts/check.py", "package.json"},
                ),
                0,
            )

    def test_declared_owner_must_stay_within_repository(self) -> None:
        with self.assertRaisesRegex(ValueError, "repository-relative path"):
            source_map_check.declared_top_level_owners(
                source_map_with_rows("| `../outside/` | Invalid |")
            )

    def test_ownership_section_rejects_malformed_rows(self) -> None:
        with self.assertRaisesRegex(ValueError, "only a Markdown table"):
            source_map_check.declared_top_level_owners(
                source_map_with_rows("`missing/` | Invalid |")
            )

    def test_ownership_section_must_be_unique(self) -> None:
        duplicate = "\n".join(
            [
                source_map_with_rows("| `scripts/` | Maintenance |"),
                source_map_check.TOP_LEVEL_OWNERS_HEADING,
                "| Path | Owns |",
                "| --- | --- |",
                "| `docs/` | Documentation |",
            ]
        )

        with self.assertRaisesRegex(ValueError, "duplicate"):
            source_map_check.declared_top_level_owners(duplicate)

    def test_repository_source_map_declares_only_existing_owners(self) -> None:
        self.assertEqual(
            source_map_check.check_source_map(
                REPO_ROOT / "SOURCEMAP.md", repo_root=REPO_ROOT
            ),
            0,
        )


if __name__ == "__main__":
    unittest.main()
