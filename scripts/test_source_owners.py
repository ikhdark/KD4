import contextlib
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from collections.abc import Iterator
from pathlib import Path

import tomllib

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.generated_output_lock import source_map_lock

SOURCE_OWNERS_CLI = REPO_ROOT / "scripts" / "source_owners.py"
SOURCE_OWNERS_MANIFEST = REPO_ROOT / "source_owners.toml"
SOURCE_MAP = REPO_ROOT / "SOURCEMAP.md"
ARCHITECTURE_INDEX = REPO_ROOT / "architecture_index.json"
BEGIN_PREFIX = "<!-- BEGIN KD4 SOURCE OWNERS"
MAX_QUERY_RELATIONSHIPS = 64


@contextlib.contextmanager
def temporary_repository() -> Iterator[Path]:
    with tempfile.TemporaryDirectory() as directory:
        yield Path(directory)


def write_fixture(
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


def run_source_owners_cli(
    root: Path,
    command: str,
    *,
    manifest: Path | None = None,
    source_map: Path | None = None,
    architecture_index: Path | None = None,
    include_repo_root: bool = True,
    extra: tuple[str, ...] = (),
    timeout: int = 60,
) -> subprocess.CompletedProcess[str]:
    selected_manifest = manifest or root / "source_owners.toml"
    selected_source_map = source_map or root / "SOURCEMAP.md"
    selected_index = architecture_index or root / "architecture_index.json"
    argv = [
        sys.executable,
        str(SOURCE_OWNERS_CLI),
        command,
        "--manifest",
        str(selected_manifest),
        "--source-map",
        str(selected_source_map),
        "--architecture-index",
        str(selected_index),
    ]
    if include_repo_root:
        argv.extend(["--repo-root", str(root)])
    argv.extend(extra)
    environment = os.environ.copy()
    environment["PYTHONUTF8"] = "1"
    return subprocess.run(
        argv,
        cwd=root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
        timeout=timeout,
        env=environment,
    )


def manifest_text(*owners: str) -> str:
    return "schema_version = 2\n" + "\n".join(owners)


def copy_repository_owner_fixture(destination: Path) -> None:
    manifest = tomllib.loads(SOURCE_OWNERS_MANIFEST.read_text(encoding="utf-8"))
    declared_paths = {
        "source_owners.toml",
        "SOURCEMAP.md",
        "architecture_index.json",
    }
    for owner in manifest["owners"]:
        for field in (
            "roots",
            "instructions",
            "consumers",
            "contracts",
            "generated_mirrors",
            "tests",
        ):
            declared_paths.update(owner.get(field, []))
        declared_paths.update(
            entry["path"] for entry in owner.get("primary_entries", [])
        )
        for relationship in owner.get("relationships", []):
            declared_paths.update(
                evidence["path"] for evidence in relationship.get("evidence", [])
            )
            target = relationship.get("target", "")
            if target.startswith("path:"):
                declared_paths.add(target.removeprefix("path:"))
        for invariant in owner.get("invariants", []):
            declared_paths.update(
                evidence["path"] for evidence in invariant.get("evidence", [])
            )
            declared_paths.update(invariant.get("tests", []))
        declared_paths.update(
            validation.get("cwd", "") for validation in owner.get("validation", [])
        )

    for relative_path in sorted(path for path in declared_paths if path):
        source = REPO_ROOT / relative_path
        target = destination / relative_path
        if source.is_dir():
            target.mkdir(parents=True, exist_ok=True)
        elif source.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)


class SourceOwnersCliTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._repository_fixture = tempfile.TemporaryDirectory()
        cls.repository_root = Path(cls._repository_fixture.name)
        copy_repository_owner_fixture(cls.repository_root)

    @classmethod
    def tearDownClass(cls) -> None:
        cls._repository_fixture.cleanup()

    def assert_cli_success(self, result: subprocess.CompletedProcess[str]) -> None:
        self.assertEqual(
            result.returncode,
            0,
            msg=f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def output_json(self, result: subprocess.CompletedProcess[str]) -> dict:
        self.assert_cli_success(result)
        parsed = json.loads(result.stdout)
        self.assertIsInstance(parsed, dict)
        return parsed

    def test_list_command_exposes_valid_owner_ids_before_slice_through_cli(
        self,
    ) -> None:
        catalog = self.output_json(run_source_owners_cli(self.repository_root, "list"))

        owner_ids = {owner["id"] for owner in catalog["owners"]}
        self.assertIn("source-owner-index", owner_ids)
        self.assertIn("code-mode-protocol-contracts", owner_ids)
        self.assertIn("source_owners.py slice --owner <owner-id>", catalog["next"])
        self.assertLess(len(json.dumps(catalog)), 10_000)

    def test_retired_task_continuity_workflow_has_no_source_owner_through_cli(
        self,
    ) -> None:
        catalog = self.output_json(run_source_owners_cli(self.repository_root, "list"))

        self.assertNotIn(
            "task-continuity-hooks",
            {owner["id"] for owner in catalog["owners"]},
        )

    def test_source_owners_slice_recipe_allows_relationship_limit_override_through_cli(
        self,
    ) -> None:
        environment = os.environ.copy()
        environment["PYTHONUTF8"] = "1"
        result = subprocess.run(
            [
                "just",
                "--justfile",
                str(REPO_ROOT / "justfile"),
                "source-owners-slice",
                "source-owner-index",
                "--manifest",
                str(self.repository_root / "source_owners.toml"),
                "--source-map",
                str(self.repository_root / "SOURCEMAP.md"),
                "--architecture-index",
                str(self.repository_root / "architecture_index.json"),
                "--repo-root",
                str(self.repository_root),
                "--focus",
                "source owner index",
                "--max-relationships",
                "1",
            ],
            cwd=REPO_ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
            timeout=60,
            env=environment,
        )

        slice_ = self.output_json(result)
        retained = sum(
            len(slice_[facet]["relationships"])
            for facet in (
                "control_and_data_flow",
                "callers_and_consumers",
                "configuration_and_gates",
                "registration_and_entrypoints",
                "tests_and_contracts",
                "generated_artifacts",
                "invariants",
            )
        )
        self.assertEqual(retained, 1)

    def test_manifest_validation_and_managed_block_are_deterministic_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(root, "AGENTS.md", "instructions\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
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
"""
                ),
            )
            source_map = write_fixture(root, "SOURCEMAP.md", "manual prose\n")

            first_result = run_source_owners_cli(root, "generate")
            self.assert_cli_success(first_result)
            first_map = source_map.read_bytes()
            first_index = (root / "architecture_index.json").read_bytes()
            second_result = run_source_owners_cli(root, "generate")

            self.assert_cli_success(second_result)
            self.assertEqual(source_map.read_bytes(), first_map)
            self.assertEqual(
                (root / "architecture_index.json").read_bytes(), first_index
            )
            rendered = first_map.decode("utf-8")
            self.assertTrue(rendered.startswith("manual prose\n"))
            self.assertIn("schema=2", rendered)
            self.assertIn("alpha", rendered)
            self.assertIn("control_flow:calls", rendered)
            self.assertIn("semantic:locator-contract", rendered)

    def test_manifest_validation_rejects_stale_evidence_symbols_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn live_symbol() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
[[owners.primary_entries]]
path = "source.rs"
symbol = "removed_symbol"
"""
                ),
            )

            result = run_source_owners_cli(root, "list")

            self.assertEqual(result.returncode, 1)
            self.assertIn("stale symbol evidence", result.stderr)
            self.assertIn("source.rs::removed_symbol", result.stderr)

    def test_targeted_validation_checks_only_the_returned_relationship_closure_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn live_symbol() {}\n")
            manifest = write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]

[[owners]]
id = "beta"
roots = ["unrelated-missing.rs"]
[[owners.relationships]]
category = "callers_consumers"
kind = "calls"
target = "owner:alpha"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "live_symbol" }]
"""
                ),
            )

            targeted = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertEqual(
                [owner["id"] for owner in targeted["owners"]],
                ["alpha"],
            )
            self.assertTrue(
                any(
                    relationship["source"] == "owner:beta"
                    for relationship in targeted["relationships"]
                )
            )

            full = run_source_owners_cli(root, "list")
            self.assertEqual(full.returncode, 1)
            self.assertIn("unrelated-missing.rs", full.stderr)

            manifest.write_text(
                manifest.read_text(encoding="utf-8").replace(
                    'evidence = [{ path = "source.rs", symbol = "live_symbol" }]',
                    'evidence = [{ path = "incoming-missing.rs" }]',
                ),
                encoding="utf-8",
            )
            invalid_closure = run_source_owners_cli(
                root,
                "query",
                extra=("--owner", "alpha"),
            )
            self.assertEqual(invalid_closure.returncode, 1)
            self.assertIn("incoming-missing.rs", invalid_closure.stderr)

    def test_repository_revision_changes_with_supporting_source_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            source = write_fixture(root, "source.rs", "fn live_symbol() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
primary_entries = [{ path = "source.rs", symbol = "live_symbol" }]
"""
                ),
            )

            first = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )
            source.write_text(
                'fn live_symbol() { println!("changed"); }\n',
                encoding="utf-8",
            )
            second = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )

            self.assertNotEqual(
                first["repository_revision"],
                second["repository_revision"],
            )

    def test_alias_collision_requires_explicit_ambiguity_through_cli(self) -> None:
        with temporary_repository() as root:
            write_fixture(
                root,
                "src/lib.rs",
                "fn alpha_entry() {}\nfn beta_entry() {}\n",
            )
            owners = []
            for owner_id in ("alpha", "beta"):
                owners.append(
                    f"""
[[owners]]
id = "{owner_id}"
aliases = ["shared alias"]
roots = ["src"]
primary_entries = [{{ path = "src/lib.rs", symbol = "{owner_id}_entry" }}]
"""
                )
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(*owners),
            )

            result = run_source_owners_cli(root, "list")

            self.assertEqual(result.returncode, 1)
            self.assertIn("phrase collision without explicit ambiguity", result.stderr)

    def test_custom_manifest_defaults_declared_paths_to_its_directory_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            (root / "src").mkdir()
            manifest = write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["src"]
