from __future__ import annotations

import contextlib
import io
import json
import tempfile
import textwrap
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
        (self.repo_root / "src" / "feature.py").write_text(
            "def main():\n    return 'live'\n",
            encoding="utf-8",
        )
        (self.repo_root / "src" / "registry.py").write_text(
            "COMMANDS = {'feature': main}\n",
            encoding="utf-8",
        )
        (self.repo_root / "tests" / "test_feature.py").write_text(
            "def test_feature_is_live():\n    pass\n",
            encoding="utf-8",
        )

    def write_manifest(self, feature_body: str) -> Path:
        path = self.repo_root / "kd4_features.toml"
        path.write_text(
            textwrap.dedent(
                f"""
                schema_version = 1

                [[features]]
                id = "feature"
                version = 1
                status = "enabled"
                capability_kind = "runtime"
                owner = "owner"
                summary = "fixture"
                upstream_equivalent = "none"
                config_keys = []
                {feature_body}
                """
            ),
            encoding="utf-8",
        )
        return path

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

    def test_valid_enabled_feature_passes(self) -> None:
        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertTrue(result.ok, result.findings)
        self.assertEqual(result.status_counts, {"enabled": 1})

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
                schema_version = 1

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

        normal = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )
        strict = check_kd4_features.validate_manifest(
            manifest,
            repo_root=self.repo_root,
            strict=True,
        )

        self.assertTrue(normal.ok)
        self.assertFalse(strict.ok)
        self.assertEqual(
            [
                finding.level
                for finding in strict.findings
                if finding.code == "orphaned-feature"
            ],
            ["error"],
        )

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


if __name__ == "__main__":
    unittest.main()
