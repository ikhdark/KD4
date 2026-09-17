import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts import source_owners
from scripts.generated_output_lock import source_map_lock


class SourceOwnersTest(unittest.TestCase):
    def test_slice_focus_resolves_ownership_and_exposes_ambiguity_through_cli(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "alpha").mkdir()
            (root / "beta").mkdir()
            manifest = root / "owners.toml"
            manifest.write_text('''schema_version = 2
[[owners]]
id = "alpha"
roots = ["alpha"]
aliases = ["output routing", "shared label"]
phrases = ["retain final diagnostics"]
ambiguous_with = ["beta"]
[[owners]]
id = "beta"
roots = ["beta"]
aliases = ["shared label"]
ambiguous_with = ["alpha"]
''', encoding="utf-8")
            for focus, extra, expected, status in [
                ("fix output routing", [], ["alpha"], "selected"),
                ("retain final diagnostics after failure", [], ["alpha"], "selected"),
                ("fix `beta/file.rs`", [], ["beta"], "selected"),
                ("shared label", [], ["alpha", "beta"], "ambiguous"),
                ("unrecognized behavior", [], [], "unresolved"),
                ("output routing", ["--owner", "beta"], ["beta"], None),
            ]:
                with self.subTest(focus=focus, extra=extra):
                    completed = subprocess.run([
                        sys.executable, str(REPO_ROOT / "scripts/source_owners.py"), "slice",
                        "--manifest", str(manifest), "--repo-root", str(root),
                        "--architecture-index", str(root / "missing-index.json"),
                        "--focus", focus, *extra,
                    ], text=True, capture_output=True, check=False)
                    self.assertEqual(completed.returncode, 0, completed.stderr)
                    result = json.loads(completed.stdout)
                    self.assertTrue(result["snapshot"].startswith("slice-v4:" + ",".join(expected) + ":routing:"))
                    if status is None:
                        self.assertNotIn("owner_resolution", result)
                    else:
                        self.assertEqual(result["owner_resolution"]["status"], status)
                        self.assertEqual(result["owner_resolution"]["candidates"], expected)
                    if status in {"ambiguous", "unresolved"}:
                        self.assertTrue(any(status in str(item) for item in result["material_unknowns"]))

    def test_compact_slice_cli_preserves_relationships_and_shared_scenario(self) -> None:
        argv = ["source_owners.py", "slice", "--path", "scripts/source_owners.py", "--max-bytes", "10000"]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
            self.assertEqual(source_owners.main(), 0)
        rich_wire = emit.call_args.args[0]
        rich = json.loads(rich_wire)
        with mock.patch.object(sys, "argv", [*argv, "--compact"]), mock.patch("builtins.print") as emit:
            self.assertEqual(source_owners.main(), 0)
        compact_wire = emit.call_args.args[0]
        compact = json.loads(compact_wire)
        self.assertLess(len(compact_wire.encode("utf-8")), len(rich_wire.encode("utf-8")))
        self.assertLessEqual(len((compact_wire + "\n").encode("utf-8")), 10000)
        self.assertEqual(compact["format"], "compact-slice-v1")
        self.assertEqual(compact["snapshot"], rich["snapshot"])
        records = compact["relationship_records"]
        self.assertTrue(records)
        for facet in source_owners.ARCHITECTURE_FACETS:
            if facet in rich:
                expanded = dict(compact[facet])
                expanded["relationships"] = [records[ref] for ref in expanded["relationships"]]
                if expanded.get("representative_scenario") is not None:
                    scenario_ref = expanded["representative_scenario"]
                    self.assertIn(scenario_ref, compact[facet]["relationships"])
                    expanded["representative_scenario"] = records[scenario_ref]
                self.assertEqual(expanded, rich[facet])

    def test_slice_reuses_validation_capture_and_reports_command_reads(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            source = root / "source.rs"
            original = b"fn entry() {}\r\n"
            source.write_bytes(original)
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                'schema_version = 2\n[[owners]]\nid = "alpha"\nroots = ["source.rs"]\n'
                'primary_entries = [{path = "source.rs", symbol = "entry"}]\n', encoding="utf-8"
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            expected_snapshot = source_owners.architecture_slice(manifest, digest, root, ["alpha"])["snapshot"]
            manifest_bytes = manifest_path.stat().st_size
            read_bytes = Path.read_bytes
            reads = []

            def capture(path, *args, **kwargs):
                data = read_bytes(path, *args, **kwargs)
                if path == source:
                    reads.append(path)
                    source.write_bytes(b"fn replacement() {}\n")
                return data

            output = io.StringIO()
            with (
                mock.patch.object(sys, "argv", ["source_owners.py", "slice", "--manifest", str(manifest_path), "--architecture-index", str(root / "missing.json"), "--owner", "alpha"]),
                mock.patch.object(Path, "read_bytes", capture),
                mock.patch.object(sys, "stdout", output),
            ):
                self.assertEqual(source_owners.main(), 0)
            result = json.loads(output.getvalue())
            self.assertEqual(reads, [source])
            self.assertEqual(result["snapshot"], expected_snapshot)
            self.assertEqual(result["metrics"]["command"]["files_read"], 2)
            self.assertEqual(result["metrics"]["command"]["bytes_read"], manifest_bytes + len(original))
            self.assertGreaterEqual(result["metrics"]["command"]["elapsed_ms"], 0)
            self.assertIsNone(result["metrics"]["late_relationship_discoveries"])

    def test_contract_association_requires_the_exact_test_symbol(self) -> None:
        owner = {
            "id": "alpha", "tests": ["tests.py"],
            "relationships": [
                {"category": "tests_contracts", "kind": "validated_by",
                 "target": "path:tests.py", "confidence": "declared",
                 "evidence": [{"path": "tests.py", "symbol": symbol}]}
                for symbol in ["rejects_cancelled_write", "accepts_valid_write"]
            ],
            "invariants": [{
                "id": "cancelled", "kind": "semantic",
                "statement": "Cancelled writes leave the file unchanged.",
                "tests": ["tests.py"],
                "evidence": [{"path": "tests.py", "symbol": "rejects_cancelled_write"}],
            }],
        }
        with tempfile.TemporaryDirectory() as directory:
            result = source_owners.architecture_slice(
                {"owners": [owner]}, "digest", Path(directory), ["alpha"]
            )
            scenarios = {
                item["scenario_symbols"][0]: item
                for item in result["tests_and_contracts"]["relationships"]
                if item.get("scenario_symbols")
            }
            self.assertEqual(scenarios["rejects_cancelled_write"]["behavioral_contracts"],
                             ["Cancelled writes leave the file unchanged."])
            self.assertNotIn("behavioral_contracts", scenarios["accepts_valid_write"])
            owner["invariants"][0]["evidence"] = [{"path": "tests.py"}]
            unbound = source_owners.architecture_slice(
                {"owners": [owner]}, "digest", Path(directory), ["alpha"]
            )
            self.assertFalse(any(item.get("behavioral_contracts") for item in
                                 unbound["tests_and_contracts"]["relationships"]))

    def test_slice_validation_prefers_scenario_then_task_without_inventing_commands(self) -> None:
        unrelated = {"id": "network", "role": "focused_tests", "argv": ["cargo", "test", "network"]}
        focused = {"id": "output", "role": "focused_tests", "argv": ["cargo", "test", "output_budget"]}
        scenario_route = {"id": "effect", "role": "focused_tests", "argv": ["cargo", "test", "submitted_write_changes_file"]}
        manifest = {"owners": [{"id": "alpha", "validation": [unrelated, focused, scenario_route]}]}
        result = source_owners.architecture_slice(manifest, "digest", REPO_ROOT, ["alpha"], focus="output budget")
        self.assertEqual(result["tests_and_contracts"]["focused_validation"], [focused])
        self.assertEqual(result["tests_and_contracts"]["omitted_validation_routes"], 2)
        selected, omitted = source_owners._focused_validation_routes(
            manifest["owners"], {"evidence": "src/effect.rs::submitted_write_changes_file"}, "network"
        )
        self.assertEqual(selected, [scenario_route])
        self.assertEqual(omitted, 2)
        selected, omitted = source_owners._focused_validation_routes(manifest["owners"], None, "unmatched")
        self.assertEqual(selected, [unrelated, focused, scenario_route])
        self.assertEqual(omitted, 0)

    def test_slice_total_byte_budget_preserves_evidence_and_continuation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "lib.rs"
            source.write_text("fn entry() {}", encoding="utf-8")
            manifest = {"owners": [{
                "id": "alpha", "primary_entries": [{"path": "lib.rs", "symbol": "entry"}],
                "invariants": [{"id": f"contract-{index}", "kind": "semantic",
                                "statement": f"Behavior {index}: " + "évidence " * 180,
                                "evidence": [{"path": "lib.rs", "symbol": "entry"}], "tests": []}
                               for index in range(12)],
                "validation": [{"id": "focused", "role": "focused_tests", "argv": ["cargo", "test", "entry"]}],
            }]}
            result = source_owners.architecture_slice(manifest, "digest", root, ["alpha"], max_bytes=10_000)
            wire = (json.dumps(result, indent=2, sort_keys=True) + "\n").encode("utf-8")
            self.assertLessEqual(len(wire), 10_000)
            self.assertTrue(result["truncated"])
            retained = result["invariants"]["relationships"]
            self.assertGreater(len(retained), 0)
            self.assertLess(len(retained), 12)
            self.assertEqual(result["invariants"]["status"], "partial")
            self.assertEqual(result["invariants"]["omitted_relationships"], 12 - len(retained))
            self.assertEqual(result["tests_and_contracts"]["focused_validation"][0]["argv"], ["cargo", "test", "entry"])
            continuation = next(item for item in result["continuations"] if item["facet"] == "invariants")
            self.assertEqual(continuation["offset"], len(retained))
            page = source_owners.architecture_slice(manifest, "digest", root, ["alpha"],
                facet="invariants", offset=continuation["offset"], expected_snapshot=continuation["expected_snapshot"], max_bytes=10_000)
            self.assertTrue(
                {item["target"] for item in retained}.isdisjoint(
                    item["target"]
                    for item in page["invariants"]["relationships"]
                )
            )
            manifest["owners"][0]["invariants"][0]["statement"] = "x" * 40_000
            with self.assertRaisesRegex(ValueError, "cannot fit protected evidence"):
                source_owners.architecture_slice(manifest, "digest", root, ["alpha"], max_bytes=4096)

    def test_slice_cli_byte_budget_includes_index_metadata(self) -> None:
        argv = ["source_owners.py", "slice", "--path", "scripts/source_owners.py", "--max-bytes", "10000"]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
            self.assertEqual(source_owners.main(), 0)
        wire = emit.call_args.args[0]
        self.assertLessEqual(len((wire + "\n").encode("utf-8")), 10000)
        result = json.loads(wire)
        self.assertIn(result["index_status"], {"reused", "fallback"})
        self.assertEqual(result["output_budget_bytes"], 10000)

    def test_slice_path_includes_incoming_file_edges_and_their_freshness(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src/callee").mkdir(parents=True)
            (root / "caller").mkdir()
            (root / "src/callee/lib.rs").write_text("fn callee() {}\n", encoding="utf-8")
            caller = root / "caller/lib.rs"
            caller.write_text("fn caller() { callee(); }\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text('''schema_version = 2
[[owners]]
id = "broad"
roots = ["src"]
[[owners]]
id = "callee"
roots = ["src/callee"]
[[owners]]
id = "caller"
roots = ["caller"]
[[owners.relationships]]
category = "control_flow"
kind = "calls"
target = "path:src/callee/lib.rs"
confidence = "declared"
evidence = [{path = "caller/lib.rs", symbol = "caller"}]
''', encoding="utf-8")
            index_path = root / "architecture_index.json"

            def query() -> dict:
                argv = ["source_owners.py", "slice", "--manifest", str(manifest_path),
                        "--repo-root", str(root), "--architecture-index", str(index_path),
                        "--path", "src/callee/lib.rs"]
                with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
                    self.assertEqual(source_owners.main(), 0)
                return json.loads(emit.call_args.args[0])

            fallback = query()
            edges = fallback["control_and_data_flow"]["relationships"]
            self.assertEqual([(edge["source"], edge["target"]) for edge in edges],
                             [("owner:caller", "path:src/callee/lib.rs")])
            self.assertEqual(fallback["control_and_data_flow"]["status"], "established")
            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            index_path.write_text(source_owners.expected_architecture_index(manifest, digest, root), encoding="utf-8")
            indexed = query()
            self.assertEqual(indexed["index_status"], "reused")
            self.assertEqual(indexed["control_and_data_flow"], fallback["control_and_data_flow"])
            self.assertEqual(indexed["snapshot"], fallback["snapshot"])
            broad = source_owners.query_graph(manifest, digest, root, ["broad"])
            self.assertEqual(broad["relationships"], [])
            caller.write_text("fn caller() { changed(); callee(); }\n", encoding="utf-8")
            self.assertNotEqual(query()["snapshot"], indexed["snapshot"])
            caller.unlink()
            with self.assertRaisesRegex(ValueError, "caller/lib.rs"):
                source_owners.load_and_validate(manifest_path, root, owner_ids=["callee"])

    def test_slice_cli_paginates_every_relationship_and_rejects_changed_sources(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            source = root / "src/lib.rs"
            source.write_text("fn entry() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                'schema_version = 2\n[[owners]]\nid = "alpha"\nroots = ["src"]\n'
                + ''.join(
                    '[[owners.relationships]]\ncategory = "control_flow"\nkind = "calls"\n'
                    f'target = "config:edge-{index:03}"\nconfidence = "declared"\n'
                    'evidence = [{ path = "src/lib.rs", symbol = "entry" }]\n'
                    for index in range(75)
                ),
                encoding="utf-8",
            )

            def query(continuation: dict | None = None) -> dict:
                argv = ["source_owners.py", "slice", "--manifest", str(manifest_path),
                        "--repo-root", str(root), "--owner", "alpha", "--max-relationships", "7"]
                if continuation:
                    argv += ["--facet", continuation["facet"], "--offset", str(continuation["offset"]),
                             "--expected-snapshot", continuation["expected_snapshot"]]
                with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
                    self.assertEqual(source_owners.main(), 0)
                return json.loads(emit.call_args.args[0])

            first = query()
            page = first
            targets = []
            for _ in range(12):
                targets.extend(edge["target"] for edge in page["control_and_data_flow"]["relationships"])
                if not page["continuations"]:
                    break
                self.assertTrue(page["truncated"])
                self.assertEqual(len(page["continuations"]), 1)
                page = query(page["continuations"][0])
                self.assertEqual(page["snapshot"], first["snapshot"])
            self.assertEqual(targets, [f"config:edge-{index:03}" for index in range(75)])
            self.assertFalse(page["truncated"])
            self.assertEqual(page["omitted_relationships"], 0)
            self.assertEqual(page["control_and_data_flow"]["status"], "established")

            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            with self.assertRaisesRegex(ValueError, "requires expected_snapshot"):
                source_owners.architecture_slice(manifest, digest, root, ["alpha"], facet="control_and_data_flow", offset=7)
            source.write_text("fn entry() { changed(); }\n", encoding="utf-8")
            continuation = first["continuations"][0]
            with self.assertRaisesRegex(ValueError, "snapshot changed"):
                source_owners.architecture_slice(manifest, digest, root, ["alpha"], facet=continuation["facet"],
                    offset=continuation["offset"], expected_snapshot=continuation["expected_snapshot"])

    def test_validation_cli_returns_only_the_most_specific_declared_routes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / "src/tools").mkdir(parents=True)
            (root / "unvalidated").mkdir()
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "broad"
roots = ["src"]
[[owners.validation]]
id = "broad-tests"
cwd = "."
argv = ["just", "core-gate", "broad"]
[[owners]]
id = "specific"
roots = ["src/tools"]
[[owners.validation]]
id = "tools-tests"
cwd = "src"
argv = ["just", "core-test-fast", "core_lib", "-E", "test(tools)"]
role = "focused_tests"
[[owners]]
id = "shared"
roots = ["src/tools"]
[[owners.validation]]
id = "shared-contract"
cwd = "."
argv = ["just", "core-gate", "shared"]
role = "generated_contract_check"
[[owners]]
id = "unvalidated"
roots = ["unvalidated"]
""",
                encoding="utf-8",
            )

            def query(paths: list[str], status: int) -> dict:
                argv = [
                    "source_owners.py",
                    "validation",
                    "--manifest",
                    str(manifest_path),
                ]
                for path in paths:
                    argv.extend(["--path", path])
                with (
                    mock.patch.object(sys, "argv", argv),
                    mock.patch("builtins.print") as emit,
                ):
                    self.assertEqual(source_owners.main(), status)
                return json.loads(emit.call_args.args[0])

            expected = [
                {
                    "owner": "shared",
                    "id": "shared-contract",
                    "cwd": ".",
                    "argv": ["just", "core-gate", "shared"],
                    "role": "generated_contract_check",
                },
                {
                    "owner": "specific",
                    "id": "tools-tests",
                    "cwd": "src",
                    "argv": ["just", "core-test-fast", "core_lib", "-E", "test(tools)"],
                    "role": "focused_tests",
                },
            ]
            result = query(["src/tools/new.rs", str(root / "src/tools/second.rs")], 0)
            self.assertEqual(result["status"], "declared")
            self.assertEqual(result["validation"], expected)
            self.assertEqual(result["repository_root"], str(root))
            self.assertEqual(result["unowned_paths"], [])
            self.assertEqual(result["owners_without_validation"], [])

            partial = query(["src/tools/new.rs", "unknown.rs", "unvalidated/new.rs"], 1)
            self.assertEqual(partial["status"], "partial")
            self.assertEqual(partial["validation"], expected)
            self.assertEqual(partial["unowned_paths"], ["unknown.rs"])
            self.assertEqual(partial["owners_without_validation"], ["unvalidated"])
            self.assertEqual(query(["unknown.rs"], 1)["validation"], [])
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "source_owners.py",
                        "validation",
                        "--manifest",
                        str(manifest_path),
                        "--path",
                        "../outside.rs",
                    ],
                ),
                mock.patch("builtins.print") as emit,
            ):
                self.assertEqual(source_owners.main(), 1)
                self.assertIn("outside the repository", str(emit.call_args.args[0]))

    def test_validation_cli_requires_a_scope(self) -> None:
        with (
            mock.patch.object(sys, "argv", ["source_owners.py", "validation"]),
            mock.patch.object(sys, "stderr"),
            self.assertRaises(SystemExit) as error,
        ):
            source_owners.main()
        self.assertEqual(error.exception.code, 2)

    def test_slice_identity_tracks_selected_routing_and_new_incoming_edges(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "selected.rs").write_text("fn selected() {}", encoding="utf-8")
            (root / "unrelated.rs").write_text("fn unrelated() {}", encoding="utf-8")
            selected = {
                "id": "selected",
                "primary_entries": [{"path": "selected.rs", "symbol": "selected"}],
            }
            unrelated = {
                "id": "unrelated",
                "primary_entries": [{"path": "unrelated.rs", "symbol": "unrelated"}],
            }
            manifest = {"owners": [selected, unrelated]}

            def snapshot(digest: str) -> str:
                return source_owners.architecture_slice(
                    manifest, digest, root, ["selected"]
                )["snapshot"]

            original = snapshot("original")
            unrelated["roots"] = ["new-unrelated-root"]
            self.assertEqual(original, snapshot("unrelated-routing-edit"))
            selected["roots"] = ["new-selected-root"]
            rerouted = snapshot("selected-routing-edit")
            self.assertNotEqual(original, rerouted)
            unrelated["relationships"] = [
                {
                    "category": "callers_consumers",
                    "kind": "calls",
                    "target": "owner:selected",
                    "confidence": "declared",
                    "evidence": [{"path": "unrelated.rs", "symbol": "unrelated"}],
                }
            ]
            with_incoming = snapshot("new-incoming-edge")
            self.assertNotEqual(rerouted, with_incoming)
            (root / "unrelated.rs").write_text(
                "fn unrelated() { selected(); }", encoding="utf-8"
            )
            self.assertNotEqual(with_incoming, snapshot("new-incoming-edge"))
            graph = json.loads(
                source_owners.expected_architecture_index(
                    manifest, "new-incoming-edge", root
                )
            )
            indexed = source_owners.architecture_slice(
                manifest, "new-incoming-edge", root, ["selected"], graph=graph
            )
            self.assertEqual(snapshot("new-incoming-edge"), indexed["snapshot"])

    def test_warm_query_reads_only_selected_and_incoming_sources(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("selected", "incoming", "unrelated"):
                (root / f"{name}.rs").write_text(f"fn {name}() {{}}\n")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text("""schema_version = 2
[[owners]]
id = "selected"
roots = ["selected.rs"]
primary_entries = [{ path = "selected.rs", symbol = "selected" }]
[[owners]]
id = "incoming"
roots = ["incoming.rs"]
[[owners.relationships]]
category = "callers_consumers"
kind = "calls"
target = "owner:selected"
confidence = "compiler_resolved"
evidence = [{ path = "incoming.rs", symbol = "incoming" }]
[[owners]]
id = "unrelated"
roots = ["unrelated.rs"]
primary_entries = [{ path = "unrelated.rs", symbol = "unrelated" }]
""")
            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            index_path = root / "architecture_index.json"
            index_path.write_text(
                source_owners.expected_architecture_index(manifest, digest, root)
            )
            argv = [
                "source_owners.py",
                "query",
                "--manifest",
                str(manifest_path),
                "--architecture-index",
                str(index_path),
                "--owner",
                "selected",
                "--max-relationships",
                "1",
            ]
            original_read_bytes = Path.read_bytes
            original_read_text = Path.read_text
            reads = set()

            def read_bytes(path, *args, **kwargs):
                reads.add(path.name)
                return original_read_bytes(path, *args, **kwargs)

            def read_text(path, *args, **kwargs):
                reads.add(path.name)
                return original_read_text(path, *args, **kwargs)

            def query():
                reads.clear()
                with (
                    mock.patch.object(sys, "argv", argv),
                    mock.patch.object(Path, "read_bytes", read_bytes),
                    mock.patch.object(Path, "read_text", read_text),
                    mock.patch("builtins.print") as emit,
                ):
                    self.assertEqual(source_owners.main(), 0)
                self.assertEqual(
                    reads & {"selected.rs", "incoming.rs", "unrelated.rs"},
                    {"selected.rs", "incoming.rs"},
                )
                result = json.loads(emit.call_args.args[0])
                self.assertEqual(result["relationships"][0]["source"], "owner:incoming")
                return result["repository_revision"]

            first = query()
            (root / "unrelated.rs").write_text("unrelated declaration was deleted")
            self.assertEqual(first, query())
            (root / "incoming.rs").write_text("fn incoming() { changed(); }\n")
            self.assertNotEqual(first, query())

    def test_slice_scopes_freshness_and_reports_warm_reads(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("selected.rs", "incoming.rs", "unrelated.rs"):
                (root / name).write_bytes(name.encode())
            manifest = {
                "owners": [
                    {
                        "id": "selected",
                        "primary_entries": [
                            {"path": "selected.rs", "symbol": "selected"}
                        ],
                    },
                    {
                        "id": "incoming",
                        "relationships": [
                            {
                                "category": "callers_consumers",
                                "kind": "calls",
                                "target": "owner:selected",
                                "confidence": "declared",
                                "evidence": [{"path": "incoming.rs"}],
                            }
                        ],
                    },
                    {
                        "id": "unrelated",
                        "primary_entries": [
                            {"path": "unrelated.rs", "symbol": "unrelated"}
                        ],
                    },
                ]
            }
            original_read = Path.read_bytes
            reads = []

            def read(path):
                content = original_read(path)
                reads.append((path.name, len(content)))
                return content

            def capture(digest="manifest"):
                reads.clear()
                with (
                    mock.patch.object(Path, "read_bytes", read),
                    mock.patch.object(
                        source_owners,
                        "_supporting_source_digest",
                        side_effect=AssertionError("slice hashed the whole graph"),
                    ),
                ):
                    result = source_owners.architecture_slice(
                        manifest, digest, root, ["selected"]
                    )
                self.assertEqual(
                    {name for name, _ in reads}, {"selected.rs", "incoming.rs"}
                )
                self.assertEqual(result["metrics"]["files_read"] - 1, len(reads))
                self.assertEqual(
                    result["metrics"]["bytes_read"], sum(size for _, size in reads)
                )
                self.assertTrue(result["snapshot"].startswith("slice-v4:selected:"))
                return result["snapshot"]

            cold = capture()
            self.assertEqual(cold, capture())
            (root / "unrelated.rs").write_bytes(b"unrelated edit")
            self.assertEqual(cold, capture())
            (root / "incoming.rs").write_bytes(b"changed caller")
            incoming = capture()
            self.assertNotEqual(cold, incoming)
            (root / "selected.rs").write_bytes(b"changed implementation")
            selected = capture()
            self.assertNotEqual(incoming, selected)
            self.assertEqual(selected, capture("unrelated manifest digest"))
            manifest["owners"][0]["primary_entries"][0]["symbol"] = "renamed_selected"
            self.assertNotEqual(selected, capture("changed selected owner"))
            index = root / "architecture_index.json"
            index.write_text(
                source_owners.expected_architecture_index(manifest, "manifest", root),
                encoding="utf-8",
            )
            with mock.patch.object(
                source_owners,
                "_supporting_source_digest",
                side_effect=AssertionError("index load hashed unrelated sources"),
            ):
                loaded = source_owners.load_architecture_index(
                    index, "manifest", root, refresh_sources=False
                )
            self.assertIsNotNone(loaded)
            self.assertIsNone(loaded["repository_revision"])

    def test_list_command_exposes_valid_owner_ids_before_slice(self) -> None:
        argv = [
            "source_owners.py",
            "list",
            "--manifest",
            str(source_owners.DEFAULT_MANIFEST),
            "--repo-root",
            str(source_owners.REPO_ROOT),
        ]

        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch("builtins.print") as emit,
            mock.patch.object(source_owners, "MAX_SLICE_RELATIONSHIPS", 17),
            mock.patch.object(
                Path,
                "read_text",
                side_effect=AssertionError("catalog read source files"),
            ),
        ):
            self.assertEqual(source_owners.main(), 0)

        catalog = json.loads(emit.call_args.args[0])
        owner_ids = {owner["id"] for owner in catalog["owners"]}
        self.assertIn("source-owner-index", owner_ids)
        self.assertIn("code-mode-protocol-contracts", owner_ids)
        catalog_by_id = {owner["id"]: owner for owner in catalog["owners"]}
        self.assertEqual(
            catalog_by_id["source-owner-index"]["roots"],
            ["scripts/source_owners.py", "source_owners.toml"],
        )
        self.assertIn("source_owners.py slice --owner <owner-id>", catalog["next"])
        self.assertTrue(catalog["next"].endswith("--max-relationships 17"))
        self.assertLess(len(emit.call_args.args[0]), 10_000)

    def test_slice_resolves_known_path_through_the_cli(self) -> None:
        argv = [
            "source_owners.py",
            "slice",
            "--path",
            "scripts/source_owners.py",
            "--focus",
            "owner routing",
        ]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
            self.assertEqual(source_owners.main(), 0)
        result = json.loads(emit.call_args.args[0])
        self.assertTrue(
            result["snapshot"].startswith("slice-v4:source-owner-index:routing:")
        )

    def test_tool_history_path_resolves_to_recovery_flow_through_the_cli(self) -> None:
        argv = [
            "source_owners.py",
            "slice",
            "--path",
            "codex-rs/core/src/tool_history.rs",
            "--focus",
            "compaction artifact recovery",
        ]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as emit:
            self.assertEqual(source_owners.main(), 0)
        result = json.loads(emit.call_args.args[0])
        self.assertTrue(
            result["snapshot"].startswith("slice-v4:tool-output-recovery:routing:")
        )
        self.assertEqual(result["control_and_data_flow"]["status"], "established")
        self.assertTrue(
            any(
                "artifact_pin_payload_for_items" in relationship["evidence"]
                for relationship in result["control_and_data_flow"]["relationships"]
            )
        )
        self.assertIn(
            "test(tool_history::tests)",
            result["tests_and_contracts"]["focused_validation"][0]["argv"][-1],
        )

    def test_path_routing_preserves_specificity_ties_and_directory_boundaries(
        self,
    ) -> None:
        manifest = {
            "owners": [
                {"id": "broad", "roots": ["src"]},
                {"id": "specific", "roots": ["src/tools"]},
                {"id": "shared", "roots": ["src/tools"]},
            ]
        }
        root = REPO_ROOT.resolve()
        self.assertEqual(
            source_owners.owners_for_paths(manifest, root, ["src/tools/new.rs"]),
            ["shared", "specific"],
        )
        self.assertEqual(
            source_owners.owners_for_paths(manifest, root, ["src/tools_extra.rs"]),
            ["broad"],
        )
        with self.assertRaisesRegex(ValueError, "outside the repository"):
            source_owners.owners_for_paths(manifest, root, ["../outside.rs"])
        with self.assertRaisesRegex(ValueError, "no source owner"):
            source_owners.owners_for_paths(manifest, root, ["unowned/new.rs"])

    def test_path_routing_resolves_shared_owner_roots_once_per_request(self) -> None:
        manifest = {
            "owners": [
                {"id": "broad", "roots": ["src"]},
                {"id": "specific", "roots": ["src/tools"]},
                {"id": "shared", "roots": ["src/tools"]},
            ]
        }
        root = REPO_ROOT.resolve()
        resolve = Path.resolve
        probes: list[Path] = []

        def observe(path: Path, *args, **kwargs) -> Path:
            probes.append(path)
            return resolve(path, *args, **kwargs)

        with mock.patch.object(Path, "resolve", observe):
            self.assertEqual(
                source_owners.owners_for_paths(
                    manifest,
                    root,
                    ["src/tools/first.rs", "src/tools/second.rs", "src/third.rs"],
                ),
                ["broad", "shared", "specific"],
            )
        self.assertEqual(probes.count(root / "src"), 1)
        self.assertEqual(probes.count(root / "src/tools"), 1)

    def test_cli_unowned_paths_preserve_scoped_results_and_expose_index_fallback(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / "owned.rs").write_text("fn entry() {}\n", encoding="utf-8")
            (root / "unowned").mkdir()
            (root / "unowned/new.rs").write_text("fn other() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                'schema_version = 2\n[[owners]]\nid = "alpha"\nroots = ["owned.rs"]\n'
                'primary_entries = [{ path = "owned.rs", symbol = "entry" }]\n',
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            index_path = root / "architecture_index.json"
            index_text = source_owners.expected_architecture_index(
                manifest, digest, root
            )
            stale_index_text = source_owners.expected_architecture_index(
                manifest, "stale", root
            )

            for index_kind, index_content in (
                ("missing", None),
                ("invalid", "{"),
                ("stale", stale_index_text),
                ("current", index_text),
            ):
                if index_content is not None:
                    index_path.write_text(index_content, encoding="utf-8")
                for command in ("query", "slice"):
                    for mixed in (False, True):
                        with self.subTest(
                            index=index_kind, command=command, mixed=mixed
                        ):
                            argv = [
                                "source_owners.py",
                                command,
                                "--manifest",
                                str(manifest_path),
                                "--architecture-index",
                                str(index_path),
                                "--path",
                                "unowned/new.rs",
                            ]
                            if mixed:
                                argv.extend(["--path", "owned.rs"])
                            with (
                                mock.patch.object(sys, "argv", argv),
                                mock.patch("builtins.print") as emit,
                            ):
                                self.assertEqual(source_owners.main(), 0)
                            result = json.loads(emit.call_args.args[0])
                            self.assertEqual(
                                result["index_status"],
                                "reused" if index_kind == "current" else "fallback",
                            )
                            self.assertEqual(
                                "index_hint" in result, index_kind != "current"
                            )
                            self.assertEqual(
                                result["unowned_paths"], ["unowned/new.rs"]
                            )
                            self.assertEqual(
                                result["fallback_searches"],
                                [
                                    {
                                        "cwd": str(root),
                                        "argv": ["rg", "--files", "--", "unowned"],
                                    }
                                ],
                            )
                            self.assertIn(
                                "No declared owner for unowned/new.rs; inspect its directory before mutation.",
                                result["material_unknowns"],
                            )
                            if command == "query":
                                self.assertEqual(result["status"], "partial")
                                self.assertEqual(
                                    [owner["id"] for owner in result["owners"]],
                                    ["alpha"] if mixed else [],
                                )
                                self.assertEqual(result["relationships"], [])
                            else:
                                entries = result["registration_and_entrypoints"]
                                self.assertEqual(
                                    entries["status"], "partial" if mixed else "unknown"
                                )
                                self.assertEqual(
                                    [
                                        entry["target"]
                                        for entry in entries["relationships"]
                                    ],
                                    ["path:owned.rs::entry"] if mixed else [],
                                )
                                self.assertEqual(
                                    result["tests_and_contracts"]["focused_validation"],
                                    [],
                                )
                                self.assertEqual(
                                    result["metrics"]["files_read"], 1
                                )
                                # Declaration validation already read owned.rs;
                                # slice hashing reuses those bytes. Command metrics
                                # must still count that read and any index read.
                                self.assertEqual(
                                    result["metrics"]["command"]["files_read"],
                                    1 + int(mixed) + int(index_kind != "missing"),
                                )

            argv = [
                "source_owners.py",
                "slice",
                "--manifest",
                str(manifest_path),
                "--path",
                "../outside.rs",
            ]
            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch("builtins.print") as emit,
            ):
                self.assertEqual(source_owners.main(), 1)
                self.assertIn("outside the repository", str(emit.call_args.args[0]))

    def test_retired_task_continuity_workflow_has_no_source_owner(self) -> None:
        manifest, _ = source_owners.load_and_validate(
            source_owners.DEFAULT_MANIFEST, source_owners.REPO_ROOT
        )
        owners = {owner["id"]: owner for owner in manifest["owners"]}

        self.assertNotIn("task-continuity-hooks", owners)

    def test_source_owners_slice_recipe_allows_relationship_limit_override(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src/lib.rs").write_text("fn entry() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                'schema_version = 2\n[[owners]]\nid = "alpha"\nroots = ["src"]\n'
                + ''.join(
                    '[[owners.relationships]]\ncategory = "control_flow"\nkind = "calls"\n'
                    f'target = "config:edge-{index}"\nconfidence = "declared"\n'
                    'evidence = [{ path = "src/lib.rs", symbol = "entry" }]\n'
                    for index in range(3)
                ),
                encoding="utf-8",
            )
            for selector in (["alpha"], ["--path", "src/lib.rs"]):
                with self.subTest(selector=selector):
                    completed = subprocess.run(
                        ["just", "--justfile", str(REPO_ROOT / "justfile"), "source-owners-slice",
                         *selector, "--manifest", str(manifest_path), "--repo-root", str(root),
                         "--max-relationships", "1"],
                        capture_output=True, text=True, check=True,
                    )
                    result = json.loads(completed.stdout)
                    self.assertEqual(
                        [edge["target"] for edge in result["control_and_data_flow"]["relationships"]],
                        ["config:edge-0"],
                    )
                    self.assertEqual(result["omitted_relationships"], 2)
                    self.assertEqual(result["continuations"][0]["offset"], 1)

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

    def test_distinct_file_entrypoints_are_unambiguous_in_generated_index(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("alpha", "beta"):
                (root / f"{name}.rs").write_text("fn main() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                "schema_version = 2\n"
                + "\n".join(
                    f'[[owners]]\nid = "{name}"\nroots = ["{name}.rs"]\n'
                    f'primary_entries = [{{ path = "{name}.rs", symbol = "main" }}]\n'
                    for name in ("alpha", "beta")
                ),
                encoding="utf-8",
            )

            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            index = json.loads(
                source_owners.expected_architecture_index(manifest, digest, root)
            )

            self.assertEqual(
                {owner["id"]: owner["primary_entries"] for owner in index["owners"]},
                {
                    "alpha": [{"path": "alpha.rs", "symbol": "main"}],
                    "beta": [{"path": "beta.rs", "symbol": "main"}],
                },
            )

    def test_shared_file_entrypoint_requires_explicit_ambiguity_for_every_owner(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "shared.rs").write_text("fn main() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            for alpha, beta in (
                (False, False),
                (True, False),
                (False, True),
                (True, True),
            ):
                with self.subTest(alpha=alpha, beta=beta):
                    manifest_path.write_text(
                        "schema_version = 2\n"
                        + "\n".join(
                            f'[[owners]]\nid = "{name}"\nroots = ["shared.rs"]\n'
                            'primary_entries = [{ path = "shared.rs", symbol = "main", '
                            f"ambiguous = {str(ambiguous).lower()} }}]\n"
                            for name, ambiguous in (("alpha", alpha), ("beta", beta))
                        ),
                        encoding="utf-8",
                    )
                    if alpha and beta:
                        manifest, digest = source_owners.load_and_validate(
                            manifest_path, root
                        )
                        index = json.loads(
                            source_owners.expected_architecture_index(
                                manifest, digest, root
                            )
                        )
                        self.assertEqual(len(index["owners"]), 2)
                        self.assertTrue(
                            all(
                                owner["primary_entries"][0]["ambiguous"]
                                for owner in index["owners"]
                            )
                        )
                    else:
                        with self.assertRaisesRegex(
                            ValueError,
                            "entry symbol is not explicitly ambiguous: shared.rs::main",
                        ):
                            source_owners.load_and_validate(manifest_path, root)

    def test_manifest_validation_rejects_stale_evidence_symbols(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_text("fn live_symbol() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["source.rs"]
[[owners.primary_entries]]
path = "source.rs"
symbol = "removed_symbol"
""",
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "stale symbol evidence"):
                source_owners.load_and_validate(manifest_path, root)

    def test_primary_entry_requires_a_declaration_or_reexport(self) -> None:
        cases = [
            ("rs", "// fn entry() {}\nfn other() {}", False),
            ("rs", "/* outer /* nested */ fn entry() {} */", False),
            ("rs", 'const DOC: &str = "fn entry() {}";', False),
            ("rs", 'const DOC: &str = r##"fn entry() {}"##;', False),
            ("rs", "fn test_entry() { entry(); }", False),
            ("rs", "pub use other::entry as renamed;", False),
            ("rs", "pub async fn entry() {}", True),
            ("rs", "pub struct entry;", True),
            ("rs", "pub use other::entry;", True),
            ("rs", "pub use other::{first, entry};", True),
            ("rs", "pub use other::original as entry;", True),
            ("py", "# def entry(): pass\ndef other(): pass", False),
            ("py", '"""def entry(): pass"""', False),
            ("py", "def test_entry():\n    entry()", False),
            ("py", "async def entry(): pass", True),
            ("py", "class entry: pass", True),
            ("py", "from other import original as entry", True),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_path = root / "source_owners.toml"
            for suffix, contents, valid in cases:
                with self.subTest(suffix=suffix, contents=contents):
                    path = f"source.{suffix}"
                    (root / path).write_text(contents, encoding="utf-8")
                    manifest_path.write_text(
                        'schema_version = 2\n[[owners]]\nid = "alpha"\n'
                        f'roots = ["{path}"]\n'
                        f'primary_entries = [{{ path = "{path}", symbol = "entry" }}]\n',
                        encoding="utf-8",
                    )
                    if valid:
                        manifest, _ = source_owners.load_and_validate(
                            manifest_path, root
                        )
                        self.assertEqual(
                            manifest["owners"][0]["primary_entries"],
                            [{"path": path, "symbol": "entry"}],
                        )
                    else:
                        with self.assertRaisesRegex(
                            ValueError, "stale symbol evidence"
                        ):
                            source_owners.load_and_validate(manifest_path, root)

    def test_targeted_validation_checks_only_the_returned_relationship_closure(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_text("fn live_symbol() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
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
""",
                encoding="utf-8",
            )

            manifest, _ = source_owners.load_and_validate(
                manifest_path, root, owner_ids=["alpha"]
            )
            self.assertEqual(
                [owner["id"] for owner in manifest["owners"]], ["alpha", "beta"]
            )
            with self.assertRaisesRegex(ValueError, "unrelated-missing.rs"):
                source_owners.load_and_validate(manifest_path, root)

            manifest_path.write_text(
                manifest_path.read_text(encoding="utf-8").replace(
                    'evidence = [{ path = "source.rs", symbol = "live_symbol" }]',
                    'evidence = [{ path = "incoming-missing.rs" }]',
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(ValueError, "incoming-missing.rs"):
                source_owners.load_and_validate(
                    manifest_path, root, owner_ids=["alpha"]
                )

    def test_repository_revision_changes_with_supporting_source(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_text("fn live_symbol() {}\n", encoding="utf-8")
            manifest = {
                "owners": [
                    {
                        "id": "alpha",
                        "primary_entries": [
                            {"path": "source.rs", "symbol": "live_symbol"}
                        ],
                    }
                ]
            }

            first = source_owners.repository_revision(root, "manifest", manifest)
            source.write_text(
                'fn live_symbol() { println!("changed"); }\n', encoding="utf-8"
            )
            second = source_owners.repository_revision(root, "manifest", manifest)

            self.assertNotEqual(first, second)

    def test_alias_collision_requires_explicit_ambiguity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text(
                "fn alpha_entry() {}\nfn beta_entry() {}\n", encoding="utf-8"
            )
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

            with (
                mock.patch.object(source_owners.os, "replace", side_effect=OSError),
                self.assertRaises(OSError),
            ):
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
            original_stat = Path.stat
            source_stat_calls = 0

            def tracked_stat(path: Path, *args: object, **kwargs: object) -> object:
                nonlocal source_stat_calls
                if path == source:
                    source_stat_calls += 1
                return original_stat(path, *args, **kwargs)

            with (
                mock.patch.object(
                    source_owners,
                    "confined_path",
                    wraps=source_owners.confined_path,
                ) as resolve_path,
                mock.patch.object(Path, "stat", tracked_stat),
            ):
                source_owners.load_and_validate(manifest_path, root)

            matching_resolutions = [
                call
                for call in resolve_path.call_args_list
                if call.args[1] == "src/lib.rs"
            ]
            self.assertEqual(len(matching_resolutions), 1)
            self.assertTrue(
                all(
                    call.args[0] == root.resolve()
                    for call in resolve_path.call_args_list
                )
            )
            self.assertEqual(source_stat_calls, 1)

    def test_architecture_index_is_reused_only_for_matching_intact_input(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_text("fn locate() {}\n", encoding="utf-8")
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["source.rs"]
primary_entries = [{ path = "source.rs", symbol = "locate" }]
""",
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)
            index_path = root / "architecture_index.json"
            index_text = source_owners.expected_architecture_index(
                manifest, digest, root
            )
            index_path.write_text(index_text, encoding="utf-8")

            loaded = source_owners.load_architecture_index(index_path, digest, root)
            self.assertIsNotNone(loaded)
            self.assertIsNone(
                source_owners.load_architecture_index(index_path, "stale", root)
            )

            damaged = json.loads(index_text)
            damaged["owners"][0]["id"] = "corrupted"
            index_path.write_text(json.dumps(damaged), encoding="utf-8")
            self.assertIsNone(
                source_owners.load_architecture_index(index_path, digest, root)
            )

            index_path.write_text(index_text, encoding="utf-8")
            argv = [
                "source_owners.py",
                "query",
                "--manifest",
                str(manifest_path),
                "--architecture-index",
                str(index_path),
                "--repo-root",
                str(root),
                "--owner",
                "alpha",
            ]
            with (
                mock.patch.object(sys, "argv", argv),
                mock.patch("builtins.print") as print_output,
            ):
                self.assertEqual(source_owners.main(), 0)
                argv[1] = "slice"
                self.assertEqual(source_owners.main(), 0)
            result = json.loads(print_output.call_args.args[0])
            self.assertEqual(
                result["registration_and_entrypoints"]["relationships"][0]["target"],
                "path:source.rs::locate",
            )
            source.write_text("fn renamed() {}\n", encoding="utf-8")
            for command in ("query", "slice"):
                argv[1] = command
                with (
                    self.subTest(command=command),
                    mock.patch.object(sys, "argv", argv),
                    mock.patch("builtins.print") as diagnostic,
                ):
                    self.assertEqual(source_owners.main(), 1)
                    self.assertIn(
                        "stale symbol evidence", str(diagnostic.call_args.args[0])
                    )

    def test_slice_snapshot_revalidates_content_identity(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source.rs"
            source.write_bytes(b"first")
            graph = {"relationships": []}
            owners = [{"primary_entries": [{"path": "source.rs"}]}]

            first = source_owners._slice_source_snapshot(root, graph, owners)
            warm = source_owners._slice_source_snapshot(root, graph, owners)
            self.assertEqual(first[0], warm[0])
            self.assertEqual(warm[1:], (1, len(b"first")))

            timestamps = source.stat()
            source.write_bytes(b"other")
            source.touch()
            source_owners.os.utime(
                source,
                ns=(timestamps.st_atime_ns, timestamps.st_mtime_ns),
            )
            changed = source_owners._slice_source_snapshot(root, graph, owners)
            self.assertNotEqual(changed[0], first[0])
            self.assertEqual(changed[1:], (1, len(b"other")))

            replacement = root / "replacement.rs"
            replacement.write_bytes(b"third")
            source_owners.os.utime(
                replacement,
                ns=(timestamps.st_atime_ns, timestamps.st_mtime_ns),
            )
            replacement.replace(source)
            replaced = source_owners._slice_source_snapshot(root, graph, owners)
            self.assertNotEqual(replaced[0], changed[0])
            self.assertEqual(replaced[1:], (1, len(b"third")))

    def test_slice_snapshot_frames_file_contents(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first, second = root / "a.rs", root / "b.rs"
            manifest = {
                "owners": [
                    {
                        "id": "sample",
                        "primary_entries": [
                            {"path": "a.rs", "symbol": "a"},
                            {"path": "b.rs", "symbol": "b"},
                        ],
                    }
                ]
            }
            header = b"b.rs\0file\0"
            first.write_bytes(b"left" + header)
            second.write_bytes(b"right")
            before = source_owners.architecture_slice(
                manifest, "manifest", root, ["sample"]
            )["snapshot"]
            first.write_bytes(b"left")
            second.write_bytes(header + b"right")
            after = source_owners.architecture_slice(
                manifest, "manifest", root, ["sample"]
            )["snapshot"]
            self.assertNotEqual(before, after)

    def test_slice_snapshot_rejects_a_symlink_that_escapes_the_root(self) -> None:
        with (
            tempfile.TemporaryDirectory() as directory,
            tempfile.TemporaryDirectory() as outside_directory,
        ):
            root = Path(directory)
            outside = Path(outside_directory) / "outside.rs"
            outside.write_bytes(b"outside")
            link = root / "link.rs"
            try:
                link.symlink_to(outside)
            except OSError as error:
                self.skipTest(f"symlink creation is unavailable: {error}")

            snapshot = source_owners._slice_source_snapshot(
                root,
                {"relationships": []},
                [{"primary_entries": [{"path": "link.rs"}]}],
            )

            missing_digest = source_owners.hashlib.sha256()
            missing_digest.update(b"link.rs")
            missing_digest.update(b"\0missing-or-not-a-file\0")
            self.assertEqual(snapshot, (missing_digest.hexdigest(), 0, 0))

    def test_generate_reads_source_map_once(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "src" / "lib.rs").write_text("fn locate() {}\n", encoding="utf-8")
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
            self.assertTrue((root / ".codex" / "locks" / "source-map.lock").is_file())
            self.assertIn(
                source_owners.BEGIN_PREFIX,
                source_map.read_text(encoding="utf-8"),
            )

    def test_generate_respects_shared_source_map_writer_lock(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text("schema_version = 2\n", encoding="utf-8")
            source_map = root / "SOURCEMAP.md"
            source_map.write_text("manual prose\n", encoding="utf-8")
            architecture_index = root / "architecture_index.json"
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
                source_map_lock(root, "test-holder"),
                mock.patch.object(sys, "argv", argv),
            ):
                self.assertEqual(source_owners.main(), 1)
            self.assertEqual(source_map.read_text(encoding="utf-8"), "manual prose\n")
            self.assertFalse(architecture_index.exists())

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

            bounded = source_owners.query_graph(
                manifest, digest, root, ["alpha"], max_relationships=1
            )
            result = source_owners.query_graph(
                manifest, digest, root, ["alpha"], max_relationships=2
            )
            self.assertEqual(
                result["repository_revision"], bounded["repository_revision"]
            )
            (root / "src" / "lib.rs").write_text("fn locate() { changed(); }\n")
            changed = source_owners.query_graph(manifest, digest, root, ["alpha"])
            self.assertNotEqual(
                result["repository_revision"], changed["repository_revision"]
            )
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
            (root / "src" / "effect.rs").write_text(
                "fn submitted_write_changes_file() {}\n", encoding="utf-8"
            )
            manifest_path = root / "source_owners.toml"
            manifest_path.write_text(
                """schema_version = 2
[[owners]]
id = "alpha"
roots = ["src"]
primary_entries = [{ path = "src/lib.rs", symbol = "locate" }]
tests = ["src/lib.rs", "src/effect.rs"]
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
[[owners.relationships]]
category = "tests_contracts"
kind = "validated_by"
target = "path:src/effect.rs"
confidence = "declared"
evidence = [{ path = "src/effect.rs", symbol = "submitted_write_changes_file" }]
[[owners.invariants]]
id = "stable"
kind = "semantic"
statement = "A submitted write changes the consumer's file."
evidence = [{ path = "src/lib.rs", symbol = "locate" }, { path = "src/effect.rs", symbol = "submitted_write_changes_file" }]
tests = ["src/effect.rs"]
[[owners.validation]]
id = "effect-test"
cwd = "."
argv = ["cargo", "test", "submitted_write_changes_file"]
role = "focused_tests"
""",
                encoding="utf-8",
            )
            manifest, digest = source_owners.load_and_validate(manifest_path, root)

            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "source_owners.py",
                        "slice",
                        "--owner",
                        "alpha",
                        "--focus",
                        "lib",
                        "--manifest",
                        str(manifest_path),
                        "--repo-root",
                        str(root),
                    ],
                ),
                mock.patch("builtins.print") as emit,
            ):
                self.assertEqual(source_owners.main(), 0)
            slice_ = json.loads(emit.call_args.args[0])
            invariant = slice_["invariants"]["relationships"][0]
            self.assertEqual(invariant["invariant_kind"], "semantic")
            self.assertEqual(
                invariant["statement"], "A submitted write changes the consumer's file."
            )
            scenario = slice_["tests_and_contracts"]["representative_scenario"]
            self.assertEqual(scenario["target"], "path:src/effect.rs")
            self.assertEqual(
                scenario["evidence"], "src/effect.rs::submitted_write_changes_file"
            )
            self.assertEqual(
                scenario["behavioral_contracts"],
                ["A submitted write changes the consumer's file."],
            )
            self.assertEqual(
                slice_["tests_and_contracts"]["focused_validation"][0]["argv"],
                ["cargo", "test", "submitted_write_changes_file"],
            )

            self.assertEqual(slice_["material_unknowns"], [])
            self.assertIsNone(slice_["metrics"]["late_relationship_discoveries"])
            self.assertFalse(slice_["truncated"])
            self.assertEqual(slice_["omitted_relationships"], 0)
            self.assertEqual(
                slice_["configuration_and_gates"]["status"], "not_applicable"
            )
            self.assertEqual(
                slice_["control_and_data_flow"]["relationships"][0]["provenance"],
                "declared",
            )
            self.assertIsNone(slice_["metrics"]["late_relationship_discoveries"])
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
[[owners.invariants]]
id = "stable"
kind = "semantic"
statement = "Retain the behavioral contract when capping relationships."
evidence = [{ path = "src/lib.rs", symbol = "item" }]
tests = ["src/lib.rs"]
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

            for cap, control_count, entry_count, test_count, invariant_count in (
                (2, 1, 0, 0, 0),
                (4, 1, 1, 1, 0),
                (5, 1, 1, 1, 1),
                (6, 2, 1, 1, 1),
            ):
                with self.subTest(cap=cap):
                    bounded = source_owners.architecture_slice(
                        manifest,
                        digest,
                        root,
                        ["alpha"],
                        max_relationships=cap,
                        focus="repair critical cache",
                    )
                    self.assertEqual(
                        [
                            edge["target"]
                            for edge in bounded["control_and_data_flow"][
                                "relationships"
                            ]
                        ],
                        ["path:src/critical_cache.rs", "path:src/secondary.rs"][
                            :control_count
                        ],
                    )
                    self.assertEqual(
                        [
                            edge["target"]
                            for edge in bounded["configuration_and_gates"][
                                "relationships"
                            ]
                        ],
                        ["config:settings"],
                    )
                    self.assertEqual(
                        len(bounded["registration_and_entrypoints"]["relationships"]),
                        entry_count,
                    )
                    for facet in source_owners.ARCHITECTURE_FACETS:
                        omitted = len(slice_[facet]["relationships"]) - len(
                            bounded[facet]["relationships"]
                        )
                        self.assertEqual(
                            bounded[facet].get("omitted_relationships", 0), omitted
                        )
                        if omitted:
                            self.assertEqual(
                                bounded[facet]["status"],
                                "partial"
                                if bounded[facet]["relationships"]
                                else "unknown",
                            )
                    self.assertEqual(
                        len(bounded["tests_and_contracts"]["relationships"]), test_count
                    )
                    self.assertEqual(
                        [
                            edge["target"]
                            for edge in bounded["invariants"]["relationships"]
                        ],
                        ["contract:stable"][:invariant_count],
                    )
                    self.assertEqual(bounded["omitted_relationships"], 6 - cap)
                    self.assertEqual(bounded["truncated"], cap < 6)
                    self.assertEqual(
                        bounded["invariants"]["status"],
                        "established" if invariant_count else "unknown",
                    )
                    self.assertEqual(
                        bounded["control_and_data_flow"]["status"],
                        "established" if control_count == 2 else "partial",
                    )

    def test_slice_missing_owner_declarations_are_partial_or_unknown(self) -> None:
        manifest = {
            "owners": [
                {"id": "alpha", "primary_entries": [{"path": "a.rs", "symbol": "a"}]},
                {"id": "beta"},
            ]
        }
        with tempfile.TemporaryDirectory() as directory:
            result = source_owners.architecture_slice(
                manifest, "digest", Path(directory), ["alpha", "beta"]
            )
        self.assertEqual(result["registration_and_entrypoints"]["status"], "partial")
        self.assertEqual(result["control_and_data_flow"]["status"], "unknown")
        self.assertIn(
            "registration_and_entrypoints: missing declarations for beta",
            result["material_unknowns"],
        )
        self.assertEqual(result["control_and_data_flow"]["relationships"], [])

    def test_slice_behavior_relevance_outranks_kind_and_uses_spare_budget(self) -> None:
        def edge(category: str, kind: str, name: str) -> dict:
            return {
                "category": category,
                "kind": kind,
                "target": f"path:{name}.rs",
                "confidence": "declared",
                "evidence": [{"path": f"{name}.rs"}],
            }

        manifest = {
            "owners": [
                {
                    "id": "alpha",
                    "relationships": [
                        edge("control_flow", "calls", "irrelevant"),
                        edge("control_flow", "emits", "cancellation"),
                        edge("callers_consumers", "consumed_by", "cancellation_one"),
                        edge("callers_consumers", "consumed_by", "cancellation_two"),
                        edge("callers_consumers", "consumed_by", "cancellation_three"),
                        {
                            **edge("tests_contracts", "validated_by", "z"),
                            "evidence": [{"path": "z.rs", "symbol": "effect"}],
                        },
                        {
                            **edge("tests_contracts", "validated_by", "a"),
                            "evidence": [{"path": "a.rs", "symbol": "effect"}],
                        },
                    ],
                    "invariants": [
                        {
                            "id": name,
                            "kind": "semantic",
                            "statement": statement,
                            "evidence": [{"path": f"{name}.rs", "symbol": "effect"}],
                            "tests": [f"{name}.rs"],
                        }
                        for name, statement in (
                            ("a", "Unrelated behavior stays stable."),
                            ("z", "Cancellation prevents updates."),
                        )
                    ],
                }
            ]
        }
        with tempfile.TemporaryDirectory() as directory:
            result = source_owners.architecture_slice(
                manifest,
                "digest",
                Path(directory),
                ["alpha"],
                max_relationships=6,
                focus="cancellation",
            )
        self.assertEqual(
            [
                edge["target"]
                for edge in result["control_and_data_flow"]["relationships"]
            ],
            ["path:cancellation.rs"],
        )
        self.assertEqual(
            [
                edge["target"]
                for edge in result["callers_and_consumers"]["relationships"]
            ],
            [
                "path:cancellation_one.rs",
                "path:cancellation_three.rs",
                "path:cancellation_two.rs",
            ],
        )
        self.assertEqual(
            result["invariants"]["relationships"][0]["target"], "contract:z"
        )
        self.assertEqual(
            result["tests_and_contracts"]["representative_scenario"]["target"],
            "path:z.rs",
        )
        self.assertEqual(result["omitted_relationships"], 3)

    def test_architecture_slice_ranks_focus_before_relationship_cap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            relationships = [
                {
                    "category": "control_flow",
                    "kind": "calls",
                    "target": f"path:src/noise_{index:03}.rs",
                    "confidence": "compiler_resolved",
                    "evidence": [{"path": f"src/noise_{index:03}.rs"}],
                }
                for index in range(source_owners.MAX_QUERY_RELATIONSHIPS + 1)
            ]
            relationships.append(
                {
                    "category": "control_flow",
                    "kind": "calls",
                    "target": "path:src/zz_critical_cache.rs",
                    "confidence": "compiler_resolved",
                    "evidence": [{"path": "src/zz_critical_cache.rs"}],
                }
            )
            manifest = {
                "owners": [
                    {
                        "id": "alpha",
                        "relationships": relationships,
                    }
                ]
            }

            slice_ = source_owners.architecture_slice(
                manifest,
                "digest",
                root,
                ["alpha"],
                max_relationships=1,
                focus="critical cache",
            )

            retained = slice_["control_and_data_flow"]["relationships"]
            self.assertEqual(retained[0]["target"], "path:src/zz_critical_cache.rs")
            self.assertEqual(slice_["omitted_relationships"], len(relationships) - 1)

    def test_query_deduplicates_relationships_before_bounding(self) -> None:
        relationship = {
            "category": "control_flow",
            "kind": "calls",
            "target": "path:src/duplicate.rs",
            "confidence": "compiler_resolved",
            "evidence": [{"path": "src/duplicate.rs"}],
        }
        unique_relationship = {
            "category": "control_flow",
            "kind": "calls",
            "target": "path:src/unique.rs",
            "confidence": "compiler_resolved",
            "evidence": [{"path": "src/unique.rs"}],
        }
        manifest = {
            "owners": [
                {
                    "id": "alpha",
                    "relationships": [relationship]
                    * (source_owners.MAX_QUERY_RELATIONSHIPS + 1)
                    + [unique_relationship],
                }
            ]
        }

        graph = source_owners.query_graph(
            manifest, "digest", REPO_ROOT, ["alpha"], max_relationships=2
        )

        self.assertEqual(graph["status"], "complete")
        self.assertEqual(graph["omitted"]["relationships"], 0)
        self.assertEqual(
            {item["target"] for item in graph["relationships"]},
            {"path:src/duplicate.rs", "path:src/unique.rs"},
        )

    def test_query_rejects_focus_instead_of_ignoring_it(self) -> None:
        argv = ["source_owners.py", "query", "--focus", "exact routing task"]

        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch("builtins.print") as print_output,
        ):
            self.assertEqual(source_owners.main(), 2)

        print_output.assert_called_once_with(
            "--focus is only valid with slice; select an owner with query, then run "
            "slice --owner <id> --focus <task>",
            file=sys.stderr,
        )

    def test_architecture_slice_counts_actual_manifest_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            custom_manifest = root / "custom.toml"
            custom_manifest.write_text("custom manifest\n", encoding="utf-8")
            default_manifest = root / "source_owners.toml"
            default_manifest.write_text(
                "default manifest with a different size\n", encoding="utf-8"
            )
            manifest = {"owners": [{"id": "alpha"}]}

            with (
                mock.patch.object(source_owners, "REPO_ROOT", root),
                mock.patch.object(source_owners, "DEFAULT_MANIFEST", default_manifest),
            ):
                slice_ = source_owners.architecture_slice(
                    manifest,
                    "digest",
                    root,
                    ["alpha"],
                    manifest_bytes_read=custom_manifest.stat().st_size,
                )

            self.assertEqual(
                slice_["metrics"]["bytes_read"], custom_manifest.stat().st_size
            )

    def test_architecture_slice_ranks_unicode_focus_terms(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = {
                "owners": [
                    {
                        "id": "alpha",
                        "relationships": [
                            {
                                "category": "control_flow",
                                "kind": "calls",
                                "target": "path:src/plain.rs",
                                "confidence": "compiler_resolved",
                                "evidence": [{"path": "src/plain.rs"}],
                            },
                            {
                                "category": "control_flow",
                                "kind": "calls",
                                "target": "path:src/東京.rs",
                                "confidence": "compiler_resolved",
                                "evidence": [{"path": "src/東京.rs"}],
                            },
                        ],
                    }
                ]
            }

            slice_ = source_owners.architecture_slice(
                manifest, "digest", root, ["alpha"], focus="東京"
            )

            ranked = slice_["control_and_data_flow"]["relationships"]
            self.assertEqual(ranked[0]["target"], "path:src/東京.rs")

    def test_architecture_index_is_source_keyed_and_deterministic(self) -> None:
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
        self.assertEqual(
            index["repository_revision"],
            source_owners.repository_revision(
                source_owners.REPO_ROOT, digest, manifest
            ),
        )
        self.assertIn(":sources:", index["repository_revision"])
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

    def test_exact_runtime_paths_route_to_behavioral_tests(self) -> None:
        cases = [
            (
                "codex-rs/core/src/session/turn.rs",
                "turn-orchestration",
                "test(session::turn::tests)",
                "mid_turn_compaction_failure_preserves_completed_message",
            ),
            (
                "codex-rs/utils/output-truncation/src/lib.rs",
                "output-truncation",
                "test(truncate_tests)",
                "formatted_token_policies_bound_dense_output_including_metadata",
            ),
        ]
        for path, owner, test_filter, scenario in cases:
            with self.subTest(path=path):
                with (
                    mock.patch.object(
                        sys,
                        "argv",
                        [
                            "source_owners.py",
                            "slice",
                            "--path",
                            path,
                            "--focus",
                            "compaction failure and output token budgets",
                        ],
                    ),
                    mock.patch("builtins.print") as emit,
                ):
                    self.assertEqual(source_owners.main(), 0)
                result = json.loads(emit.call_args.args[0])
                self.assertEqual(
                    {
                        entry["source"]
                        for entry in result["registration_and_entrypoints"][
                            "relationships"
                        ]
                        if entry["kind"] == "entrypoint"
                    },
                    {f"owner:{owner}"},
                )
                self.assertEqual(result["material_unknowns"], [])
                self.assertEqual(
                    result["control_and_data_flow"]["status"], "established"
                )
                tests = result["tests_and_contracts"]
                self.assertIn(scenario, tests["representative_scenario"]["evidence"])
                self.assertIn(test_filter, tests["focused_validation"][0]["argv"])

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