"""
                ),
            )

            catalog = self.output_json(
                run_source_owners_cli(
                    root,
                    "list",
                    manifest=manifest,
                    include_repo_root=False,
                )
            )

            self.assertEqual([owner["id"] for owner in catalog["owners"]], ["alpha"])

    def test_malformed_nested_shape_uses_manifest_diagnostic_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(
                root,
                "source_owners.toml",
                'schema_version = 2\nowners = ["not-a-table"]\n',
            )

            result = run_source_owners_cli(root, "list")

            self.assertEqual(result.returncode, 1)
            self.assertIn("routing_manifest_invalid", result.stderr)
            self.assertIn("owners[0] must be a table", result.stderr)

    def test_atomic_writer_preserves_target_when_replace_fails_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(
                root, "source_owners.toml", "schema_version = 2\nowners = []\n"
            )
            target = write_fixture(root, "SOURCEMAP.md", "old\n")
            (root / ".codex/locks").mkdir(parents=True)
            original = target.read_bytes()
            original_mode = target.stat().st_mode
            parent_mode = root.stat().st_mode
            if os.name == "nt":
                target.chmod(stat.S_IREAD)
            else:
                root.chmod(stat.S_IREAD | stat.S_IEXEC)
            try:
                result = run_source_owners_cli(root, "generate")
            finally:
                if os.name == "nt":
                    target.chmod(original_mode)
                else:
                    root.chmod(parent_mode)

            self.assertEqual(result.returncode, 1)
            self.assertEqual(target.read_bytes(), original)
            self.assertEqual(list(root.glob(".SOURCEMAP.md.*.tmp")), [])
            self.assertFalse((root / "architecture_index.json").exists())

    def test_manifest_validation_reuses_equivalent_path_probes_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["src/lib.rs"]
contracts = ["src/lib.rs"]
tests = ["src/lib.rs"]
primary_entries = [{ path = "src/lib.rs", symbol = "locate" }]
relationships = [{ category = "control_flow", kind = "calls", target = "path:src/lib.rs", confidence = "compiler_resolved", evidence = [{ path = "src/lib.rs", symbol = "locate" }] }]
invariants = [{ id = "stable", kind = "semantic", statement = "Stable.", evidence = [{ path = "src/lib.rs", symbol = "locate" }], tests = ["src/lib.rs"] }]
"""
                ),
            )

            catalog = self.output_json(run_source_owners_cli(root, "list"))

            self.assertEqual([owner["id"] for owner in catalog["owners"]], ["alpha"])

    def test_architecture_index_is_reused_only_for_matching_intact_input_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn locate() {}\n")
            manifest = write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
