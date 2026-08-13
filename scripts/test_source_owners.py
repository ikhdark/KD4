from pathlib import Path
from unittest import mock
import tempfile
import unittest

from scripts import source_owners


class SourceOwnersTest(unittest.TestCase):
    def test_manifest_validation_and_managed_block_are_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            (root / "AGENTS.md").write_text("instructions\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 1

[[owners]]
id = "alpha"
concern_ids = ["alpha-routing"]
aliases = ["alpha"]
phrases = ["alpha locator"]
ambiguous_with = []
roots = ["src"]
instructions = ["AGENTS.md"]
consumers = []
contracts = []
generated_mirrors = []
tests = ["src/lib.rs"]

[[owners.primary_entries]]
path = "src/lib.rs"
symbol = "locate"

[[owners.validation]]
id = "focused"
cwd = "."
argv = ["cargo", "test", "focused"]
role = "focused_tests"
""",
                encoding="utf-8",
            )

            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            block = source_owners.render_block(manifest, digest)
            first = source_owners.replace_managed_block("manual prose\n", block)
            second = source_owners.replace_managed_block(first, block)

            self.assertEqual(first, second)
            self.assertTrue(first.startswith("manual prose\n"))
            self.assertIn("schema=1", block)
            self.assertIn("`alpha`", block)

    def test_alias_collision_requires_explicit_ambiguity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            (root / "AGENTS.md").write_text("instructions\n", encoding="utf-8")
            owners = []
            for owner_id in ("alpha", "beta"):
                owners.append(
                    f"""
[[owners]]
id = "{owner_id}"
concern_ids = []
aliases = ["shared alias"]
phrases = []
ambiguous_with = []
roots = ["src"]
instructions = ["AGENTS.md"]
consumers = []
contracts = []
generated_mirrors = []
tests = []

[[owners.primary_entries]]
path = "src/lib.rs"
symbol = "{owner_id}_entry"
"""
                )
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                "schema_version = 1\n" + "".join(owners), encoding="utf-8"
            )

            with self.assertRaisesRegex(ValueError, "collision"):
                source_owners.load_and_validate(manifest_path, root)

    def test_custom_manifest_defaults_declared_paths_to_its_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 1
[[owners]]
id = "alpha"
roots = ["src"]
""",
                encoding="utf-8",
            )

            manifest, _ = source_owners.load_and_validate(manifest_path)

            self.assertEqual(manifest["owners"][0]["id"], "alpha")

    def test_malformed_nested_shape_uses_manifest_diagnostic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            manifest_path = Path(directory) / "source_owners.toml"
            manifest_path.write_text(
                'schema_version = 1\nowners = ["not-a-table"]\n',
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "routing_manifest_invalid"):
                source_owners.load_and_validate(manifest_path)

    def test_atomic_writer_preserves_target_when_replace_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "SOURCEMAP.md"
            target.write_text("old\n", encoding="utf-8")

            with mock.patch.object(source_owners.os, "replace", side_effect=OSError):
                with self.assertRaises(OSError):
                    source_owners.write_text_atomic(target, "new\n")

            self.assertEqual(target.read_text(encoding="utf-8"), "old\n")
            self.assertEqual(list(Path(directory).glob("*.tmp")), [])


if __name__ == "__main__":
    unittest.main()
