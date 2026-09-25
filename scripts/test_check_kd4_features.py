from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

import tomllib

from scripts import check_kd4_features

GATE_COMMAND = [
    "python",
    "scripts/rust_test_runner.py",
    "run-gate",
    "proof",
    "--profile",
    "fast",
]


class CheckKd4FeaturesTest(unittest.TestCase):
    def test_malformed_field_types_produce_findings(self):
        for old, new, expected in [
            ('status = "enabled"', "status = []", "invalid-status"),
            (
                'capability_kind = "runtime"',
                "capability_kind = {}",
                "invalid-capability-kind",
            ),
            ("config_keys = []", "config_keys = [42]", "invalid-config-keys"),
        ]:
            with self.subTest(field=old):
                manifest = self.write_manifest(self.valid_evidence())
                manifest.write_text(manifest.read_text().replace(old, new))
                result = check_kd4_features.validate_manifest(
                    manifest, repo_root=self.repo_root
                )
                self.assertFalse(result.ok)
                self.assertIn(expected, {finding.code for finding in result.findings})

    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.repo_root = Path(self.tempdir.name)
        (self.repo_root / "owner" / "src").mkdir(parents=True)
        (self.repo_root / "src").mkdir()
        (self.repo_root / "src" / "feature.py").write_text(
            "def main():\n    return 'live'\n",
            encoding="utf-8",
        )
        (self.repo_root / "src" / "registry.py").write_text(
            "from src.feature import main\n\nCOMMANDS = {'feature': main}\n",
            encoding="utf-8",
        )
        # Runtime proof uses the production route: one exact test in a named gate.
        (self.repo_root / "owner" / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\n', encoding="utf-8"
        )
        (self.repo_root / "owner" / "src" / "lib.rs").write_text(
            "#[test]\nfn test_feature_is_live() { assert!(registered()); }\n",
            encoding="utf-8",
        )
        gates = self.repo_root / "codex-rs" / ".config" / "kd4-rust-tests.toml"
        gates.parent.mkdir(parents=True)
        gates.write_text(
            'version = 1\n[helpers]\n[targets.fixture_lib]\npackage = "fixture"\nlib = true\nhelpers = []\n'
            '[gates.proof]\n[[gates.proof.steps]]\ntarget = "fixture_lib"\ntests = ["test_feature_is_live"]\nhelpers = []\n',
            encoding="utf-8",
        )

    def write_manifest(
        self,
        feature_body: str,
        *,
        verification_path: str = "owner/src/lib.rs",
        command: list[str] | None = None,
    ) -> Path:
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
                runtime_verification = {{ kind = "contract_test", path = "{verification_path}", symbol = "test_feature_is_live", command = {json.dumps(command or GATE_COMMAND)} }}
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
            """
        )

    @contextlib.contextmanager
    def fake_cargo(self, stream: str, returncode: int = 0):
        """Serve metadata and one nextest run to the real gate runner."""
        calls: list[list[str]] = []

        def cargo(argv, **kwargs):
            calls.append(argv)
            if argv[:2] == ["cargo", "metadata"]:
                metadata = {
                    "target_directory": str(self.repo_root / "target"),
                    "packages": [
                        {
                            "name": "fixture",
                            "id": "fixture-id",
                            "targets": [{"name": "fixture", "kind": ["lib"]}],
                        }
                    ],
                }
                return subprocess.CompletedProcess(argv, 0, json.dumps(metadata), "")
            if argv[:3] == ["cargo", "nextest", "run"]:
                return subprocess.CompletedProcess(argv, returncode, stream, "")
            self.fail(f"unexpected cargo invocation: {argv}")

        runner = check_kd4_features.rust_test_runner
        runner_class, load_metadata = runner.RustTestRunner, runner.load_metadata
        with (
            mock.patch.object(subprocess, "run", side_effect=cargo),
            mock.patch.object(
                runner,
                "load_metadata",
                side_effect=lambda **kwargs: load_metadata(executor=cargo, **kwargs),
            ),
            mock.patch.object(
                runner,
                "RustTestRunner",
                side_effect=lambda *args, **kwargs: runner_class(
                    *args, **kwargs, executor=cargo
                ),
            ),
        ):
            yield calls

    def test_repository_manifest_passes(self) -> None:
        result = check_kd4_features.validate_manifest(
            check_kd4_features.DEFAULT_MANIFEST,
            repo_root=check_kd4_features.REPO_ROOT,
        )

        self.assertTrue(result.ok, result.findings)
        self.assertGreaterEqual(result.feature_count, 1)

    def test_inline_suite_shard_verification_requires_the_registered_binary(
        self,
    ) -> None:
        owner = self.repo_root / "owner"
        suite = owner / "tests/suite"
        suite.mkdir(parents=True)
        (suite / "behavior.rs").write_text(
            "#[test]\nfn expected_behavior() { assert_eq!(2 + 2, 4); }\n"
        )
        shard = owner / "tests/alpha.rs"
        shard.write_text(
            '#[path = "suite"]\nmod suite {\n'
            '    #[path = "behavior.rs"]\n    mod behavior;\n}\n'
        )
        (owner / "tests/beta.rs").write_text(
            '#[path = "suite"]\nmod suite {\n'
            '    #[path = "other.rs"]\n    mod other;\n}\n'
        )
        gates = self.repo_root / "codex-rs/.config/kd4-rust-tests.toml"
        gates.write_text(
            "version = 1\n[helpers]\n"
            '[targets.alpha]\npackage = "fixture"\ntest = "alpha"\nhelpers = []\n'
            '[targets.beta]\npackage = "fixture"\ntest = "beta"\nhelpers = []\n'
            '[gates.alpha]\n[[gates.alpha.steps]]\ntarget = "alpha"\n'
            'tests = ["suite::behavior::expected_behavior"]\nhelpers = []\n'
            '[gates.beta]\n[[gates.beta.steps]]\ntarget = "beta"\n'
            'tests = ["suite::behavior::expected_behavior"]\nhelpers = []\n'
        )
        verification = {
            "path": "owner/tests/suite/behavior.rs",
            "symbol": "expected_behavior",
            "command": [
                "python",
                "scripts/rust_test_runner.py",
                "run-gate",
                "alpha",
                "--profile",
                "fast",
            ],
        }
        self.assertEqual(
            check_kd4_features._verification_route(verification, self.repo_root),
            "nextest",
        )
        gates.write_text(
            gates.read_text() + '\n[[gates.alpha.steps]]\ntarget = "beta"\n'
            'tests = ["suite::other::independent_behavior"]\nhelpers = []\n'
        )
        self.assertEqual(
            check_kd4_features._verification_route(verification, self.repo_root),
            "nextest",
        )
        wrong_binary = {**verification, "command": [*verification["command"]]}
        wrong_binary["command"][3] = "beta"
        with self.assertRaisesRegex(ValueError, "source's package and test binary"):
            check_kd4_features._verification_route(wrong_binary, self.repo_root)
        shard.write_text("mod suite {}\n")
        with self.assertRaisesRegex(
            ValueError, "resolve to one integration test binary"
        ):
            check_kd4_features._verification_route(verification, self.repo_root)

    def test_rust_gate_rejects_same_named_test_in_another_module_of_same_binary(
        self,
    ) -> None:
        source = self.repo_root / "owner" / "src"
        (source / "lib.rs").write_text(
            'mod unrelated;\n#[path = "correct_owner.rs"] mod actual;\n'
        )
        body = "#[test]\nfn proves_feature() { assert_eq!(2 + 2, 4); }\n"
        (source / "correct_owner.rs").write_text(body)
        (source / "unrelated.rs").write_text(body)
        gate_path = self.repo_root / "codex-rs/.config/kd4-rust-tests.toml"
        gate_template = (
            'version = 1\n[helpers]\n[targets.fixture_lib]\npackage = "fixture"\nlib = true\nhelpers = []\n'
            '[gates.proof]\n[[gates.proof.steps]]\ntarget = "fixture_lib"\ntests = ["%s::proves_feature"]\nhelpers = []\n'
        )
        verification = {
            "path": "owner/src/correct_owner.rs",
            "symbol": "proves_feature",
            "command": GATE_COMMAND,
        }
        gate_path.write_text(gate_template % "actual")
        self.assertEqual(
            check_kd4_features._verification_route(verification, self.repo_root),
            "nextest",
        )
        gate_path.write_text(gate_template % "unrelated")
        with self.assertRaisesRegex(ValueError, "exact source-qualified test identity"):
            check_kd4_features._verification_route(verification, self.repo_root)
        # A filename-shaped selector is also wrong when the source is registered under an alias.
        gate_path.write_text(gate_template % "correct_owner")
        with self.assertRaisesRegex(ValueError, "exact source-qualified test identity"):
            check_kd4_features._verification_route(verification, self.repo_root)

    def test_rust_gate_binds_inline_module_and_ignores_same_named_literal_text(
        self,
    ) -> None:
        source = self.repo_root / "inline.rs"
        for name, decoy in (
            (
                "raw string",
                'const EXAMPLE: &str = r#"mod desired { #[test] fn proves_feature() {} }"#;\n',
            ),
            (
                "block comment",
                "/* mod desired { #[test] fn proves_feature() {} } */\n",
            ),
        ):
            with self.subTest(scenario=name):
                source.write_text(
                    decoy
                    + "mod unrelated { #[test] fn proves_feature() { assert_eq!(2 + 2, 4); } }\n"
                )
                self.assertIsNone(
                    check_kd4_features._rust_test_source(
                        source, "desired::proves_feature", self.repo_root
                    )
                )
                self.assertEqual(
                    check_kd4_features._rust_test_source(
                        source, "unrelated::proves_feature", self.repo_root
                    ),
                    source.resolve(),
                )
                self.assertIsNone(
                    check_kd4_features._rust_test_source(
                        source, "proves_feature", self.repo_root
                    )
                )

    def test_task_continuity_workflow_is_retired_end_to_end(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)

        self.assertNotIn(
            "task-continuity-hooks",
            {feature["id"] for feature in manifest["features"]},
        )
        # `.codex/hooks.json` is Codex's supported repo hooks file, so only the
        # continuity-specific scripts stay retired.
        for retired_path in (
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

    def test_unknown_feature_keys_cannot_silently_skip_checks(self) -> None:
        # A misspelled retired_paths would otherwise let a retired file return.
        (self.repo_root / "src" / "legacy_feature.py").write_text(
            "def main():\n    return 'stale'\n", encoding="utf-8"
        )
        for key in (
            'retired_path = ["src/legacy_feature.py"]',
            'source_owner = "feature-owner"',
        ):
            with self.subTest(key=key):
                result = check_kd4_features.validate_manifest(
                    self.write_manifest(key + "\n" + self.valid_evidence()),
                    repo_root=self.repo_root,
                )
                self.assertFalse(result.ok)
                self.assertIn(
                    "unknown-feature-key", {finding.code for finding in result.findings}
                )

    def test_repository_maps_remain_retired(self) -> None:
        with check_kd4_features.DEFAULT_MANIFEST.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
        self.assertNotIn(
            "repository-intelligence", {f["id"] for f in manifest["features"]}
        )
        feature = next(
            f for f in manifest["features"] if f["id"] == "kd4-feature-manifest"
        )
        retired = {
            "SOURCEMAP.md",
            "source_owners.toml",
            "architecture_index.json",
            "scripts/source_map_check.py",
            "scripts/test_source_map_check.py",
            "scripts/source_owners.py",
            "scripts/test_source_owners.py",
        }
        self.assertTrue(retired.issubset(feature["retired_paths"]))
        for path in retired:
            with self.subTest(path=path):
                self.assertFalse((check_kd4_features.REPO_ROOT / path).exists())
                (self.repo_root / path).parent.mkdir(parents=True, exist_ok=True)
                restored = self.repo_root / path
                restored.write_text("restored", encoding="utf-8")
                manifest_path = self.write_manifest(
                    "retired_paths = ["
                    + json.dumps(path)
                    + "]\n"
                    + self.valid_evidence()
                )
                result = check_kd4_features.validate_manifest(
                    manifest_path, repo_root=self.repo_root
                )
                self.assertFalse(result.ok)
                self.assertIn(
                    "parallel-implementation", {f.code for f in result.findings}
                )
                restored.unlink()

    def test_valid_enabled_feature_passes(self) -> None:
        # No test marker: an enabled runtime feature's gate is its test proof.
        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertTrue(result.ok, result.findings)
        self.assertEqual(result.status_counts, {"enabled": 1})
        self.assertEqual(result.runtime_status_counts, {})

    def test_enabled_workflow_requires_test_evidence(self) -> None:
        manifest = self.write_manifest(self.valid_evidence())
        manifest.write_text(
            manifest.read_text().replace(
                'capability_kind = "runtime"', 'capability_kind = "workflow"'
            )
        )

        result = check_kd4_features.validate_manifest(
            manifest, repo_root=self.repo_root
        )

        self.assertFalse(result.ok)
        self.assertIn("missing-test", {finding.code for finding in result.findings})

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
        (self.repo_root / "owner" / "src" / "lib.rs").write_text(
            "// fn test_feature_is_live() {}\n#[test]\nfn removed_test() {}\n",
            encoding="utf-8",
        )

        result = check_kd4_features.validate_manifest(
            self.write_manifest(self.valid_evidence()),
            repo_root=self.repo_root,
        )

        self.assertFalse(result.ok)
        self.assertIn(
            "invalid-runtime-verification",
            {finding.code for finding in result.findings},
        )

    def run_json_verification(self, manifest: Path, *extra: str) -> dict:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = check_kd4_features.main(
                [
                    "--manifest",
                    str(manifest),
                    "--repo-root",
                    str(self.repo_root),
                    "--json",
                    *extra,
                ]
            )
        payload = json.loads(output.getvalue())
        self.assertEqual(code == 0, payload["ok"])
        return payload

    def test_non_gate_verification_routes_are_rejected_before_launch(self) -> None:
        (self.repo_root / "tests").mkdir()
        (self.repo_root / "tests" / "test_feature.py").write_text(
            "import unittest\n\n\nclass FeatureTest(unittest.TestCase):\n"
            "    def test_feature_is_live(self):\n        self.assertTrue(True)\n",
            encoding="utf-8",
        )
        gate = GATE_COMMAND[:3]
        for path, command in (
            ("owner/src/lib.rs", ["cargo", "test", "test_feature_is_live"]),
            ("owner/src/lib.rs", [*gate, "proof"]),
            ("owner/src/lib.rs", [*gate, "missing", "--profile", "fast"]),
            # A Python test cannot stand in for a capability gate.
            (
                "tests/test_feature.py",
                [
                    "python",
                    "-m",
                    "unittest",
                    "tests.test_feature.FeatureTest.test_feature_is_live",
                ],
            ),
        ):
            with self.subTest(command=command):
                manifest = self.write_manifest(
                    self.valid_evidence(), verification_path=path, command=command
                )
                with (
                    mock.patch.object(subprocess, "run") as run,
                    mock.patch.object(subprocess, "Popen") as popen,
                ):
                    payload = self.run_json_verification(manifest)
                self.assertFalse(payload["ok"])
                self.assertIsNone(payload["runtimeVerificationExitCode"])
                run.assert_not_called()
                popen.assert_not_called()

    def test_static_only_does_not_execute_the_test(self) -> None:
        # Execution would invoke real cargo in a directory with no workspace.
        manifest = self.write_manifest(self.valid_evidence())
        process = subprocess.run(
            [
                sys.executable,
                str(Path(check_kd4_features.__file__).resolve()),
                "--manifest",
                str(manifest),
                "--repo-root",
                str(self.repo_root),
                "--json",
                "--static-only",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )
        self.assertEqual(process.returncode, 0, process.stderr)
        payload = json.loads(process.stdout)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["staticEvidence"], "present")
        self.assertEqual(payload["runtimeVerification"], "not_run")
        self.assertIsNone(payload["runtimeVerificationExitCode"])

    def test_rust_verification_batches_through_real_gate_runner(self) -> None:
        # Two capabilities share one proof; both must receive its actual result.
        manifest = self.write_manifest(self.valid_evidence())
        feature = manifest.read_text().split("[[features]]", 1)[1]
        manifest.write_text(
            manifest.read_text()
            + "\n[[features]]"
            + feature.replace('id = "feature"', 'id = "second"', 1)
        )
        for stream, returncode, expected in (
            ("PASS [ 0.001s] fixture test_feature_is_live", 0, "passed"),
            ("FAIL [ 0.001s] fixture test_feature_is_live", 100, "failed"),
            ("", 0, "not_executed"),
            ("PASS [0.001s] fixture unrelated", 0, "not_executed"),
            # Without an exact PASS for the declared test, a skip is not proof.
            ("SKIP [0.001s] fixture test_feature_is_live", 0, "not_executed"),
        ):
            passed = expected == "passed"
            with self.subTest(stream=stream):
                with self.fake_cargo(stream, returncode) as calls:
                    payload = self.run_json_verification(manifest)
                self.assertEqual(payload["ok"], passed, payload)
                self.assertEqual(payload["featureCount"], 2)
                self.assertEqual(
                    payload["runtimeVerificationExitCode"], 0 if passed else 2
                )
                results = payload["runtimeVerificationResults"]
                self.assertEqual(
                    [item["outcome"] for item in results], [expected, expected]
                )
                self.assertEqual(
                    [item["feature_id"] for item in results], ["feature", "second"]
                )
                self.assertEqual(
                    [item["test_identities"] for item in results],
                    [["test_feature_is_live"], ["test_feature_is_live"]]
                    if passed
                    else [[], []],
                )
                self.assertEqual(
                    sum(argv[:2] == ["cargo", "metadata"] for argv in calls), 1
                )
                self.assertEqual(
                    sum(argv[:3] == ["cargo", "nextest", "run"] for argv in calls), 1
                )

    def test_default_cli_exit_code_follows_the_executed_gate(self) -> None:
        args = [
            "--manifest",
            str(self.write_manifest(self.valid_evidence())),
            "--repo-root",
            str(self.repo_root),
        ]
        for stream, returncode, expected_exit in (
            ("PASS [ 0.001s] fixture test_feature_is_live", 0, 0),
            ("FAIL [ 0.001s] fixture test_feature_is_live", 100, 2),
        ):
            with self.subTest(stream=stream):
                output = io.StringIO()
                with (
                    self.fake_cargo(stream, returncode),
                    contextlib.redirect_stdout(output),
                ):
                    exit_code = check_kd4_features.main(args)
                self.assertEqual(exit_code, expected_exit, output.getvalue())
                self.assertEqual(
                    "KD4 TEST RESULT [feature]: passed" in output.getvalue(),
                    expected_exit == 0,
                )

    def test_busy_core_lane_is_reported_without_launching_anything(self) -> None:
        busy = "Cargo lane 'core-tests' is busy; no cold overflow was started."

        @contextlib.contextmanager
        def busy_lane(**_kwargs):
            raise RuntimeError(busy)
            yield

        (self.repo_root / "codex-rs" / "Cargo.toml").write_text("[workspace]\n")
        manifest = self.write_manifest(self.valid_evidence())
        # A command already running inside a lane would bypass the reservation.
        env = {
            name: value
            for name, value in os.environ.items()
            if name not in {"CODEX_CARGO_LANE_TARGET_DIR", "CODEX_CARGO_LANE_OWNER_PID"}
        }
        with (
            mock.patch.object(
                check_kd4_features.rust_build_status, "reserve_cargo_lane", busy_lane
            ),
            mock.patch.dict(os.environ, env, clear=True),
            mock.patch.object(subprocess, "run") as run,
            mock.patch.object(subprocess, "Popen") as popen,
            contextlib.redirect_stderr(io.StringIO()) as stderr,
        ):
            payload = self.run_json_verification(manifest)
            defaults = check_kd4_features._load_feature_defaults(self.repo_root)

        self.assertFalse(payload["ok"])
        self.assertEqual(payload["runtimeVerificationExitCode"], 2)
        [result] = payload["runtimeVerificationResults"]
        self.assertEqual(result["outcome"], "not_executed")
        self.assertIn(busy, result["error"])
        self.assertIsNone(defaults)
        self.assertIn(busy, stderr.getvalue())
        run.assert_not_called()
        popen.assert_not_called()

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

    @mock.patch.object(check_kd4_features, "run_finite")
    def test_feature_default_comes_from_machine_readable_rust_export(
        self, run: mock.Mock
    ) -> None:
        (self.repo_root / "codex-rs" / "Cargo.toml").write_text(
            "[workspace]\n", encoding="utf-8"
        )
        run.return_value = subprocess_completed = mock.Mock(
            returncode=0,
            output_truncated=False,
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
        self.assertNotIn("--locked", run.call_args.args[0])
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

    def test_enabled_feature_requires_entrypoint_and_registration(self) -> None:
        for kind in ("entrypoint", "registration"):
            with self.subTest(kind=kind):
                evidence = self.valid_evidence().replace(
                    f'kind = "{kind}"', 'kind = "workflow"'
                )
                result = check_kd4_features.validate_manifest(
                    self.write_manifest(evidence),
                    repo_root=self.repo_root,
                )

                self.assertFalse(result.ok)
                self.assertIn(
                    f"missing-{kind}", {finding.code for finding in result.findings}
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

    def test_evidence_without_non_empty_contains_is_rejected(self) -> None:
        # Kinds are counted before matching, so an unmatched marker must fail.
        for replacement in ('regex = "def main"', 'contains = ""'):
            with self.subTest(replacement=replacement):
                evidence = self.valid_evidence().replace(
                    'contains = "def main()"', replacement
                )
                result = check_kd4_features.validate_manifest(
                    self.write_manifest(evidence), repo_root=self.repo_root
                )
                self.assertFalse(result.ok)
                self.assertIn(
                    "invalid-evidence-match",
                    {finding.code for finding in result.findings},
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