primary_entries = [{ path = "source.rs", symbol = "locate" }]
"""
                ),
            )
            write_fixture(root, "SOURCEMAP.md", "manual prose\n")
            self.assert_cli_success(run_source_owners_cli(root, "generate"))

            query = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )
            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertEqual(query["owners"][0]["id"], "alpha")
            self.assertIn("registration_and_entrypoints", slice_)

            index_path = root / "architecture_index.json"
            damaged = json.loads(index_path.read_text(encoding="utf-8"))
            damaged["owners"][0]["id"] = "corrupted"
            index_path.write_text(json.dumps(damaged), encoding="utf-8")
            fallback = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertEqual(fallback["owners"][0]["id"], "alpha")

            first_revision = fallback["repository_revision"]
            manifest.write_text(
                manifest.read_text(encoding="utf-8") + "\n",
                encoding="utf-8",
            )
            stale_digest = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertNotEqual(stale_digest["repository_revision"], first_revision)

    def test_slice_snapshot_cache_revalidates_content_identity_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            source = write_fixture(root, "source.rs", "fn locate() { 11111 }\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
primary_entries = [{ path = "source.rs", symbol = "locate" }]
"""
                ),
            )

            first = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )
            timestamps = source.stat()
            source.write_text("fn locate() { 22222 }\n", encoding="utf-8")
            os.utime(
                source,
                ns=(timestamps.st_atime_ns, timestamps.st_mtime_ns),
            )
            changed = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )

            replacement = write_fixture(
                root,
                "replacement.rs",
                "fn locate() { 33333 }\n",
            )
            os.utime(
                replacement,
                ns=(timestamps.st_atime_ns, timestamps.st_mtime_ns),
            )
            os.replace(replacement, source)
            replaced = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )

            self.assertNotEqual(changed["snapshot"], first["snapshot"])
            self.assertNotEqual(replaced["snapshot"], changed["snapshot"])
            self.assertGreater(changed["metrics"]["bytes_read"], 0)
            self.assertGreater(replaced["metrics"]["bytes_read"], 0)

    def test_slice_snapshot_rejects_a_symlink_that_escapes_the_root_through_cli(
        self,
    ) -> None:
        with (
            temporary_repository() as root,
            tempfile.TemporaryDirectory() as outside_directory,
        ):
            source = write_fixture(root, "link.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["link.rs"]
primary_entries = [{ path = "link.rs", symbol = "locate" }]
"""
                ),
            )
            write_fixture(root, "SOURCEMAP.md", "manual prose\n")
            self.assert_cli_success(run_source_owners_cli(root, "generate"))
            outside = Path(outside_directory) / "outside.rs"
            outside.write_text("fn locate() {}\n", encoding="utf-8")
            source.unlink()
            try:
                source.symlink_to(outside)
            except OSError as error:
                self.skipTest(f"symlink creation is unavailable: {error}")

            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )

            missing_digest = hashlib.sha256()
            missing_digest.update(b"link.rs")
            missing_digest.update(b"\0missing-or-not-a-file\0")
            self.assertTrue(slice_["snapshot"].endswith(missing_digest.hexdigest()))
            self.assertEqual(slice_["metrics"]["files_read"], 1)

    def test_bounded_ranking_uses_partial_selection_and_preserves_order_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn locate() {}\n")
            relationships = "\n".join(
                f"""
[[owners.relationships]]
category = "configuration"
kind = "reads_config"
target = "config:rank-{rank:03}"
confidence = "declared"
evidence = [{{ path = "source.rs", symbol = "locate" }}]
"""
                for rank in range(100, 0, -1)
            )
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    f"""
[[owners]]
id = "alpha"
roots = ["source.rs"]
{relationships}
"""
                ),
            )

            graph = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha", "--max-relationships", "4"),
                )
            )

            self.assertEqual(
                [item["target"] for item in graph["relationships"]],
                [f"config:rank-{rank:03}" for rank in range(1, 5)],
            )
            self.assertEqual(graph["status"], "partial")
            self.assertEqual(graph["omitted"]["relationships"], 96)

    def test_round_robin_relationship_cap_preserves_facet_order_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn locate() {}\n")
            write_fixture(root, "test0.rs", "")
            write_fixture(root, "test1.rs", "")
            relationships = "\n".join(
                f"""
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:c{index}"
confidence = "compiler_resolved"
evidence = [{{ path = "source.rs", symbol = "locate" }}]
"""
                for index in range(3)
            )
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    f"""
[[owners]]
id = "alpha"
roots = ["source.rs"]
tests = ["test0.rs", "test1.rs"]
{relationships}
[[owners.invariants]]
id = "i0"
kind = "semantic"
statement = "Stable."
evidence = [{{ path = "source.rs", symbol = "locate" }}]
tests = []
"""
                ),
            )

            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha", "--max-relationships", "4"),
                )
            )

            self.assertEqual(
                [
                    item["target"]
                    for item in slice_["control_and_data_flow"]["relationships"]
                ],
                ["config:c0", "config:c1"],
            )
            self.assertEqual(
                [
                    item["target"]
                    for item in slice_["tests_and_contracts"]["relationships"]
                ],
                ["path:test0.rs"],
            )
            self.assertEqual(
                [item["target"] for item in slice_["invariants"]["relationships"]],
                ["contract:i0"],
            )

    def test_generate_reads_source_map_once_through_cli(self) -> None:
        with temporary_repository() as root:
            write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["src/lib.rs"]
"""
                ),
            )
            source_map = write_fixture(root, "SOURCEMAP.md", "manual prose\n")

            result = run_source_owners_cli(root, "generate")

            self.assert_cli_success(result)
            rendered = source_map.read_text(encoding="utf-8")
            self.assertTrue(rendered.startswith("manual prose\n"))
            self.assertEqual(rendered.count(BEGIN_PREFIX), 1)
            self.assertTrue((root / "architecture_index.json").is_file())
            self.assertTrue((root / ".codex/locks/source-map.lock").is_file())

    def test_generate_respects_shared_source_map_writer_lock_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(
                root, "source_owners.toml", "schema_version = 2\nowners = []\n"
            )
            source_map = write_fixture(root, "SOURCEMAP.md", "manual prose\n")

            with source_map_lock(root, "test-holder"):
                result = run_source_owners_cli(root, "generate")

            self.assertEqual(result.returncode, 1)
            self.assertIn("source map outputs is already locked", result.stderr)
            self.assertEqual(
                source_map.read_text(encoding="utf-8"),
                "manual prose\n",
            )
            self.assertFalse((root / "architecture_index.json").exists())

    def test_query_is_bounded_revision_keyed_and_includes_incoming_edges_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
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
"""
                ),
            )

            bounded = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha", "--max-relationships", "1"),
                )
            )
            result = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha", "--max-relationships", "2"),
                )
            )

            self.assertIn(":sources:", result["repository_revision"])
            self.assertEqual(bounded["status"], "partial")
            self.assertEqual(bounded["omitted"]["relationships"], 1)
            self.assertTrue(
                any(item["source"] == "owner:beta" for item in result["relationships"])
            )
            self.assertEqual(result["owners"][0]["feature_ids"], ["alpha-feature"])
            self.assertEqual(result["owners"][0]["invariants"][0]["id"], "stable")

    def test_architecture_slice_distinguishes_unknowns_from_bounded_noise_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            source = write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
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
"""
                ),
            )

            first = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertEqual(first["material_unknowns"], [])
            self.assertFalse(first["truncated"])
            self.assertEqual(first["omitted_relationships"], 0)
            self.assertEqual(
                first["configuration_and_gates"]["status"],
                "not_applicable",
            )
            self.assertEqual(
                first["control_and_data_flow"]["relationships"][0]["provenance"],
                "exact",
            )

            source.write_text(
                'fn locate() { println!("changed"); }\n',
                encoding="utf-8",
            )
            second = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha"),
                )
            )
            self.assertNotEqual(first["snapshot"], second["snapshot"])

    def test_architecture_slice_ranks_task_relevant_edges_within_each_facet_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            for name in ("lib.rs", "critical_cache.rs", "secondary.rs"):
                write_fixture(root, f"src/{name}", "fn item() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
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
"""
                ),
            )

            ranked = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=(
                        "--owner",
                        "alpha",
                        "--focus",
                        "repair critical cache",
                    ),
                )
            )
            control = ranked["control_and_data_flow"]["relationships"]
            self.assertIn("critical_cache.rs", control[0]["target"])
            self.assertIn("secondary.rs", control[1]["target"])

            bounded = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=(
                        "--owner",
                        "alpha",
                        "--max-relationships",
                        "2",
                        "--focus",
                        "repair critical cache",
                    ),
                )
            )
            self.assertEqual(
                len(bounded["control_and_data_flow"]["relationships"]),
                1,
            )
            self.assertEqual(
                len(bounded["configuration_and_gates"]["relationships"]),
                1,
            )
            self.assertGreater(bounded["omitted_relationships"], 0)

    def test_architecture_slice_ranks_focus_before_relationship_cap_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn item() {}\n")
            relationships = [
                f"""
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:noise-{index:03}"
confidence = "compiler_resolved"
evidence = [{{ path = "source.rs", symbol = "item" }}]
"""
                for index in range(MAX_QUERY_RELATIONSHIPS + 1)
            ]
            relationships.append(
                """
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:zz-critical-cache"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "item" }]
"""
            )
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    f"""
[[owners]]
id = "alpha"
roots = ["source.rs"]
{"".join(relationships)}
"""
                ),
            )

            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=(
                        "--owner",
                        "alpha",
                        "--max-relationships",
                        "1",
                        "--focus",
                        "critical cache",
                    ),
                )
            )

            retained = slice_["control_and_data_flow"]["relationships"]
            self.assertEqual(retained[0]["target"], "config:zz-critical-cache")
            self.assertEqual(
                slice_["omitted_relationships"],
                len(relationships) - 1,
            )

    def test_query_deduplicates_relationships_before_bounding_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn item() {}\n")
            duplicate = """
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:duplicate"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "item" }]
"""
            unique = """
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:unique"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "item" }]
"""
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    f"""
[[owners]]
id = "alpha"
roots = ["source.rs"]
{duplicate * (MAX_QUERY_RELATIONSHIPS + 1)}
{unique}
"""
                ),
            )

            graph = self.output_json(
                run_source_owners_cli(
                    root,
                    "query",
                    extra=("--owner", "alpha", "--max-relationships", "2"),
                )
            )

            self.assertEqual(graph["status"], "complete")
            self.assertEqual(graph["omitted"]["relationships"], 0)
            self.assertEqual(
                {item["target"] for item in graph["relationships"]},
                {"config:duplicate", "config:unique"},
            )

    def test_query_rejects_focus_instead_of_ignoring_it_through_cli(self) -> None:
        with temporary_repository() as root:
            result = run_source_owners_cli(
                root,
                "query",
                extra=("--focus", "exact routing task"),
            )

            self.assertEqual(result.returncode, 2)
            self.assertEqual(
                result.stderr.strip(),
                "--focus is only valid with slice; select an owner with query, "
                "then run slice --owner <id> --focus <task>",
            )

    def test_architecture_slice_counts_actual_manifest_bytes_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            manifest = write_fixture(
                root,
                "custom.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
"""
                ),
            )
            write_fixture(
                root,
                "source_owners.toml",
                "schema_version = 2\nowners = []\n# deliberately different size\n",
            )

            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    manifest=manifest,
                    extra=("--owner", "alpha"),
                )
            )

            self.assertEqual(
                slice_["metrics"]["bytes_read"],
                manifest.stat().st_size,
            )

    def test_architecture_slice_ranks_unicode_focus_terms_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            write_fixture(root, "source.rs", "fn item() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:plain"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "item" }]
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "config:東京"
confidence = "compiler_resolved"
evidence = [{ path = "source.rs", symbol = "item" }]
"""
                ),
            )

            slice_ = self.output_json(
                run_source_owners_cli(
                    root,
                    "slice",
                    extra=("--owner", "alpha", "--focus", "東京"),
                )
            )

            ranked = slice_["control_and_data_flow"]["relationships"]
            self.assertEqual(ranked[0]["target"], "config:東京")

    def test_architecture_index_is_source_keyed_and_deterministic_through_cli(
        self,
    ) -> None:
        with temporary_repository() as root:
            source = write_fixture(root, "source.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["source.rs"]
primary_entries = [{ path = "source.rs", symbol = "locate" }]
"""
                ),
            )
            write_fixture(root, "SOURCEMAP.md", "manual prose\n")

            self.assert_cli_success(run_source_owners_cli(root, "generate"))
            index_path = root / "architecture_index.json"
            first = index_path.read_bytes()
            self.assert_cli_success(run_source_owners_cli(root, "generate"))
            second = index_path.read_bytes()
            self.assertEqual(first, second)
            index = json.loads(first)
            self.assertIn(":sources:", index["repository_revision"])
            self.assertTrue(
                all("facet_exclusions" in owner for owner in index["owners"])
            )

            source.write_text(
                'fn locate() { println!("changed"); }\n',
                encoding="utf-8",
            )
            self.assert_cli_success(run_source_owners_cli(root, "generate"))
            changed = index_path.read_bytes()
            self.assertNotEqual(changed, first)
            self.assertNotEqual(
                json.loads(changed)["repository_revision"],
                index["repository_revision"],
            )

    def test_repository_slices_retain_representative_relationships_through_cli(
        self,
    ) -> None:
        cases = [
            (
                ("repository-context-discovery",),
                "nested repository identity and failed instruction reads",
                (
                    ("registration_and_entrypoints", "git_workspace.rs"),
                    ("callers_and_consumers", "session/mod.rs"),
                    ("control_and_data_flow", "agents_md.rs"),
                    ("tests_and_contracts", "agents_md_tests.rs"),
                    ("invariants", "snapshot-scoped-discovery"),
                ),
            ),
            (
                ("feature-registry", "core-agent-runtime"),
                "feature registry runtime wiring and compatibility",
                (
                    ("callers_and_consumers", "core-agent-runtime"),
                    ("registration_and_entrypoints", "features/src/lib.rs"),
                    ("invariants", "feature-key-compatibility"),
                ),
            ),
            (
                ("kd4-capability-manifest",),
                "KD4 capability lifecycle and static reachability evidence",
                (
                    ("configuration_and_gates", "kd4_features.toml"),
                    ("callers_and_consumers", "kd4_perf_snapshot.py"),
                    ("registration_and_entrypoints", "check-kd4-features"),
                    ("tests_and_contracts", "test_check_kd4_features.py"),
                    ("invariants", "capability-evidence-reachability"),
                ),
            ),
            (
                ("app-server-protocol-contracts", "app-server-runtime"),
                "generated protocol source consumer and parity",
                (
                    ("generated_artifacts", "app-server-protocol/schema"),
                    ("callers_and_consumers", "app-server-runtime"),
                    ("registration_and_entrypoints", "app-server-schema-check"),
                    ("invariants", "schema-source-parity"),
                ),
            ),
            (
                ("completion-proof-certification",),
                "canonical completion certification inventory contracts",
                (
                    ("control_and_data_flow", "scripts/completion_proof.py"),
                    ("callers_and_consumers", "core/src/completion_proof.rs"),
                    ("configuration_and_gates", "completion-proof.toml"),
                    ("registration_and_entrypoints", "justfile"),
                    ("tests_and_contracts", "test_completion_proof_inventory_v2.py"),
                    ("generated_artifacts", "frozen-test-inventory-v1.json"),
                    ("invariants", "canonical-command-gate-separation"),
                    ("invariants", "immutable-baseline-reconciliation"),
                    ("invariants", "cross-language-validation-contract-parity"),
                ),
            ),
            (
                ("repository-maintenance-runners",),
                "maintenance runner registration ownership and validation routes",
                (
                    ("control_and_data_flow", "source-owner-index"),
                    ("callers_and_consumers", "completion-proof-certification"),
                    ("configuration_and_gates", "kd4-rust-tests.toml"),
                    ("registration_and_entrypoints", "justfile"),
                    ("tests_and_contracts", "test_rust_test_runner.py"),
                    ("invariants", "maintenance-runner-route-fidelity"),
                ),
            ),
            (
                ("configuration-generated-contracts",),
                "configuration schema proto generators outputs and consumers",
                (
                    ("control_and_data_flow", "generate-proto.rs"),
                    ("callers_and_consumers", "thread_config/remote.rs"),
                    ("configuration_and_gates", "codex.thread_config.v1.proto"),
                    ("registration_and_entrypoints", "justfile"),
                    ("tests_and_contracts", "test_generate_config_proto.py"),
                    ("generated_artifacts", "core/config.schema.json"),
                    ("generated_artifacts", "codex.thread_config.v1.rs"),
                    ("invariants", "generated-configuration-source-parity"),
                ),
            ),
            (
                ("exec-server-relay-contracts",),
                "exec server relay proto generator exact output and consumers",
                (
                    ("control_and_data_flow", "relay.rs"),
                    ("callers_and_consumers", "relay_proto.rs"),
                    ("registration_and_entrypoints", "justfile"),
                    ("tests_and_contracts", "tests/relay.rs"),
                    ("generated_artifacts", "codex.exec_server.relay.v1.rs"),
                    ("invariants", "relay-proto-source-parity"),
                ),
            ),
        ]

        for owners, focus, expectations in cases:
            with self.subTest(owners=owners):
                owner_arguments = tuple(
                    argument for owner in owners for argument in ("--owner", owner)
                )
                slice_ = self.output_json(
                    run_source_owners_cli(
                        self.repository_root,
                        "slice",
                        extra=(
                            *owner_arguments,
                            "--max-relationships",
                            "32",
                            "--focus",
                            focus,
                        ),
                    )
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

    def test_repository_maintenance_slice_resolves_just_shell_and_build_tooling_test(
        self,
    ) -> None:
        slice_ = self.output_json(
            run_source_owners_cli(
                self.repository_root,
                "slice",
                extra=(
                    "--owner",
                    "repository-maintenance-runners",
                    "--max-relationships",
                    "32",
                    "--focus",
                    "just shell adapter build tooling test ownership",
                ),
            )
        )

        self.assertFalse(slice_["truncated"])
        self.assertEqual(slice_["omitted_relationships"], 0)
        self.assertEqual(slice_["material_unknowns"], [])
        self.assertTrue(
            any(
                relationship["target"] == "path:scripts/just-shell.py::main"
                for relationship in slice_["registration_and_entrypoints"][
                    "relationships"
                ]
            )
        )
        self.assertTrue(
            any(
                relationship["target"] == "path:scripts/test_build_tooling.py"
                for relationship in slice_["tests_and_contracts"]["relationships"]
            )
        )

    def test_repository_maintenance_slice_resolves_readme_toc_cli_and_test(
        self,
    ) -> None:
        slice_ = self.output_json(
            run_source_owners_cli(
                self.repository_root,
                "slice",
                extra=(
                    "--owner",
                    "repository-maintenance-runners",
                    "--max-relationships",
                    "32",
                    "--focus",
                    "README ToC CLI and its subprocess integration tests",
                ),
            )
        )

        self.assertFalse(slice_["truncated"])
        self.assertEqual(slice_["omitted_relationships"], 0)
        self.assertEqual(slice_["material_unknowns"], [])
        self.assertTrue(
            any(
                relationship["target"] == "path:scripts/readme_toc.py::main"
                for relationship in slice_["registration_and_entrypoints"][
                    "relationships"
                ]
            )
        )
        self.assertTrue(
            any(
                relationship["target"] == "path:scripts/test_readme_toc.py"
                for relationship in slice_["tests_and_contracts"]["relationships"]
            )
        )

    def test_workflow_preflight_coordination_slice_resolves_cli_contract_and_consumers(
        self,
    ) -> None:
        slice_ = self.output_json(
            run_source_owners_cli(
                self.repository_root,
                "slice",
                extra=(
                    "--owner",
                    "workflow-preflight-coordination",
                    "--max-relationships",
                    "32",
                    "--focus",
                    "workflow preflight CLI template harness consumers and focused tests",
                ),
            )
        )

        self.assertFalse(slice_["truncated"])
        self.assertEqual(slice_["omitted_relationships"], 0)
        self.assertEqual(slice_["material_unknowns"], [])
        expected_targets = {
            "control_and_data_flow": {"path:scripts/workflow_preflight.py"},
            "callers_and_consumers": {
                "path:justfile",
                "path:.codex/harness/workflow.md",
                "path:.codex/harness/README.md",
            },
            "configuration_and_gates": {
                "config:.codex/harness/templates/PREFLIGHT.json"
            },
            "registration_and_entrypoints": {
                "path:scripts/workflow_preflight.py::main",
                "path:justfile",
            },
            "tests_and_contracts": {
                "path:scripts/test_workflow_preflight.py",
                "path:scripts/test_source_owners.py",
                "contract:.codex/harness/templates/PREFLIGHT.json",
            },
            "invariants": {"contract:workflow-preflight-contract-fidelity"},
        }
        for facet, targets in expected_targets.items():
            with self.subTest(facet=facet):
                self.assertTrue(
                    targets.issubset(
                        {
                            relationship["target"]
                            for relationship in slice_[facet]["relationships"]
                        }
                    )
                )
        self.assertEqual(slice_["generated_artifacts"]["status"], "not_applicable")
        self.assertEqual(slice_["generated_artifacts"]["relationships"], [])

    def test_runtime_features_and_kd4_capabilities_have_distinct_owners_through_cli(
        self,
    ) -> None:
        graph = self.output_json(
            run_source_owners_cli(
                self.repository_root,
                "query",
                extra=(
                    "--owner",
                    "feature-registry",
                    "--owner",
                    "kd4-capability-manifest",
                ),
            )
        )
        owners = {owner["id"]: owner for owner in graph["owners"]}

        self.assertNotIn(
            "kd4_features.toml",
            owners["feature-registry"]["configuration"],
        )
        self.assertIn(
            "kd4_features.toml",
            owners["kd4-capability-manifest"]["configuration"],
        )
        runtime_relationships = [
            relationship
            for relationship in graph["relationships"]
            if relationship["source"] == "owner:feature-registry"
        ]
        capability_relationships = [
            relationship
            for relationship in graph["relationships"]
            if relationship["source"] == "owner:kd4-capability-manifest"
        ]
        self.assertFalse(
            any(
                relationship["target"] == "config:kd4_features.toml"
                for relationship in runtime_relationships
            )
        )
        self.assertTrue(
            any(
                relationship["target"] == "config:kd4_features.toml"
                for relationship in capability_relationships
            )
        )

    def test_unknown_relationship_category_is_rejected_through_cli(self) -> None:
        with temporary_repository() as root:
            write_fixture(root, "src/lib.rs", "fn locate() {}\n")
            write_fixture(
                root,
                "source_owners.toml",
                manifest_text(
                    """
[[owners]]
id = "alpha"
roots = ["src"]
[[owners.relationships]]
category = "surprising"
kind = "calls"
target = "path:src/lib.rs"
confidence = "declared"
evidence = [{ path = "src/lib.rs" }]
"""
                ),
            )

            result = run_source_owners_cli(root, "list")

            self.assertEqual(result.returncode, 1)
            self.assertIn("unknown relationship category", result.stderr)


if __name__ == "__main__":
    unittest.main()
