#!/usr/bin/env python3

import contextlib
import hashlib
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Iterator
from pathlib import Path

from scripts.generated_output_lock import source_map_lock

REPO_ROOT = Path(__file__).resolve().parents[1]
SOURCE_MAP_CHECK = REPO_ROOT / "scripts" / "source_map_check.py"
TOP_LEVEL_OWNERS_HEADING = "## Top-level ownership"
INSTRUCTION_SCOPES_HEADING = "## Instruction scopes"
RUST_PACKAGE_INVENTORY_HEADING = "## Rust package inventory"
NON_RUST_PROJECT_INVENTORY_HEADING = "## Non-Rust project inventory"
TRACKED_PATH_SNAPSHOT_BEGIN = "<!-- BEGIN TRACKED PATH SNAPSHOT -->"
TRACKED_PATH_SNAPSHOT_INSERT_AFTER = (
    "Update it in the same change whenever the repository materially changes."
)


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
            "## Maintenance contract",
            "",
            TRACKED_PATH_SNAPSHOT_INSERT_AFTER,
            "",
            *table_section(
                TOP_LEVEL_OWNERS_HEADING,
                ("Path", "Owns"),
                *top_level_rows,
            ),
            *table_section(
                INSTRUCTION_SCOPES_HEADING,
                ("Path", "Applies to"),
                *instruction_rows,
            ),
            *table_section(
                RUST_PACKAGE_INVENTORY_HEADING,
                ("Domain", "Package roots"),
                *rust_rows,
            ),
            *table_section(
                NON_RUST_PROJECT_INVENTORY_HEADING,
                ("Manifest", "Owns"),
                *project_rows,
            ),
        ]
    )


@contextlib.contextmanager
def temporary_git_repository() -> Iterator[Path]:
    with tempfile.TemporaryDirectory() as temp_dir:
        root = Path(temp_dir)
        subprocess.run(
            ["git", "init", "--quiet"],
            cwd=root,
            capture_output=True,
            text=True,
            check=True,
        )
        yield root


def write_repository_file(
    root: Path,
    relative_path: str,
    contents: str | bytes,
) -> Path:
    path = root / relative_path
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(contents, bytes):
        path.write_bytes(contents)
    else:
        path.write_text(contents, encoding="utf-8")
    return path


def stage_repository_paths(root: Path, *relative_paths: str) -> None:
    command = ["git", "add"]
    if relative_paths:
        command.extend(["--", *relative_paths])
    else:
        command.append("--all")
    subprocess.run(
        command,
        cwd=root,
        capture_output=True,
        text=True,
        check=True,
    )


def run_source_map_cli(
    root: Path,
    source_map: Path | None = None,
    *,
    check_only: bool = False,
    timeout: int = 60,
) -> subprocess.CompletedProcess[str]:
    selected_source_map = (
        source_map if source_map is not None else root / "SOURCEMAP.md"
    )
    command = [
        sys.executable,
        str(SOURCE_MAP_CHECK),
        str(selected_source_map),
        "--repo-root",
        str(root),
    ]
    if check_only:
        command.append("--check-only")
    return subprocess.run(
        command,
        cwd=root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
        timeout=timeout,
    )


def workspace_bytes(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in root.rglob("*")
        if path.is_file() and ".git" not in path.relative_to(root).parts
    }


def expected_snapshot(paths: set[str]) -> str:
    payload = "".join(f"{path}\n" for path in sorted(paths)).encode("utf-8")
    return (
        f"Tracked repository path snapshot: `count={len(paths)} "
        f"sha256={hashlib.sha256(payload).hexdigest()}`."
    )


