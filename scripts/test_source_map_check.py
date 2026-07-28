#!/usr/bin/env python3

import contextlib
import io
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import source_map_check

REPO_ROOT = Path(__file__).resolve().parents[1]


def table_section(
    heading: str,
    headers: tuple[str, str],
    *rows: str,
) -> list[str]:
    return [
        heading,
        "",
        f"| {headers[0]} | {headers[1]} |",
        "| --- | --- |",
        *rows,
        "",
    ]


def source_map_with_rows(*rows: str) -> str:
    return "\n".join(
        [
            "# Source Map",
            "",
            *table_section(
                source_map_check.TOP_LEVEL_OWNERS_HEADING,
                ("Path", "Owns"),
                *rows,
            ),
            "## Next section",
        ]
    )


def complete_source_map(
    *,
    top_level_rows: tuple[str, ...],
    instruction_rows: tuple[str, ...] = (),
    rust_rows: tuple[str, ...] = (),
    project_rows: tuple[str, ...] = (),
) -> str:
    return "\n".join(
        [
            "# Source Map",
            "",
            *table_section(
                source_map_check.TOP_LEVEL_OWNERS_HEADING,
                ("Path", "Owns"),
                *top_level_rows,
            ),
            *table_section(
                source_map_check.INSTRUCTION_SCOPES_HEADING,
                ("Path", "Applies to"),
                *instruction_rows,
            ),
            *table_section(
                source_map_check.RUST_PACKAGE_INVENTORY_HEADING,
                ("Domain", "Package roots"),
                *rust_rows,
            ),
            *table_section(
                source_map_check.NON_RUST_PROJECT_INVENTORY_HEADING,
                ("Manifest", "Owns"),
                *project_rows,
            ),
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
                complete_source_map(
                    top_level_rows=("| `.github/` | Automation |",),
                ),
                encoding="utf-8",
            )
            errors = io.StringIO()

            with contextlib.redirect_stderr(errors):
                result = source_map_check.check_source_map(source_map, repo_root=root)

            self.assertEqual(result, 1)
            self.assertIn(
                "declared owner has no repository source: .github", errors.getvalue()
            )

    def test_check_accepts_complete_material_inventory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source_map = root / "SOURCEMAP.md"
            source_map.write_text(
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md` | Policy |",
                        "| `codex-rs/` | Rust |",
                        "| `package.json` | Root tooling |",
                        "| `sdk/` | SDKs |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                    rust_rows=("| Core | `codex-rs/core` |",),
                    project_rows=(
                        "| `package.json` | Root tooling |",
                        "| `sdk/typescript/package.json` | TypeScript SDK |",
                    ),
                ),
                encoding="utf-8",
            )
            sources = {
                "AGENTS.md",
                "codex-rs/core/Cargo.toml",
                "package.json",
                "sdk/typescript/package.json",
            }

            self.assertEqual(
                source_map_check.check_source_map(
                    source_map,
                    repo_root=root,
                    source_paths=sources,
                    tracked_source_paths=sources,
                ),
                0,
            )

    def test_check_reports_undeclared_top_level_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source_map = root / "SOURCEMAP.md"
            source_map.write_text(
                complete_source_map(
                    top_level_rows=("| `scripts/` | Maintenance |",),
                ),
                encoding="utf-8",
            )
            sources = {"scripts/check.py", "docs/guide.md"}
            errors = io.StringIO()

            with contextlib.redirect_stderr(errors):
                result = source_map_check.check_source_map(
                    source_map,
                    repo_root=root,
                    source_paths=sources,
                    tracked_source_paths=sources,
                )

            self.assertEqual(result, 1)
            self.assertIn(
                "missing top-level ownership entry for tracked path: docs",
                errors.getvalue(),
            )

    def test_check_reports_new_instruction_rust_and_project_inventories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source_map = root / "SOURCEMAP.md"
            source_map.write_text(
                complete_source_map(
                    top_level_rows=(
                        "| `codex-rs/` | Rust |",
                        "| `sdk/` | SDK |",
                    ),
                ),
                encoding="utf-8",
            )
            sources = {
                "codex-rs/new-crate/AGENTS.md",
                "codex-rs/new-crate/Cargo.toml",
                "sdk/new-client/pyproject.toml",
            }
            errors = io.StringIO()

            with contextlib.redirect_stderr(errors):
                result = source_map_check.check_source_map(
                    source_map,
                    repo_root=root,
                    source_paths=sources,
                    tracked_source_paths=sources,
                )

            self.assertEqual(result, 1)
            output = errors.getvalue()
            self.assertIn(
                "missing instruction scope entry for tracked path: "
                "codex-rs/new-crate/AGENTS.md",
                output,
            )
            self.assertIn(
                "missing Rust package entry for tracked path: codex-rs/new-crate",
                output,
            )
            self.assertIn(
                "missing non-Rust project manifest entry for tracked path: "
                "sdk/new-client/pyproject.toml",
                output,
            )

    def test_declared_owner_must_stay_within_repository(self) -> None:
        with self.assertRaisesRegex(ValueError, "repository-relative path"):
            source_map_check.declared_top_level_owners(
                source_map_with_rows("| `../outside/` | Invalid |")
            )

    def test_top_level_owner_cannot_name_nested_path(self) -> None:
        with self.assertRaisesRegex(ValueError, "one top-level entry"):
            source_map_check.declared_top_level_owners(
                source_map_with_rows("| `scripts/check.py` | Invalid |")
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

    def test_inventory_paths_must_be_unique(self) -> None:
        markdown = complete_source_map(
            top_level_rows=("| `scripts/` | Maintenance |",),
            rust_rows=(
                "| Runtime | `codex-rs/core` |",
                "| Runtime support | `codex-rs/core` |",
            ),
        )

        with self.assertRaisesRegex(ValueError, "duplicate rust package inventory"):
            source_map_check.declared_rust_package_roots(markdown)

    def test_repository_source_map_matches_material_inventory(self) -> None:
        self.assertEqual(
            source_map_check.check_source_map(
                REPO_ROOT / "SOURCEMAP.md", repo_root=REPO_ROOT
            ),
            0,
        )


if __name__ == "__main__":
    unittest.main()
