from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