class SourceMapCheckTest(unittest.TestCase):
    def assert_cli_success(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(
            result.returncode,
            0,
            msg=f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def test_source_map_check_respects_shared_writer_lock_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("| `SOURCEMAP.md` | Source routing |",),
                ),
            )
            stage_repository_paths(root, "SOURCEMAP.md")

            with source_map_lock(root, "test-holder"):
                result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("source map outputs is already locked", result.stderr)

    def test_declared_top_level_owners_extracts_every_path_in_path_cell_through_cli(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `SOURCEMAP.md`, `package.json`, `pnpm-workspace.yaml` | Root tooling |",
                    ),
                    project_rows=("| `package.json` | Root tooling |",),
                ),
            )
            write_repository_file(root, "package.json", "{}\n")
            write_repository_file(root, "pnpm-workspace.yaml", "packages: []\n")
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assert_cli_success(result)
            self.assertNotIn("missing top-level ownership entry", result.stderr)

    def test_untracked_file_cannot_satisfy_declared_owner_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `SOURCEMAP.md` | Source routing |",
                        "| `.github/` | Automation |",
                    ),
                ),
            )
            write_repository_file(root, ".github/generated/local.log", "local\n")
            stage_repository_paths(root, "SOURCEMAP.md")

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "declared owner has no repository source: .github",
                result.stderr,
            )

    def test_check_accepts_complete_material_inventory_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
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
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            write_repository_file(root, "codex-rs/core/Cargo.toml", "[package]\n")
            write_repository_file(root, "package.json", "{}\n")
            write_repository_file(root, "sdk/typescript/package.json", "{}\n")
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assert_cli_success(result)
            self.assertIn(TRACKED_PATH_SNAPSHOT_BEGIN, source_map.read_text("utf-8"))

    def test_check_reports_undeclared_top_level_entry_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `SOURCEMAP.md` | Source routing |",
                        "| `scripts/` | Maintenance |",
                    ),
                ),
            )
            write_repository_file(root, "scripts/check.py", "pass\n")
            write_repository_file(root, "docs/guide.md", "# Guide\n")
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "missing top-level ownership entry for tracked path: docs",
                result.stderr,
            )

    def test_check_reports_new_instruction_rust_and_project_inventories_through_cli(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `SOURCEMAP.md` | Source routing |",
                        "| `codex-rs/` | Rust |",
                        "| `sdk/` | SDK |",
                    ),
                ),
            )
            write_repository_file(root, "codex-rs/new-crate/AGENTS.md", "# Policy\n")
            write_repository_file(root, "codex-rs/new-crate/Cargo.toml", "[package]\n")
            write_repository_file(root, "sdk/new-client/pyproject.toml", "[project]\n")
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "missing instruction scope entry for tracked path: "
                "codex-rs/new-crate/AGENTS.md",
                result.stderr,
            )
            self.assertIn(
                "missing Rust package entry for tracked path: codex-rs/new-crate",
                result.stderr,
            )
            self.assertIn(
                "missing non-Rust project manifest entry for tracked path: "
                "sdk/new-client/pyproject.toml",
                result.stderr,
            )

    def test_declared_owner_must_stay_within_repository_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("| `../outside/` | Invalid |",),
                ),
            )
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("repository-relative path", result.stderr)

    def test_top_level_owner_cannot_name_nested_path_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("| `scripts/check.py` | Invalid |",),
                ),
            )
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("one top-level entry", result.stderr)

    def test_ownership_section_rejects_malformed_rows_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("`missing/` | Invalid |",),
                ),
            )
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("only a Markdown table", result.stderr)

    def test_ownership_section_must_be_unique_through_cli(self) -> None:
        with temporary_git_repository() as root:
            duplicate = "\n".join(
                [
                    complete_source_map(
                        top_level_rows=(
                            "| `SOURCEMAP.md` | Source routing |",
                            "| `scripts/` | Maintenance |",
                        ),
                    ),
                    TOP_LEVEL_OWNERS_HEADING,
                    "| Path | Owns |",
                    "| --- | --- |",
                    "| `docs/` | Documentation |",
                ]
            )
            source_map = write_repository_file(root, "SOURCEMAP.md", duplicate)
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("duplicate '## Top-level ownership' sections", result.stderr)

    def test_inventory_paths_must_be_unique_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("| `SOURCEMAP.md` | Source routing |",),
                    rust_rows=(
                        "| Runtime | `codex-rs/core` |",
                        "| Runtime support | `codex-rs/core` |",
                    ),
                ),
            )
            stage_repository_paths(root)

            result = run_source_map_cli(root, source_map)

            self.assertEqual(result.returncode, 1)
            self.assertIn("duplicate rust package inventory path", result.stderr)

    def test_sync_tracked_path_snapshot_rewrites_structural_changes_through_cli(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
                        "| `src/` | Source |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                ),
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            write_repository_file(root, "src/old_name.rs", "pub fn old() {}\n")
            stage_repository_paths(root)

            first = run_source_map_cli(root, source_map)
            self.assert_cli_success(first)
            original = source_map.read_text(encoding="utf-8")
            self.assertIn(
                expected_snapshot({"AGENTS.md", "SOURCEMAP.md", "src/old_name.rs"}),
                original,
            )

            unchanged = run_source_map_cli(root, source_map)
            self.assert_cli_success(unchanged)
            self.assertEqual(source_map.read_text(encoding="utf-8"), original)

            subprocess.run(
                ["git", "mv", "src/old_name.rs", "src/new_name.rs"],
                cwd=root,
                capture_output=True,
                text=True,
                check=True,
            )
            renamed_result = run_source_map_cli(root, source_map)
            self.assert_cli_success(renamed_result)
            renamed = source_map.read_text(encoding="utf-8")
            self.assertNotEqual(renamed, original)
            self.assertIn(
                expected_snapshot({"AGENTS.md", "SOURCEMAP.md", "src/new_name.rs"}),
                renamed,
            )
            self.assertEqual(renamed.count(TRACKED_PATH_SNAPSHOT_BEGIN), 1)

    def test_check_only_rejects_stale_snapshot_without_changing_workspace_bytes(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
                        "| `src/` | Source |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                ),
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            write_repository_file(root, "src/old.rs", "pub fn old() {}\n")
            stage_repository_paths(root)
            self.assert_cli_success(run_source_map_cli(root, source_map))

            write_repository_file(root, "src/new.rs", "pub fn new() {}\n")
            stage_repository_paths(root, "src/new.rs")
            before = workspace_bytes(root)

            result = run_source_map_cli(root, source_map, check_only=True)

            self.assertEqual(result.returncode, 1)
            self.assertIn("tracked path snapshot is stale", result.stderr)
            self.assertEqual(workspace_bytes(root), before)

    def test_check_only_rejects_missing_snapshot_without_writer_side_effects(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                ),
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            stage_repository_paths(root)
            before = workspace_bytes(root)
            lock_file = root / ".codex" / "locks" / "source-map.lock"

            result = run_source_map_cli(root, source_map, check_only=True)

            self.assertEqual(result.returncode, 1)
            self.assertIn(
                "tracked path snapshot is stale; run just source-map-check",
                result.stderr,
            )
            self.assertEqual(workspace_bytes(root), before)
            self.assertFalse(lock_file.exists())

    def test_check_only_accepts_fresh_snapshot_without_writer_side_effects(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                ),
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            stage_repository_paths(root)
            self.assert_cli_success(run_source_map_cli(root, source_map))
            lock_file = root / ".codex" / "locks" / "source-map.lock"
            lock_file.unlink()
            lock_file.parent.rmdir()
            lock_file.parent.parent.rmdir()
            before = workspace_bytes(root)

            result = run_source_map_cli(root, source_map, check_only=True)

            self.assert_cli_success(result)
            self.assertEqual(workspace_bytes(root), before)
            self.assertFalse(lock_file.exists())

    def test_sync_snapshot_preserves_target_when_atomic_write_fails_through_cli(
        self,
    ) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "docs/SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=("| `docs/` | Documentation |",),
                ),
            )
            stage_repository_paths(root)
            original = source_map.read_bytes()
            original_mode = source_map.stat().st_mode
            parent_mode = source_map.parent.stat().st_mode
            if os.name == "nt":
                os.chmod(source_map, stat.S_IREAD)
            else:
                os.chmod(source_map.parent, stat.S_IREAD | stat.S_IEXEC)
            try:
                result = run_source_map_cli(root, source_map)
            finally:
                if os.name == "nt":
                    os.chmod(source_map, original_mode)
                else:
                    os.chmod(source_map.parent, parent_mode)

            self.assertEqual(result.returncode, 1)
            self.assertEqual(source_map.read_bytes(), original)
            self.assertEqual(list(source_map.parent.glob(".SOURCEMAP.md.*.tmp")), [])

    def test_main_synchronizes_snapshot_before_validation_through_cli(self) -> None:
        with temporary_git_repository() as root:
            source_map = write_repository_file(
                root,
                "SOURCEMAP.md",
                complete_source_map(
                    top_level_rows=(
                        "| `AGENTS.md`, `SOURCEMAP.md` | Policy and routing |",
                    ),
                    instruction_rows=("| `AGENTS.md` | Repository |",),
                ),
            )
            write_repository_file(root, "AGENTS.md", "# Policy\n")
            write_repository_file(root, "local-untracked.txt", "local\n")
            stage_repository_paths(root, "AGENTS.md", "SOURCEMAP.md")

            result = run_source_map_cli(root, source_map)

            self.assert_cli_success(result)
            synchronized = source_map.read_text(encoding="utf-8")
            self.assertIn(TRACKED_PATH_SNAPSHOT_BEGIN, synchronized)
            self.assertIn(
                expected_snapshot({"AGENTS.md", "SOURCEMAP.md"}),
                synchronized,
            )
            self.assertTrue((root / ".codex/locks/source-map.lock").is_file())

    def test_repository_source_map_matches_material_inventory_through_cli(self) -> None:
        tracked = (
            subprocess.run(
                ["git", "ls-files", "-z"],
                cwd=REPO_ROOT,
                capture_output=True,
                check=True,
            )
            .stdout.decode("utf-8")
            .split("\0")
        )
        material_paths = [
            path for path in tracked if path and (REPO_ROOT / Path(path)).is_file()
        ]
        self.assertIn("SOURCEMAP.md", material_paths)

        with temporary_git_repository() as root:
            for path in material_paths:
                contents = (
                    (REPO_ROOT / "SOURCEMAP.md").read_bytes()
                    if path == "SOURCEMAP.md"
                    else b""
                )
                write_repository_file(root, path, contents)
            stage_repository_paths(root)

            result = run_source_map_cli(root, timeout=180)

            self.assert_cli_success(result)


if __name__ == "__main__":
    unittest.main()
