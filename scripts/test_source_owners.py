from pathlib import Path
from unittest import mock
import json
import sys
import tempfile
import unittest

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts import source_owners  # noqa: E402


class SourceOwnersTest(unittest.TestCase):
    def test_source_owners_slice_recipe_allows_relationship_limit_override(self) -> None:
        justfile = (REPO_ROOT / "justfile").read_text(encoding="utf-8")
        recipe = justfile.split("source-owners-slice owner *args:", 1)[1].split(
            "\n\n", 1
        )[0]

        self.assertIn(
            'slice --owner "{{ owner }}" --max-relationships 32 @forwarded_args',
            recipe,
        )

    def test_manifest_validation_and_managed_block_are_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            (root / "AGENTS.md").write_text("instructions\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2

[[owners]]
id = "alpha"
feature_ids = ["alpha-feature"]
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

[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "path:src/lib.rs"
confidence = "compiler_resolved"
evidence = [{ path = "src/lib.rs", symbol = "locate" }]

[[owners.invariants]]
id = "locator-contract"
kind = "semantic"
statement = "The locator remains the runtime entrypoint."
evidence = [{ path = "src/lib.rs", symbol = "locate" }]
tests = ["src/lib.rs"]

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
            self.assertIn("schema=2", block)
            self.assertIn("`alpha`", block)
            self.assertIn("`control_flow:calls`", block)
            self.assertIn("`semantic:locator-contract`", block)

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
                "schema_version = 2\n" + "".join(owners), encoding="utf-8"
            )

            with self.assertRaisesRegex(ValueError, "collision"):
                source_owners.load_and_validate(manifest_path, root)

    def test_custom_manifest_defaults_declared_paths_to_its_directory(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
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
                'schema_version = 2\nowners = ["not-a-table"]\n',
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

    def test_manifest_validation_reuses_equivalent_path_probes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            source = root / "src" / "lib.rs"
            source.write_text("fn locate() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src/lib.rs"]
contracts = ["src/lib.rs"]
tests = ["src/lib.rs"]
primary_entries = [{ path = "src/lib.rs", symbol = "locate" }]
relationships = [{ category = "control_flow", kind = "calls", target = "path:src/lib.rs", confidence = "compiler_resolved", evidence = [{ path = "src/lib.rs", symbol = "locate" }] }]
invariants = [{ id = "stable", kind = "semantic", statement = "Stable.", evidence = [{ path = "src/lib.rs", symbol = "locate" }], tests = ["src/lib.rs"] }]
""",
                encoding="utf-8",
            )
            original_exists = Path.exists
            source_exists_calls = 0

            def tracked_exists(path: Path) -> bool:
                nonlocal source_exists_calls
                if path == source:
                    source_exists_calls += 1
                return original_exists(path)

            with (
                mock.patch.object(
                    source_owners,
                    "confined_path",
                    wraps=source_owners.confined_path,
                ) as resolve_path,
                mock.patch.object(Path, "exists", tracked_exists),
            ):
                source_owners.load_and_validate(manifest_path, root)

            matching_resolutions = [
                call
                for call in resolve_path.call_args_list
                if call.args[1] == "src/lib.rs"
            ]
            self.assertEqual(len(matching_resolutions), 1)
            self.assertEqual(source_exists_calls, 1)

    def test_generate_reads_source_map_once(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text(
                "fn locate() {}\n", encoding="utf-8"
            )
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src/lib.rs"]
""",
                encoding="utf-8",
            )
            source_map = root / "SOURCEMAP.md"
            source_map.write_text("manual prose\n", encoding="utf-8")
            architecture_index = root / "architecture_index.json"
            original_read_text = Path.read_text
            source_map_reads = 0

            def tracked_read_text(path: Path, *args: object, **kwargs: object) -> str:
                nonlocal source_map_reads
                if path == source_map:
                    source_map_reads += 1
                return original_read_text(path, *args, **kwargs)

            argv = [
                "source_owners.py",
                "generate",
                "--manifest",
                str(manifest_path),
                "--source-map",
                str(source_map),
                "--architecture-index",
                str(architecture_index),
                "--repo-root",
                str(root),
            ]
            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch.object(Path, "read_text", tracked_read_text),
            ):
                self.assertEqual(source_owners.main(), 0)

            self.assertEqual(source_map_reads, 1)
            self.assertIn(
                source_owners.BEGIN_PREFIX,
                source_map.read_text(encoding="utf-8"),
            )

    def test_query_is_bounded_revision_keyed_and_includes_incoming_edges(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
feature_ids = ["alpha-feature"]
roots = ["src"]
[[owners.relationships]]
category = "tests_contracts"
kind = "validated_by"
target = "path:src/lib.rs"
confidence = "declared"
evidence = [{ path = "src/lib.rs" }]
[[owners.invariants]]
id = "stable"
kind = "compatibility"
statement = "Stable."
evidence = [{ path = "src/lib.rs" }]
tests = []

[[owners]]
id = "beta"
roots = ["src"]
[[owners.relationships]]
category = "callers_consumers"
kind = "calls"
target = "owner:alpha"
confidence = "compiler_resolved"
evidence = [{ path = "src/lib.rs", symbol = "locate" }]
""",
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)

            with mock.patch.object(
                source_owners, "repository_revision", return_value="revision-1"
            ):
                bounded = source_owners.query_graph(
                    manifest, digest, root, ["alpha"], max_relationships=1
                )
                result = source_owners.query_graph(
                    manifest, digest, root, ["alpha"], max_relationships=2
                )

            self.assertEqual(result["repository_revision"], "revision-1")
            self.assertEqual(bounded["status"], "partial")
            self.assertEqual(bounded["omitted"]["relationships"], 1)
            self.assertTrue(
                any(item["source"] == "owner:beta" for item in result["relationships"])
            )
            self.assertEqual(result["owners"][0]["feature_ids"], ["alpha-feature"])
            self.assertEqual(result["owners"][0]["invariants"][0]["id"], "stable")

    def test_architecture_slice_distinguishes_unknowns_from_bounded_noise(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src"]
primary_entries = [{ path = "src/lib.rs", symbol = "locate" }]
tests = ["src/lib.rs"]
[owners.facet_exclusions]
callers_and_consumers = "No external consumer in this fixture."
configuration_and_gates = "No configuration in this fixture."
generated_artifacts = "No generated output in this fixture."
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "path:src/lib.rs"
confidence = "compiler_resolved"
evidence = [{ path = "src/lib.rs", symbol = "locate" }]
[[owners.invariants]]
id = "stable"
kind = "semantic"
statement = "Stable."
evidence = [{ path = "src/lib.rs", symbol = "locate" }]
tests = ["src/lib.rs"]
""",
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)

            slice_ = source_owners.architecture_slice(manifest, digest, root, ["alpha"])

            self.assertEqual(slice_["material_unknowns"], [])
            self.assertFalse(slice_["truncated"])
            self.assertEqual(slice_["omitted_relationships"], 0)
            self.assertEqual(
                slice_["configuration_and_gates"]["status"], "not_applicable"
            )
            self.assertEqual(
                slice_["control_and_data_flow"]["relationships"][0]["provenance"],
                "exact",
            )
            first_snapshot = slice_["snapshot"]
            (root / "src" / "lib.rs").write_text(
                'fn locate() { println!("changed"); }\n', encoding="utf-8"
            )
            second_snapshot = source_owners.architecture_slice(
                manifest, digest, root, ["alpha"]
            )["snapshot"]
            self.assertNotEqual(first_snapshot, second_snapshot)

    def test_architecture_slice_ranks_task_relevant_edges_within_each_facet(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            for name in ("lib.rs", "critical_cache.rs", "secondary.rs"):
                (root / "src" / name).write_text("fn item() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src"]
primary_entries = [{ path = "src/lib.rs", symbol = "item" }]
tests = ["src/lib.rs"]
[owners.facet_exclusions]
callers_and_consumers = "No consumers in this fixture."
generated_artifacts = "No generated output in this fixture."
invariants = "No invariant in this fixture."
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "path:src/secondary.rs"
confidence = "compiler_resolved"
evidence = [{ path = "src/secondary.rs", symbol = "item" }]
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "path:src/critical_cache.rs"
confidence = "compiler_resolved"
evidence = [{ path = "src/critical_cache.rs", symbol = "item" }]
[[owners.relationships]]
category = "configuration"
kind = "reads_config"
target = "config:settings"
confidence = "declared"
evidence = [{ path = "src/lib.rs", symbol = "item" }]
""",
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)

            slice_ = source_owners.architecture_slice(
                manifest, digest, root, ["alpha"], focus="repair critical cache"
            )

            ranked = slice_["control_and_data_flow"]["relationships"]
            self.assertIn("critical_cache.rs", ranked[0]["target"])
            self.assertIn("secondary.rs", ranked[1]["target"])

            bounded = source_owners.architecture_slice(
                manifest,
                digest,
                root,
                ["alpha"],
                max_relationships=2,
                focus="repair critical cache",
            )
            self.assertEqual(len(bounded["control_and_data_flow"]["relationships"]), 1)
            self.assertEqual(
                len(bounded["configuration_and_gates"]["relationships"]), 1
            )
            self.assertGreater(bounded["omitted_relationships"], 0)

    def test_architecture_index_is_manifest_keyed_and_deterministic(self) -> None:
        manifest, digest = source_owners.load_and_validate(
            source_owners.DEFAULT_MANIFEST, source_owners.REPO_ROOT
        )

        first = source_owners.expected_architecture_index(
            manifest, digest, source_owners.REPO_ROOT
        )
        second = source_owners.expected_architecture_index(
            manifest, digest, source_owners.REPO_ROOT
        )

        self.assertEqual(first, second)
        index = json.loads(first)
        self.assertEqual(index["repository_revision"], f"manifest:{digest}")
        self.assertTrue(all("facet_exclusions" in owner for owner in index["owners"]))

    def test_repository_slices_retain_representative_relationships(self) -> None:
        manifest, digest = source_owners.load_and_validate(
            source_owners.DEFAULT_MANIFEST, source_owners.REPO_ROOT
        )
        cases = [
            (
                ["repository-context-discovery"],
                "nested repository identity and failed instruction reads",
                [
                    ("registration_and_entrypoints", "git_workspace.rs"),
                    ("callers_and_consumers", "session/mod.rs"),
                    ("control_and_data_flow", "agents_md.rs"),
                    ("tests_and_contracts", "agents_md_tests.rs"),
                    ("invariants", "snapshot-scoped-discovery"),
                ],
            ),
            (
                ["feature-registry", "core-agent-runtime"],
                "feature registry runtime wiring and compatibility",
                [
                    ("callers_and_consumers", "core-agent-runtime"),
                    ("registration_and_entrypoints", "features/src/lib.rs"),
                    ("invariants", "feature-key-compatibility"),
                ],
            ),
            (
                ["kd4-capability-manifest"],
                "KD4 capability lifecycle and static reachability evidence",
                [
                    ("configuration_and_gates", "kd4_features.toml"),
                    ("callers_and_consumers", "kd4_perf_snapshot.py"),
                    ("registration_and_entrypoints", "check-kd4-features"),
                    ("tests_and_contracts", "test_check_kd4_features.py"),
                    ("invariants", "capability-evidence-reachability"),
                ],
            ),
            (
                ["app-server-protocol-contracts", "app-server-runtime"],
                "generated protocol source consumer and parity",
                [
                    ("generated_artifacts", "app-server-protocol/schema"),
                    ("callers_and_consumers", "app-server-runtime"),
                    ("registration_and_entrypoints", "app-server-schema-check"),
                    ("invariants", "schema-source-parity"),
                ],
            ),
        ]

        for owners, focus, expectations in cases:
            with self.subTest(owners=owners):
                slice_ = source_owners.architecture_slice(
                    manifest,
                    digest,
                    source_owners.REPO_ROOT,
                    owners,
                    max_relationships=32,
                    focus=focus,
                )
                self.assertFalse(slice_["truncated"])
                self.assertEqual(slice_["omitted_relationships"], 0)
                self.assertEqual(slice_["material_unknowns"], [])
                for facet, needle in expectations:
                    relationships = slice_[facet]["relationships"]
                    self.assertTrue(
                        any(
                            needle in relationship.get("target", "")
                            or needle in relationship.get("evidence", "")
                            for relationship in relationships
                        ),
                        f"{facet} did not contain {needle!r}",
                    )

    def test_runtime_features_and_kd4_capabilities_have_distinct_owners(self) -> None:
        manifest, _ = source_owners.load_and_validate(
            source_owners.DEFAULT_MANIFEST, source_owners.REPO_ROOT
        )
        owners = {owner["id"]: owner for owner in manifest["owners"]}

        runtime_owner = owners["feature-registry"]
        capability_owner = owners["kd4-capability-manifest"]
        self.assertNotIn("kd4_features.toml", runtime_owner["contracts"])
        self.assertIn("kd4_features.toml", capability_owner["contracts"])
        self.assertFalse(
            any(
                relationship["target"] == "config:kd4_features.toml"
                for relationship in runtime_owner.get("relationships", [])
            )
        )
        self.assertTrue(
            any(
                relationship["target"] == "config:kd4_features.toml"
                for relationship in capability_owner.get("relationships", [])
            )
        )

    def test_unknown_relationship_category_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src"]
[[owners.relationships]]
category = "surprising"
kind = "calls"
target = "path:src/lib.rs"
confidence = "declared"
evidence = [{ path = "src/lib.rs" }]
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "unknown relationship category"):
                source_owners.load_and_validate(manifest_path, root)


if __name__ == "__main__":
    unittest.main()
