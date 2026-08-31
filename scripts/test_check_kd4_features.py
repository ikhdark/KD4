from __future__ import annotations

import contextlib
import io
import json
import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path
from unittest import mock

from scripts import check_kd4_features


class CheckKd4FeaturesTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.repo_root = Path(self.tempdir.name)
        (self.repo_root / "owner").mkdir()
        (self.repo_root / "src").mkdir()
        (self.repo_root / "tests").mkdir()
        (self.repo_root / "src" / "__init__.py").write_text("", encoding="utf-8")
        (self.repo_root / "tests" / "__init__.py").write_text("", encoding="utf-8")
        (self.repo_root / "src" / "feature.py").write_text(
            "def main():\n    return 'live'\n",
            encoding="utf-8",
        )
        (self.repo_root / "src" / "registry.py").write_text(
            "from src.feature import main\n\nCOMMANDS = {'feature': main}\n",
            encoding="utf-8",
        )
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                """
                import unittest

                from src.registry import COMMANDS


                class FeatureRegistrationTest(unittest.TestCase):
                    def test_feature_is_live(self):
                        self.assertEqual(COMMANDS["feature"](), "live")
                """
            ),
            encoding="utf-8",
        )

    def write_manifest(self, feature_body: str) -> Path:
        path = self.repo_root / "kd4_features.toml"
        path.write_text(
            textwrap.dedent(
                f"""
                schema_version = 2
                upstream_commit = "1111111111111111111111111111111111111111"
                status_semantics = "implementation_lifecycle"

                [[features]]
                id = "feature"
                version = 1
                status = "enabled"
                capability_kind = "runtime"
                owner = "owner"
                summary = "fixture"
                upstream_equivalent = "none"
                config_keys = []
                runtime_verification = {{ kind = "contract_test", path = "tests/test_feature.py", symbol = "test_feature_is_live", command = ["python", "-m", "unittest", "tests.test_feature.FeatureRegistrationTest.test_feature_is_live"] }}
                {feature_body}
                """
            ),
            encoding="utf-8",
        )
        return path

    def write_source_owner(self) -> None:
        (self.repo_root / "source_owners.toml").write_text(
            textwrap.dedent(
                """
                schema_version = 2

                [[owners]]
                id = "feature-owner"
                feature_ids = ["feature"]
                primary_entries = [{ path = "src/feature.py", symbol = "main" }]
                tests = ["tests/test_feature.py"]

                [[owners.relationships]]
                category = "runtime_registration"
                kind = "registers"
                target = "path:src/registry.py"
                evidence = [{ path = "src/registry.py", symbol = "COMMANDS" }]
                """
            ),
            encoding="utf-8",
        )

    @staticmethod
    def valid_evidence() -> str:
        return textwrap.dedent(
            """
            [[features.evidence]]
            kind = "entrypoint"
            path = "src/feature.py"
            contains = "def main()"

            [[features.evidence]]
            kind = "registration"
            path = "src/registry.py"
            contains = "'feature': main"

            [[features.evidence]]
            kind = "test"
            path = "tests/test_feature.py"
            contains = "test_feature_is_live"
            """
        )

    def test_repository_manifest_passes_non_strict(self) -> None:
        result = check_kd4_features.validate_manifest(
            check_kd4_features.DEFAULT_MANIFEST,
            repo_root=check_kd4_features.REPO_ROOT,
        )

        self.assertTrue(result.ok, result.findings)
        self.assertGreaterEqual(result.feature_count, 1)

    def test_repository_core_verifications_use_named_rust_target(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)

        core_verifications = [
            feature["runtime_verification"]
            for feature in manifest["features"]
            if feature.get("runtime_verification", {})
            .get("path", "")
            .startswith("codex-rs/core/")
        ]
        self.assertTrue(core_verifications)
        for verification in core_verifications:
            command = verification["command"]
            self.assertEqual(command[:3], ["just", "core-test-fast", "core_lib"])
            self.assertEqual(command[3], "-E")
            self.assertIn(verification["symbol"], command[4])
            self.assertNotIn("codex-core", command)

    def test_desktop_runtime_receipt_feature_is_absent(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)

        feature_ids = {feature["id"] for feature in manifest["features"]}
        self.assertNotIn("desktop-runtime-receipt", feature_ids)

    def test_repository_intelligence_uses_live_source_owner_workflow(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
        with (check_kd4_features.REPO_ROOT / "source_owners.toml").open(
            "rb"
        ) as source_owner_file:
            source_owners = tomllib.load(source_owner_file)

        feature = next(
            feature
            for feature in manifest["features"]
            if feature["id"] == "repository-intelligence"
        )
        source_owner = next(
            owner
            for owner in source_owners["owners"]
            if owner["id"] == "source-owner-index"
        )

        self.assertEqual(feature["version"], 2)
        self.assertEqual(feature["status"], "enabled")
        self.assertEqual(feature["capability_kind"], "workflow")
        self.assertEqual(feature["owner"], "scripts")
        self.assertEqual(feature["source_owner"], "source-owner-index")
        self.assertEqual(
            feature["generated_artifacts"],
            ["SOURCEMAP.md", "architecture_index.json"],
        )
        self.assertIn("repository-intelligence", source_owner["feature_ids"])

    def test_performance_sensitive_completion_requires_comparable_evidence(
        self,
    ) -> None:
        instructions = (check_kd4_features.REPO_ROOT / "AGENTS.md").read_text(
            encoding="utf-8"
        )
        benchmarking = instructions.split("## Benchmarking\n", maxsplit=1)[1]

        self.assertIn("explicit optimization or documented hot path", benchmarking)
        self.assertIn("Hold them constant for baseline and candidate", benchmarking)
        self.assertIn("latency statistic and threshold", benchmarking)
        self.assertIn("Finish only when the quality gate passes", benchmarking)

    def test_task_continuity_workflow_is_retired_end_to_end(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
        with (check_kd4_features.REPO_ROOT / "source_owners.toml").open(
            "rb"
        ) as owner_file:
            owners = tomllib.load(owner_file)

        self.assertNotIn(
            "task-continuity-hooks",
            {feature["id"] for feature in manifest["features"]},
        )
        self.assertNotIn(
            "task-continuity-hooks",
            {owner["id"] for owner in owners["owners"]},
        )
        for retired_path in (
            ".codex/hooks.json",
            ".codex/hooks/task-continuity-entry.ps1",
            ".codex/hooks/task-continuity-fast-basic.ps1",
            ".codex/hooks/task-continuity-fast-compact.ps1",
            ".codex/hooks/task-continuity-fast-session.ps1",
            ".codex/hooks/task-continuity.ps1",
            "codex-rs/core/src/continuity.rs",
            "scripts/test_task_continuity_hook.py",
        ):
            with self.subTest(path=retired_path):
                self.assertFalse((check_kd4_features.REPO_ROOT / retired_path).exists())

        for consumer_path in (
            "codex-rs/core/src/lib.rs",
            "codex-rs/core/src/hook_runtime.rs",
            "codex-rs/core/src/context_manager/history.rs",
        ):
            source = (check_kd4_features.REPO_ROOT / consumer_path).read_text(
                encoding="utf-8"
            )
            with self.subTest(consumer=consumer_path):
                self.assertNotIn("crate::continuity", source)
                self.assertNotIn("mod continuity;", source)

    def test_valid_enabled_feature_passes(self) -> None:
        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertTrue(result.ok, result.findings)
        self.assertEqual(result.status_counts, {"enabled": 1})
        self.assertEqual(result.runtime_status_counts, {})

    def test_enabled_runtime_requires_executable_verification(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            "\n".join(
                line
                for line in manifest.read_text(encoding="utf-8").splitlines()
                if not line.strip().startswith("runtime_verification =")
            )
            + "\n",
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest,
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "missing-runtime-verification",
            {finding.code for finding in result.findings},
        )

    def test_runtime_verification_symbol_must_remain_live(self) -> None:
        (self.repo_root / "tests" / "test_feature.py").write_text(
            "# def test_feature_is_live():\ndef removed_test():\n    pass\n",
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "stale-runtime-verification",
            {finding.code for finding in result.findings},
        )

    @mock.patch.object(check_kd4_features.subprocess, "run")
    def test_selected_runtime_verification_executes_declared_command_only(
        self, run: mock.Mock
    ) -> None:
        run.return_value = mock.Mock(returncode=0)
        manifest = self.write_manifest(self.valid_evidence())

        with contextlib.redirect_stdout(io.StringIO()):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(manifest),
                    "--repo-root",
                    str(self.repo_root),
                    "--run-runtime-verification",
                    "feature",
                ]
            )

        self.assertEqual(exit_code, 0)
        run.assert_called_once_with(
            [
                "python",
                "-m",
                "unittest",
                "tests.test_feature.FeatureRegistrationTest.test_feature_is_live",
            ],
            cwd=self.repo_root,
            check=False,
        )

    def test_default_cli_executes_registration_contract(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(manifest),
                    "--repo-root",
                    str(self.repo_root),
                ]
            )

        self.assertEqual(exit_code, 0, output.getvalue())
        self.assertIn("KD4 RUNTIME VERIFICATION [feature]", output.getvalue())

    def test_default_cli_rejects_unimported_registration(self) -> None:
        (self.repo_root / "src" / "registry.py").write_text(
            "COMMANDS = {'feature': main}\n",
            encoding="utf-8",
        )

        with contextlib.redirect_stdout(io.StringIO()):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(self.write_manifest(self.valid_evidence())),
                    "--repo-root",
                    str(self.repo_root),
                ]
            )

        self.assertNotEqual(exit_code, 0)

    def test_default_cli_rejects_dead_registration(self) -> None:
        (self.repo_root / "src" / "registry.py").write_text(
            "from src.feature import main\n\nCOMMANDS = {}\n",
            encoding="utf-8",
        )

        with contextlib.redirect_stdout(io.StringIO()):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(self.write_manifest(self.valid_evidence())),
                    "--repo-root",
                    str(self.repo_root),
                ]
            )

        self.assertNotEqual(exit_code, 0)

    def test_pass_only_runtime_verification_is_rejected_before_execution(self) -> None:
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                """
                import unittest


                class FeatureRegistrationTest(unittest.TestCase):
                    def test_feature_is_live(self):
                        pass
                """
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "vacuous-runtime-verification",
            {finding.code for finding in result.findings},
        )

    def test_planned_feature_cannot_retain_live_route_evidence(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'status = "enabled"', 'status = "planned"'
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest,
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "planned-feature-has-production-route",
            {finding.code for finding in result.findings},
        )

    def test_contract_schema_version_is_read_from_runtime_constant(self) -> None:
        (self.repo_root / "src" / "schema.rs").write_text(
            "pub const CONTRACT_SCHEMA_VERSION: u64 = 13;\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest(
            textwrap.dedent(
                """
                contract_schema_version = 12
                contract_schema_source = "src/schema.rs"
                contract_schema_symbol = "CONTRACT_SCHEMA_VERSION"
                """
            )
            + self.valid_evidence()
        )

        result = check_kd4_features.validate_manifest(
            manifest,
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "contract-schema-drift",
            {finding.code for finding in result.findings},
        )

    def test_source_owner_supplies_reachability_without_inline_markers(self) -> None:
        self.write_source_owner()
        result = check_kd4_features.validate_manifest(
            self.write_manifest('source_owner = "feature-owner"'),
            repo_root=self.repo_root,
        )

        self.assertTrue(result.ok, result.findings)

    def test_shared_source_owner_liveness_is_observed_once(self) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'feature_ids = ["feature"]',
                'feature_ids = ["feature", "feature-two"]',
            ),
            encoding="utf-8",
        )
        findings: list[check_kd4_features.Finding] = []
        observations: dict[
            str, tuple[frozenset[str], check_kd4_features.Counter[str]]
        ] = {}
        owner_cache = None
        text_cache: dict[Path, str] = {}

        with mock.patch.object(
            check_kd4_features,
            "_safe_repo_path",
            wraps=check_kd4_features._safe_repo_path,
        ) as safe_repo_path:
            _, owner_cache = check_kd4_features._source_owner_evidence(
                source_owner_id="feature-owner",
                repo_root=self.repo_root.resolve(),
                feature_id="feature",
                findings=findings,
                owner_cache=owner_cache,
                owner_observation_cache=observations,
                text_cache=text_cache,
            )
            check_kd4_features._source_owner_evidence(
                source_owner_id="feature-owner",
                repo_root=self.repo_root.resolve(),
                feature_id="feature-two",
                findings=findings,
                owner_cache=owner_cache,
                owner_observation_cache=observations,
                text_cache=text_cache,
            )

        self.assertEqual(findings, [])
        self.assertEqual(safe_repo_path.call_count, 3)

    def test_source_owner_must_explicitly_own_feature(self) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'feature_ids = ["feature"]', 'feature_ids = ["different-feature"]'
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest('source_owner = "feature-owner"'),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "source-owner-feature-mismatch",
            {finding.code for finding in result.findings},
        )

    def test_source_owner_markers_must_resolve_to_live_symbols(self) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'symbol = "main"', 'symbol = "removed_entrypoint"'
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest('source_owner = "feature-owner"'),
            repo_root=self.repo_root,
        )

        codes = {finding.code for finding in result.findings}
        self.assertIn("stale-source-owner-evidence", codes)
        self.assertIn("missing-entrypoint", codes)

    def test_source_owner_symbol_in_comment_is_not_live_evidence(self) -> None:
        self.write_source_owner()
        (self.repo_root / "src" / "feature.py").write_text(
            "# def main():\ndef active_entrypoint():\n    return 'live'\n",
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest('source_owner = "feature-owner"'),
            repo_root=self.repo_root,
        )

        codes = {finding.code for finding in result.findings}
        self.assertIn("stale-source-owner-evidence", codes)
        self.assertIn("missing-entrypoint", codes)

    def test_source_owner_registration_must_have_live_evidence(self) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'symbol = "COMMANDS"', 'symbol = "REMOVED_REGISTRY"'
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest('source_owner = "feature-owner"'),
            repo_root=self.repo_root,
        )

        codes = {finding.code for finding in result.findings}
        self.assertIn("stale-source-owner-evidence", codes)
        self.assertIn("missing-registration", codes)

    def test_source_owner_cannot_duplicate_inline_evidence(self) -> None:
        self.write_source_owner()
        result = check_kd4_features.validate_manifest(
            self.write_manifest(
                'source_owner = "feature-owner"\n' + self.valid_evidence()
            ),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "duplicate-evidence-authority",
            {finding.code for finding in result.findings},
        )

    def test_feature_config_requires_runtime_status(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "config_keys = []",
                'config_keys = ["features.example"]',
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-runtime-status", {finding.code for finding in result.findings}
        )

    @mock.patch.object(check_kd4_features.subprocess, "run")
    def test_feature_default_comes_from_machine_readable_rust_export(
        self, run: mock.Mock
    ) -> None:
        (self.repo_root / "codex-rs").mkdir()
        (self.repo_root / "codex-rs" / "Cargo.toml").write_text(
            "[workspace]\n", encoding="utf-8"
        )
        run.return_value = subprocess_completed = mock.Mock(
            returncode=0,
            stdout='[{"key":"platform_feature","defaultEnabled":true}]',
        )

        cache: dict[str, dict[str, bool] | None] = {}
        self.assertEqual(
            check_kd4_features._feature_default(
                self.repo_root, "features.platform_feature", cache
            ),
            True,
        )
        self.assertEqual(
            check_kd4_features._feature_default(
                self.repo_root, "features.missing", cache
            ),
            None,
        )
        self.assertEqual(run.call_count, 1)
        self.assertEqual(subprocess_completed.stdout.count("defaultEnabled"), 1)

    def test_project_runtime_status_must_match_effective_config(self) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features]\nexample = false\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "config_keys = []",
                'config_keys = ["features.example"]\n'
                'runtime_feature_key = "features.example"\n'
                'runtime_status = "enabled"\n'
                'runtime_status_source = ".codex/config.toml"',
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "stale-runtime-status", {finding.code for finding in result.findings}
        )

    def test_project_config_is_parsed_once_for_multiple_feature_lookups(self) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features]\nfirst = true\nsecond = false\n",
            encoding="utf-8",
        )
        cache: dict[str, object] = {}

        with mock.patch.object(
            check_kd4_features.tomllib,
            "load",
            wraps=check_kd4_features.tomllib.load,
        ) as load:
            first = check_kd4_features._project_feature_override(
                self.repo_root, "features.first", cache
            )
            second = check_kd4_features._project_feature_override(
                self.repo_root, "features.second", cache
            )

        self.assertIs(first, True)
        self.assertIs(second, False)
        self.assertEqual(load.call_count, 1)

    def test_safe_repo_path_does_not_reresolve_resolved_root(self) -> None:
        resolved_root = self.repo_root.resolve()
        original_resolve = Path.resolve

        with mock.patch.object(
            Path,
            "resolve",
            autospec=True,
            side_effect=lambda path, *args, **kwargs: original_resolve(
                path, *args, **kwargs
            ),
        ) as resolve:
            candidate, error = check_kd4_features._safe_repo_path(
                resolved_root, "src/feature.py"
            )

        self.assertIsNone(error)
        self.assertEqual(candidate, (resolved_root / "src/feature.py").resolve())
        self.assertEqual(resolve.call_count, 1)

    def test_malformed_project_config_is_reported_instead_of_using_defaults(
        self,
    ) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features\nexample = true\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "config_keys = []",
                'config_keys = ["features.example"]\n'
                'runtime_feature_key = "features.example"\n'
                'runtime_status = "enabled"\n'
                'runtime_status_source = ".codex/config.toml"',
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-project-config", {finding.code for finding in result.findings}
        )

    def test_enabled_feature_without_registration_fails(self) -> None:
        evidence = self.valid_evidence().replace(
            'kind = "registration"',
            'kind = "workflow"',
        )
        result = check_kd4_features.validate_manifest(
            self.write_manifest(evidence),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "missing-registration", {finding.code for finding in result.findings}
        )

    def test_stale_marker_fails(self) -> None:
        evidence = self.valid_evidence().replace("def main()", "def missing()")
        result = check_kd4_features.validate_manifest(
            self.write_manifest(evidence),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn("stale-evidence", {finding.code for finding in result.findings})

    def test_missing_generated_artifact_fails(self) -> None:
        result = check_kd4_features.validate_manifest(
            self.write_manifest(
                'generated_artifacts = ["generated/feature.json"]\n'
                + self.valid_evidence()
            ),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "missing-generated-artifact",
            {finding.code for finding in result.findings},
        )

    def test_retired_parallel_implementation_fails_if_it_reappears(self) -> None:
        (self.repo_root / "src" / "legacy_feature.py").write_text(
            "def main():\n    return 'stale'\n",
            encoding="utf-8",
        )
        result = check_kd4_features.validate_manifest(
            self.write_manifest(
                'retired_paths = ["src/legacy_feature.py"]\n' + self.valid_evidence()
            ),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "parallel-implementation", {finding.code for finding in result.findings}
        )

    def test_parent_path_escape_fails(self) -> None:
        evidence = self.valid_evidence().replace(
            'path = "src/feature.py"',
            'path = "../outside.py"',
        )
        result = check_kd4_features.validate_manifest(
            self.write_manifest(evidence),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-evidence-path", {finding.code for finding in result.findings}
        )

    def test_unhashable_feature_id_reports_finding(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'id = "feature"', 'id = ["feature"]'
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn("missing-field", {finding.code for finding in result.findings})

    def test_empty_regex_is_not_silently_ignored(self) -> None:
        evidence = self.valid_evidence().replace(
            'contains = "def main()"',
            'contains = "def main()"\nregex = ""',
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest(evidence), repo_root=self.repo_root
        )

        matches = [
            finding
            for finding in result.findings
            if finding.code == "invalid-evidence-match"
        ]
        self.assertTrue(matches)
        self.assertIn("exactly one", matches[0].message)

    def test_empty_regex_alone_reports_non_empty_requirement(self) -> None:
        evidence = self.valid_evidence().replace(
            'contains = "def main()"',
            'regex = ""',
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest(evidence), repo_root=self.repo_root
        )

        self.assertTrue(
            any(
                finding.code == "invalid-evidence-match"
                and "non-empty string" in finding.message
                for finding in result.findings
            )
        )

    def test_missing_owner_has_one_root_cause_finding(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'owner = "owner"\n',
                "",
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        owner_findings = [
            finding
            for finding in result.findings
            if "owner" in finding.message or finding.code == "invalid-owner"
        ]
        self.assertEqual(
            [finding.code for finding in owner_findings], ["missing-field"]
        )

    def test_evidence_text_is_cached_across_features(self) -> None:
        manifest = self.repo_root / "kd4_features.toml"
        manifest.write_text(
            textwrap.dedent(
                """
                schema_version = 2
                upstream_commit = "0123456789abcdef0123456789abcdef01234567"
                status_semantics = "implementation_lifecycle"

                [[features]]
                id = "one"
                version = 1
                status = "disabled"
                capability_kind = "library"
                owner = "owner"
                summary = "one"
                upstream_equivalent = "none"
                config_keys = []
                [[features.evidence]]
                kind = "module"
                path = "src/feature.py"
                contains = "def main()"

                [[features]]
                id = "two"
                version = 1
                status = "disabled"
                capability_kind = "library"
                owner = "owner"
                summary = "two"
                upstream_equivalent = "none"
                config_keys = []
                [[features.evidence]]
                kind = "module"
                path = "src/feature.py"
                contains = "return 'live'"
                """
            ),
            encoding="utf-8",
        )
        original_read_text = Path.read_text
        evidence_reads = 0

        def count_reads(path: Path, *args: object, **kwargs: object) -> str:
            nonlocal evidence_reads
            if path == self.repo_root / "src" / "feature.py":
                evidence_reads += 1
            return original_read_text(path, *args, **kwargs)

        with mock.patch.object(
            Path, "read_text", autospec=True, side_effect=count_reads
        ):
            result = check_kd4_features.validate_manifest(
                manifest, repo_root=self.repo_root
            )

        self.assertTrue(result.ok, result.findings)
        self.assertEqual(evidence_reads, 1)

    def test_strict_mode_promotes_orphan_to_error(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        text = manifest.read_text(encoding="utf-8").replace(
            'status = "enabled"',
            'status = "orphaned"',
        )
        manifest.write_text(text, encoding="utf-8")

        non_strict = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root, strict=False
        )
        strict = check_kd4_features.validate_manifest(
            manifest,
            repo_root=self.repo_root,
            strict=True,
        )

        self.assertTrue(non_strict.ok)
        self.assertFalse(strict.ok)
        self.assertEqual(
            [
                finding.level
                for finding in strict.findings
                if finding.code == "orphaned-feature"
            ],
            ["error"],
        )

    def test_cli_is_strict_by_default_with_explicit_opt_out(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'status = "enabled"',
                'status = "orphaned"',
            ),
            encoding="utf-8",
        )
        common_args = [
            "--manifest",
            str(manifest),
            "--repo-root",
            str(self.repo_root),
            "--json",
        ]

        with contextlib.redirect_stdout(io.StringIO()):
            strict_exit = check_kd4_features.main(common_args)
            non_strict_exit = check_kd4_features.main([*common_args, "--no-strict"])

        self.assertEqual(strict_exit, 1)
        self.assertEqual(non_strict_exit, 0)

    def test_json_cli_reports_machine_readable_verdict(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(manifest),
                    "--repo-root",
                    str(self.repo_root),
                    "--json",
                ]
            )

        self.assertEqual(exit_code, 0)
        payload = json.loads(output.getvalue())
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["featureCount"], 1)
        self.assertEqual(payload["runtimeStatusCounts"], {})
        self.assertEqual(payload["runtimeVerificationExitCode"], 0)

    @mock.patch.object(check_kd4_features.subprocess, "run")
    def test_json_cli_reports_runtime_verification_failure(
        self, run: mock.Mock
    ) -> None:
        run.return_value = mock.Mock(returncode=7)
        manifest = self.write_manifest(self.valid_evidence())
        output = io.StringIO()

        with contextlib.redirect_stdout(output):
            exit_code = check_kd4_features.main(
                [
                    "--manifest",
                    str(manifest),
                    "--repo-root",
                    str(self.repo_root),
                    "--json",
                ]
            )

        self.assertEqual(exit_code, 7)
        payload = json.loads(output.getvalue())
        self.assertFalse(payload["ok"])
        self.assertEqual(payload["runtimeVerificationExitCode"], 7)
        run.assert_called_once_with(
            [
                "python",
                "-m",
                "unittest",
                "tests.test_feature.FeatureRegistrationTest.test_feature_is_live",
            ],
            cwd=self.repo_root,
            check=False,
            stdout=check_kd4_features.subprocess.DEVNULL,
            stderr=check_kd4_features.subprocess.DEVNULL,
        )

    def test_missing_upstream_commit_is_rejected(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'upstream_commit = "1111111111111111111111111111111111111111"\n', ""
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-upstream-commit", {finding.code for finding in result.findings}
        )

    def test_malformed_upstream_commit_is_rejected(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "1111111111111111111111111111111111111111", "NOT-A-COMMIT"
            ),
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-upstream-commit", {finding.code for finding in result.findings}
        )


if __name__ == "__main__":
    unittest.main()
