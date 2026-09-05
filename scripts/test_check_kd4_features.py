from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


class CheckKd4FeaturesTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo_checkout = Path(__file__).resolve().parents[1]
        just = shutil.which("just")
        if just is None:
            raise unittest.SkipTest("just is required for the supported CLI boundary")
        cls.just = Path(just).resolve()
        cls.native_tempdir = tempfile.TemporaryDirectory()
        native_root = Path(cls.native_tempdir.name)
        source = native_root / "probe.rs"
        source.write_text(
            textwrap.dedent(
                r"""
                use std::env;
                use std::fs::OpenOptions;
                use std::io::Write;
                use std::process;

                fn main() {
                    if let Ok(path) = env::var("KD4_FAKE_LOG") {
                        let mut file = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .expect("open fake runner log");
                        let arguments = env::args().skip(1).collect::<Vec<_>>().join("\t");
                        writeln!(file, "{arguments}").expect("write fake runner log");
                    }
                    if let Ok(output) = env::var("KD4_FAKE_STDOUT") {
                        print!("{output}");
                    }
                    let exit_code = env::var("KD4_FAKE_EXIT")
                        .ok()
                        .and_then(|value| value.parse::<i32>().ok())
                        .unwrap_or(0);
                    process::exit(exit_code);
                }
                """
            ),
            encoding="utf-8",
        )
        cls.native_probe = native_root / "probe.exe"
        subprocess.run(
            ["rustc", "--edition=2021", str(source), "-o", str(cls.native_probe)],
            check=True,
            capture_output=True,
            text=True,
        )

    @classmethod
    def tearDownClass(cls) -> None:
        cls.native_tempdir.cleanup()

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
        self.write_runtime_test()

    @staticmethod
    def toml_array(arguments: list[str]) -> str:
        return "[" + ", ".join(json.dumps(argument) for argument in arguments) + "]"

    def write_runtime_test(
        self,
        symbol: str = "test_feature_is_live",
        body: str = "self.assertEqual(COMMANDS['feature'](), 'live')",
    ) -> None:
        imports = "from src.registry import COMMANDS\n" if "COMMANDS" in body else ""
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                f"""
                import unittest
                {imports}

                class FeatureRegistrationTest(unittest.TestCase):
                    def {symbol}(self):
                        {body}
                """
            ),
            encoding="utf-8",
        )

    def valid_evidence(self, test_symbol: str = "test_feature_is_live") -> str:
        return textwrap.dedent(
            f"""
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
            contains = "{test_symbol}"
            """
        )

    def write_manifest(
        self,
        feature_body: str | None = None,
        *,
        feature_id: str = "feature",
        status: str = "enabled",
        capability_kind: str = "runtime",
        runtime_symbol: str = "test_feature_is_live",
        runtime_command: list[str] | None = None,
        include_runtime_verification: bool = True,
    ) -> Path:
        if feature_body is None:
            feature_body = self.valid_evidence(runtime_symbol)
        if runtime_command is None:
            runtime_command = [
                sys.executable,
                "-m",
                "unittest",
                f"tests.test_feature.FeatureRegistrationTest.{runtime_symbol}",
            ]
        verification = ""
        if include_runtime_verification and capability_kind == "runtime":
            verification = (
                'runtime_verification = { kind = "contract_test", '
                f'path = "tests/test_feature.py", symbol = "{runtime_symbol}", '
                f"command = {self.toml_array(runtime_command)} }}\n"
            )
        path = self.repo_root / "kd4_features.toml"
        path.write_text(
            textwrap.dedent(
                f"""
                schema_version = 2
                upstream_commit = "1111111111111111111111111111111111111111"
                status_semantics = "implementation_lifecycle"

                [[features]]
                id = "{feature_id}"
                version = 1
                status = "{status}"
                capability_kind = "{capability_kind}"
                owner = "owner"
                summary = "fixture"
                upstream_equivalent = "none"
                config_keys = []
                {verification}{feature_body}
                """
            ),
            encoding="utf-8",
        )
        return path

    def write_source_owner(self, feature_ids: tuple[str, ...] = ("feature",)) -> None:
        ids = self.toml_array(list(feature_ids))
        (self.repo_root / "source_owners.toml").write_text(
            textwrap.dedent(
                f"""
                schema_version = 2

                [[owners]]
                id = "feature-owner"
                feature_ids = {ids}
                primary_entries = [{{ path = "src/feature.py", symbol = "main" }}]
                tests = ["tests/test_feature.py"]

                [[owners.relationships]]
                category = "runtime_registration"
                kind = "registers"
                target = "path:src/registry.py"
                evidence = [{{ path = "src/registry.py", symbol = "COMMANDS" }}]
                """
            ),
            encoding="utf-8",
        )

    def install_fake(self, name: str) -> tuple[dict[str, str], Path]:
        bin_dir = self.repo_root / "fake-bin"
        bin_dir.mkdir(exist_ok=True)
        shutil.copy2(self.native_probe, bin_dir / f"{name}.exe")
        log = self.repo_root / f"{name}.log"
        env = os.environ.copy()
        env["PATH"] = f"{bin_dir}{os.pathsep}{env.get('PATH', '')}"
        env["KD4_FAKE_LOG"] = str(log)
        return env, log

    def run_cli(
        self,
        manifest: Path,
        *extra: str,
        repo_root: Path | None = None,
        env: dict[str, str] | None = None,
        json_output: bool = True,
    ) -> subprocess.CompletedProcess[str]:
        root = repo_root or self.repo_root
        command = [
            str(self.just),
            "--justfile",
            str(self.repo_checkout / "justfile"),
            "check-kd4-features",
            "--manifest",
            str(manifest),
            "--repo-root",
            str(root),
            *extra,
        ]
        if json_output:
            command.append("--json")
        return subprocess.run(
            command,
            cwd=root,
            env=env,
            check=False,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )

    def run_json(
        self,
        manifest: Path,
        *extra: str,
        repo_root: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> tuple[subprocess.CompletedProcess[str], dict[str, object]]:
        completed = self.run_cli(
            manifest,
            *extra,
            repo_root=repo_root,
            env=env,
        )
        try:
            payload = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            self.fail(
                f"checker did not emit JSON: {exc}\nstdout={completed.stdout!r}\n"
                f"stderr={completed.stderr!r}"
            )
        self.assertIsInstance(payload, dict)
        return completed, payload

    def assert_finding(
        self,
        manifest: Path,
        code: str,
        *,
        message_fragment: str | None = None,
        env: dict[str, str] | None = None,
    ) -> dict[str, object]:
        completed, payload = self.run_json(manifest, env=env)
        self.assertNotEqual(completed.returncode, 0, completed.stdout)
        findings = payload["findings"]
        self.assertIsInstance(findings, list)
        matching = [
            finding
            for finding in findings
            if isinstance(finding, dict) and finding.get("code") == code
        ]
        self.assertTrue(matching, payload)
        if message_fragment is not None:
            self.assertTrue(
                any(
                    message_fragment in str(finding.get("message"))
                    for finding in matching
                ),
                matching,
            )
        return payload

    def test_cli_is_strict_by_default_with_explicit_opt_out_through_cli(self) -> None:
        manifest = self.write_manifest(status="orphaned")
        strict, strict_payload = self.run_json(manifest)
        relaxed, relaxed_payload = self.run_json(manifest, "--no-strict")
        self.assertEqual(strict.returncode, 1, strict.stderr)
        self.assertFalse(strict_payload["ok"])
        self.assertEqual(relaxed.returncode, 0, relaxed.stderr)
        self.assertTrue(relaxed_payload["ok"])

    def test_contract_schema_version_is_read_from_runtime_constant_through_cli(
        self,
    ) -> None:
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
        self.assert_finding(manifest, "contract-schema-drift")

    def test_default_cli_executes_registration_contract_through_cli(self) -> None:
        manifest = self.write_manifest()
        completed = self.run_cli(manifest, json_output=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("KD4 RUNTIME VERIFICATION [feature]", completed.stdout)

    def test_default_cli_rejects_dead_registration_through_cli(self) -> None:
        (self.repo_root / "src" / "registry.py").write_text(
            "from src.feature import main\n\nCOMMANDS = {}\n",
            encoding="utf-8",
        )
        self.assert_finding(self.write_manifest(), "stale-evidence")

    def test_default_cli_rejects_unimported_registration_through_cli(self) -> None:
        (self.repo_root / "src" / "registry.py").write_text(
            "COMMANDS = {'feature': main}\n",
            encoding="utf-8",
        )
        completed, payload = self.run_json(self.write_manifest())
        self.assertNotEqual(completed.returncode, 0)
        self.assertFalse(payload["ok"])
        self.assertNotEqual(payload["runtimeVerificationExitCode"], 0)
        self.assertEqual(payload["findings"], [])

    def test_desktop_runtime_receipt_feature_is_absent_through_cli(self) -> None:
        manifest = self.write_manifest()
        completed, payload = self.run_json(
            manifest,
            "--no-strict",
            "--run-runtime-verification",
            "desktop-runtime-receipt",
        )
        self.assertEqual(completed.returncode, 2, completed.stderr)
        self.assertEqual(payload["runtimeVerificationExitCode"], 2)

    def test_empty_regex_alone_reports_non_empty_requirement_through_cli(self) -> None:
        evidence = self.valid_evidence().replace(
            'contains = "def main()"',
            'regex = ""',
        )
        self.assert_finding(
            self.write_manifest(evidence),
            "invalid-evidence-match",
            message_fragment="non-empty string",
        )

    def test_empty_regex_is_not_silently_ignored_through_cli(self) -> None:
        evidence = self.valid_evidence().replace(
            'contains = "def main()"',
            'contains = "def main()"\nregex = ""',
        )
        self.assert_finding(
            self.write_manifest(evidence),
            "invalid-evidence-match",
            message_fragment="exactly one",
        )

    def test_enabled_feature_without_registration_fails_through_cli(self) -> None:
        evidence = self.valid_evidence().replace(
            'kind = "registration"',
            'kind = "workflow"',
        )
        self.assert_finding(self.write_manifest(evidence), "missing-registration")

    def test_enabled_runtime_requires_executable_verification_through_cli(self) -> None:
        manifest = self.write_manifest(include_runtime_verification=False)
        self.assert_finding(manifest, "missing-runtime-verification")

    def test_evidence_text_is_cached_across_features_through_cli(self) -> None:
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
        completed, payload = self.run_json(manifest)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["featureCount"], 2)

    def test_feature_config_requires_runtime_status_through_cli(self) -> None:
        manifest = self.write_manifest()
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "config_keys = []",
                'config_keys = ["features.example"]',
            ),
            encoding="utf-8",
        )
        self.assert_finding(manifest, "invalid-runtime-status")

    def test_feature_default_comes_from_machine_readable_rust_export_through_cli(
        self,
    ) -> None:
        (self.repo_root / "codex-rs").mkdir()
        (self.repo_root / "codex-rs" / "Cargo.toml").write_text(
            "[workspace]\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest(
            textwrap.dedent(
                """
                runtime_feature_key = "features.platform_feature"
                runtime_status = "enabled"
                runtime_status_source = "codex-rs/features/src/lib.rs"
                """
            ),
            status="disabled",
            capability_kind="library",
            include_runtime_verification=False,
        )
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "config_keys = []",
                'config_keys = ["features.platform_feature"]',
            ),
            encoding="utf-8",
        )
        env, log = self.install_fake("cargo")
        env["KD4_FAKE_STDOUT"] = json.dumps(
            [{"key": "platform_feature", "defaultEnabled": True}]
        )
        completed, payload = self.run_json(manifest, env=env)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["runtimeStatusCounts"], {"enabled": 1})
        self.assertEqual(
            log.read_text(encoding="utf-8").strip().split("\t")[:5],
            [
                "run",
                "--quiet",
                "--manifest-path",
                str(self.repo_root / "codex-rs" / "Cargo.toml"),
                "-p",
            ],
        )

    def test_json_cli_reports_machine_readable_verdict_through_cli(self) -> None:
        completed, payload = self.run_json(self.write_manifest())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["featureCount"], 1)
        self.assertEqual(payload["runtimeStatusCounts"], {})
        self.assertEqual(payload["runtimeVerificationExitCode"], 0)

    def test_json_cli_reports_runtime_verification_failure_through_cli(self) -> None:
        command = [
            sys.executable,
            "-c",
            "import sys; marker = 'test_feature_is_live'; sys.exit(7)",
        ]
        completed, payload = self.run_json(self.write_manifest(runtime_command=command))
        self.assertEqual(completed.returncode, 7, completed.stderr)
        self.assertFalse(payload["ok"])
        self.assertEqual(payload["runtimeVerificationExitCode"], 7)

    def test_malformed_project_config_is_reported_instead_of_using_defaults_through_cli(
        self,
    ) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features\nexample = true\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest()
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
        self.assert_finding(manifest, "invalid-project-config")

    def test_malformed_upstream_commit_is_rejected_through_cli(self) -> None:
        manifest = self.write_manifest()
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "1111111111111111111111111111111111111111",
                "NOT-A-COMMIT",
            ),
            encoding="utf-8",
        )
        self.assert_finding(manifest, "invalid-upstream-commit")

    def test_missing_generated_artifact_fails_through_cli(self) -> None:
        manifest = self.write_manifest(
            'generated_artifacts = ["generated/feature.json"]\n' + self.valid_evidence()
        )
        self.assert_finding(manifest, "missing-generated-artifact")

    def test_missing_owner_has_one_root_cause_finding_through_cli(self) -> None:
        manifest = self.write_manifest()
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace('owner = "owner"\n', ""),
            encoding="utf-8",
        )
        _, payload = self.run_json(manifest)
        owner_findings = [
            finding
            for finding in payload["findings"]
            if isinstance(finding, dict)
            and (
                "owner" in str(finding.get("message"))
                or finding.get("code") == "invalid-owner"
            )
        ]
        self.assertEqual(
            [finding["code"] for finding in owner_findings],
            ["missing-field"],
        )

    def test_missing_upstream_commit_is_rejected_through_cli(self) -> None:
        manifest = self.write_manifest()
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'upstream_commit = "1111111111111111111111111111111111111111"\n',
                "",
            ),
            encoding="utf-8",
        )
        self.assert_finding(manifest, "invalid-upstream-commit")

    def test_parent_path_escape_fails_through_cli(self) -> None:
        evidence = self.valid_evidence().replace(
            'path = "src/feature.py"',
            'path = "../outside.py"',
        )
        self.assert_finding(self.write_manifest(evidence), "invalid-evidence-path")

    def test_pass_only_runtime_verification_is_rejected_before_execution_through_cli(
        self,
    ) -> None:
        self.write_runtime_test(body="pass")
        self.assert_finding(self.write_manifest(), "vacuous-runtime-verification")

    def test_performance_sensitive_completion_requires_comparable_evidence_through_cli(
        self,
    ) -> None:
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                """
                import copy
                import sys
                import unittest

                sys.path.insert(0, __KD4_REPOSITORY__)
                from scripts import kd4_live_agent_benchmark as benchmark


                def run():
                    return {
                        "success": True,
                        "outcomeCorrect": True,
                        "taskContractCompliant": True,
                        "terminalEvent": "turn.completed",
                        "completionMs": 100.0,
                        "modelWaitMs": 75.0,
                        "actualCommandCount": 4,
                        "duplicateCommandCount": 0,
                        "taskContract": {"successfulTestObserved": True},
                        "latencyExplanation": {
                            "observed": {
                                "postFirstOutputMs": 90.0,
                                "firstWorkspaceMutationObservedMs": 20.0,
                                "firstRequiredTestCompletedObservedMs": 80.0,
                                "requiredTestToTerminalMs": 20.0,
                            },
                            "instrumentedRuntime": {
                                "available": True,
                                "counters": {"logicalGenerationCount": 2},
                                "tokenTotalsAcrossRequests": {"totalTokens": 100},
                            },
                        },
                    }

                class FeatureRegistrationTest(unittest.TestCase):
                    def test_feature_is_live(self):
                        pairs = [
                            {
                                "taskId": task.task_id,
                                "taskShape": task.shape,
                                "repetition": repetition,
                                "currentFork": run(),
                                "upstreamC": run(),
                            }
                            for task in benchmark.BENCHMARK_TASKS
                            for repetition in range(
                                1, benchmark.MIN_GATE_REPETITIONS_PER_TASK + 1
                            )
                        ]
                        passing = benchmark.build_regression_gate(
                            pairs,
                            fork_label="candidate",
                            upstream_label="baseline",
                            experiment_feature="terminalization",
                        )
                        self.assertTrue(passing["passed"], passing)
                        self.assertEqual(
                            passing["thresholds"]["completionMs"],
                            {"maxCandidateToControlRatio": 1.05},
                        )
                        task_id = benchmark.BENCHMARK_TASKS[0].task_id
                        completion = passing["taskGates"][task_id]["metrics"][
                            "completionMs"
                        ]
                        self.assertEqual(
                            passing["taskGates"][task_id]["candidate"], "candidate"
                        )
                        self.assertEqual(
                            passing["taskGates"][task_id]["control"], "baseline"
                        )
                        self.assertEqual(
                            completion["median"],
                            {"candidate": 100.0, "control": 100.0, "passed": True},
                        )
                        self.assertEqual(
                            completion["p90"],
                            {"candidate": 100.0, "control": 100.0, "passed": True},
                        )

                        regressed = copy.deepcopy(pairs)
                        regressed_pair = next(
                            pair
                            for pair in regressed
                            if pair["taskId"] == task_id and pair["repetition"] == 6
                        )
                        regressed_pair["currentFork"]["completionMs"] = 106.0
                        failing = benchmark.build_regression_gate(
                            regressed,
                            fork_label="candidate",
                            upstream_label="baseline",
                            experiment_feature="terminalization",
                        )
                        failed_completion = failing["taskGates"][task_id]["metrics"][
                            "completionMs"
                        ]
                        self.assertFalse(failing["passed"])
                        self.assertTrue(failed_completion["median"]["passed"])
                        self.assertFalse(failed_completion["p90"]["passed"])
                        self.assertTrue(failing["aggregateDiagnosticOnly"]["passed"])
                """
            ).replace("__KD4_REPOSITORY__", repr(str(self.repo_checkout))),
            encoding="utf-8",
        )
        completed, payload = self.run_json(self.write_manifest())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["runtimeVerificationExitCode"], 0)

    def test_planned_feature_cannot_retain_live_route_evidence_through_cli(
        self,
    ) -> None:
        self.assert_finding(
            self.write_manifest(status="planned"),
            "planned-feature-has-production-route",
        )

    def test_project_config_is_parsed_once_for_multiple_feature_lookups_through_cli(
        self,
    ) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features]\nfirst = true\nsecond = false\n",
            encoding="utf-8",
        )
        manifest = self.repo_root / "kd4_features.toml"
        feature_template = """
            [[features]]
            id = "{feature_id}"
            version = 1
            status = "disabled"
            capability_kind = "library"
            owner = "owner"
            summary = "{feature_id}"
            upstream_equivalent = "none"
            config_keys = ["features.{config_key}"]
            runtime_feature_key = "features.{config_key}"
            runtime_status = "{runtime_status}"
            runtime_status_source = ".codex/config.toml"
        """
        manifest.write_text(
            textwrap.dedent(
                """
                schema_version = 2
                upstream_commit = "0123456789abcdef0123456789abcdef01234567"
                status_semantics = "implementation_lifecycle"
                """
            )
            + textwrap.dedent(
                feature_template.format(
                    feature_id="one", config_key="first", runtime_status="enabled"
                )
            )
            + textwrap.dedent(
                feature_template.format(
                    feature_id="two", config_key="second", runtime_status="disabled"
                )
            ),
            encoding="utf-8",
        )
        completed, payload = self.run_json(manifest)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            payload["runtimeStatusCounts"],
            {"disabled": 1, "enabled": 1},
        )

    def test_project_runtime_status_must_match_effective_config_through_cli(
        self,
    ) -> None:
        (self.repo_root / ".codex").mkdir()
        (self.repo_root / ".codex" / "config.toml").write_text(
            "[features]\nexample = false\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest()
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
        self.assert_finding(manifest, "stale-runtime-status")

    def test_repository_core_verifications_use_named_rust_target_through_cli(
        self,
    ) -> None:
        symbol = "tagged_script_and_legacy_shapes_are_explicit"
        self.write_runtime_test(symbol=symbol, body="self.assertTrue(True)")
        command = [
            "just",
            "core-test-fast",
            "core_lib",
            "-E",
            f"test({symbol})",
        ]
        manifest = self.write_manifest(
            self.valid_evidence(symbol),
            runtime_symbol=symbol,
            runtime_command=command,
        )
        env, log = self.install_fake("just")
        completed, payload = self.run_json(manifest, env=env)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(
            log.read_text(encoding="utf-8").strip().split("\t"),
            command[1:],
        )

    def test_repository_intelligence_uses_live_source_owner_workflow_through_cli(
        self,
    ) -> None:
        self.write_source_owner()
        (self.repo_root / "SOURCEMAP.md").write_text("fixture\n", encoding="utf-8")
        (self.repo_root / "architecture_index.json").write_text(
            "{}\n", encoding="utf-8"
        )
        manifest = self.write_manifest(
            'source_owner = "feature-owner"\n'
            'generated_artifacts = ["SOURCEMAP.md", "architecture_index.json"]\n',
            capability_kind="workflow",
            include_runtime_verification=False,
        )
        completed, payload = self.run_json(manifest)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["statusCounts"], {"enabled": 1})

    def test_repository_manifest_passes_non_strict_through_cli(self) -> None:
        manifest = self.repo_checkout / "kd4_features.toml"
        env, log = self.install_fake("just")
        completed, payload = self.run_json(
            manifest,
            "--no-strict",
            "--run-runtime-verification",
            "structured-command-execution",
            repo_root=self.repo_checkout,
            env=env,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["findings"], [])
        self.assertGreater(payload["featureCount"], 1)
        self.assertEqual(payload["runtimeVerificationExitCode"], 0)
        self.assertEqual(
            log.read_text(encoding="utf-8").strip().split("\t")[:2],
            ["core-test-fast", "core_lib"],
        )

    def test_retired_parallel_implementation_fails_if_it_reappears_through_cli(
        self,
    ) -> None:
        (self.repo_root / "src" / "legacy_feature.py").write_text(
            "def main():\n    return 'stale'\n",
            encoding="utf-8",
        )
        manifest = self.write_manifest(
            'retired_paths = ["src/legacy_feature.py"]\n' + self.valid_evidence()
        )
        self.assert_finding(manifest, "parallel-implementation")

    def test_runtime_verification_symbol_must_remain_live_through_cli(self) -> None:
        (self.repo_root / "tests" / "test_feature.py").write_text(
            "# def test_feature_is_live():\ndef removed_test():\n    return True\n",
            encoding="utf-8",
        )
        self.assert_finding(self.write_manifest(), "stale-runtime-verification")

    def test_safe_repo_path_does_not_reresolve_resolved_root_through_cli(self) -> None:
        completed, payload = self.run_json(
            self.write_manifest(),
            repo_root=self.repo_root.resolve(),
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])

    def test_selected_runtime_verification_executes_declared_command_only_through_cli(
        self,
    ) -> None:
        first_marker = self.repo_root / "first.marker"
        second_marker = self.repo_root / "second.marker"
        first_symbol = "test_feature_is_live"
        second_symbol = "test_feature_two_is_live"
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                f"""
                import unittest

                class FeatureRegistrationTest(unittest.TestCase):
                    def {first_symbol}(self):
                        self.assertTrue(True)

                    def {second_symbol}(self):
                        self.assertTrue(True)
                """
            ),
            encoding="utf-8",
        )
        first_command = [
            sys.executable,
            "-c",
            f"from pathlib import Path; Path({str(first_marker)!r}).write_text('{first_symbol}')",
        ]
        second_command = [
            sys.executable,
            "-c",
            f"from pathlib import Path; Path({str(second_marker)!r}).write_text('{second_symbol}')",
        ]
        feature_template = """
            [[features]]
            id = "{feature_id}"
            version = 1
            status = "enabled"
            capability_kind = "runtime"
            owner = "owner"
            summary = "fixture"
            upstream_equivalent = "none"
            config_keys = []
            runtime_verification = {{ kind = "contract_test", path = "tests/test_feature.py", symbol = "{symbol}", command = {command} }}
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
            contains = "{symbol}"
        """
        manifest = self.repo_root / "kd4_features.toml"
        manifest.write_text(
            textwrap.dedent(
                """
                schema_version = 2
                upstream_commit = "0123456789abcdef0123456789abcdef01234567"
                status_semantics = "implementation_lifecycle"
                """
            )
            + textwrap.dedent(
                feature_template.format(
                    feature_id="feature",
                    symbol=first_symbol,
                    command=self.toml_array(first_command),
                )
            )
            + textwrap.dedent(
                feature_template.format(
                    feature_id="feature-two",
                    symbol=second_symbol,
                    command=self.toml_array(second_command),
                )
            ),
            encoding="utf-8",
        )
        completed, payload = self.run_json(
            manifest,
            "--run-runtime-verification",
            "feature",
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(first_marker.read_text(encoding="utf-8"), first_symbol)
        self.assertFalse(second_marker.exists())

    def test_shared_source_owner_liveness_is_observed_once_through_cli(self) -> None:
        self.write_source_owner(("feature", "feature-two"))
        feature_template = """
            [[features]]
            id = "{feature_id}"
            version = 1
            status = "enabled"
            capability_kind = "workflow"
            owner = "owner"
            summary = "fixture"
            upstream_equivalent = "none"
            config_keys = []
            source_owner = "feature-owner"
        """
        manifest = self.repo_root / "kd4_features.toml"
        manifest.write_text(
            textwrap.dedent(
                """
                schema_version = 2
                upstream_commit = "0123456789abcdef0123456789abcdef01234567"
                status_semantics = "implementation_lifecycle"
                """
            )
            + textwrap.dedent(feature_template.format(feature_id="feature"))
            + textwrap.dedent(feature_template.format(feature_id="feature-two")),
            encoding="utf-8",
        )
        completed, payload = self.run_json(manifest)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["featureCount"], 2)

    def test_source_owner_cannot_duplicate_inline_evidence_through_cli(self) -> None:
        self.write_source_owner()
        manifest = self.write_manifest(
            'source_owner = "feature-owner"\n' + self.valid_evidence()
        )
        self.assert_finding(manifest, "duplicate-evidence-authority")

    def test_source_owner_markers_must_resolve_to_live_symbols_through_cli(
        self,
    ) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'symbol = "main"', 'symbol = "removed_entrypoint"'
            ),
            encoding="utf-8",
        )
        payload = self.assert_finding(
            self.write_manifest('source_owner = "feature-owner"'),
            "stale-source-owner-evidence",
        )
        self.assertIn("missing-entrypoint", self.finding_codes(payload))

    def test_source_owner_must_explicitly_own_feature_through_cli(self) -> None:
        self.write_source_owner(("different-feature",))
        self.assert_finding(
            self.write_manifest('source_owner = "feature-owner"'),
            "source-owner-feature-mismatch",
        )

    def test_source_owner_registration_must_have_live_evidence_through_cli(
        self,
    ) -> None:
        self.write_source_owner()
        owner_path = self.repo_root / "source_owners.toml"
        owner_path.write_text(
            owner_path.read_text(encoding="utf-8").replace(
                'symbol = "COMMANDS"', 'symbol = "REMOVED_REGISTRY"'
            ),
            encoding="utf-8",
        )
        payload = self.assert_finding(
            self.write_manifest('source_owner = "feature-owner"'),
            "stale-source-owner-evidence",
        )
        self.assertIn("missing-registration", self.finding_codes(payload))

    def test_source_owner_supplies_reachability_without_inline_markers_through_cli(
        self,
    ) -> None:
        self.write_source_owner()
        completed, payload = self.run_json(
            self.write_manifest('source_owner = "feature-owner"')
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])

    def test_source_owner_symbol_in_comment_is_not_live_evidence_through_cli(
        self,
    ) -> None:
        self.write_source_owner()
        (self.repo_root / "src" / "feature.py").write_text(
            "# def main():\ndef active_entrypoint():\n    return 'live'\n",
            encoding="utf-8",
        )
        payload = self.assert_finding(
            self.write_manifest('source_owner = "feature-owner"'),
            "stale-source-owner-evidence",
        )
        self.assertIn("missing-entrypoint", self.finding_codes(payload))

    def test_stale_marker_fails_through_cli(self) -> None:
        evidence = self.valid_evidence().replace("def main()", "def missing()")
        self.assert_finding(self.write_manifest(evidence), "stale-evidence")

    @staticmethod
    def finding_codes(payload: dict[str, object]) -> set[str]:
        findings = payload["findings"]
        assert isinstance(findings, list)
        return {
            str(finding["code"])
            for finding in findings
            if isinstance(finding, dict) and "code" in finding
        }

    def test_strict_mode_promotes_orphan_to_error_through_cli(self) -> None:
        manifest = self.write_manifest(status="orphaned")
        _, non_strict = self.run_json(manifest, "--no-strict")
        _, strict = self.run_json(manifest)
        non_strict_levels = [
            finding["level"]
            for finding in non_strict["findings"]
            if isinstance(finding, dict) and finding.get("code") == "orphaned-feature"
        ]
        strict_levels = [
            finding["level"]
            for finding in strict["findings"]
            if isinstance(finding, dict) and finding.get("code") == "orphaned-feature"
        ]
        self.assertEqual(non_strict_levels, ["warning"])
        self.assertEqual(strict_levels, ["error"])

    def test_task_continuity_workflow_is_retired_end_to_end_through_cli(self) -> None:
        root_literal = str(self.repo_checkout)
        retired = [
            ".codex/hooks.json",
            ".codex/hooks/task-continuity-entry.ps1",
            ".codex/hooks/task-continuity-fast-basic.ps1",
            ".codex/hooks/task-continuity-fast-compact.ps1",
            ".codex/hooks/task-continuity-fast-session.ps1",
            ".codex/hooks/task-continuity.ps1",
            "codex-rs/core/src/continuity.rs",
            "scripts/test_task_continuity_hook.py",
        ]
        consumers = [
            "codex-rs/core/src/lib.rs",
            "codex-rs/core/src/hook_runtime.rs",
            "codex-rs/core/src/context_manager/history.rs",
        ]
        (self.repo_root / "tests" / "test_feature.py").write_text(
            textwrap.dedent(
                f"""
                import unittest
                from pathlib import Path

                class FeatureRegistrationTest(unittest.TestCase):
                    def test_feature_is_live(self):
                        root = Path({root_literal!r})
                        retired = {retired!r}
                        consumers = {consumers!r}
                        self.assertTrue(all(not (root / path).exists() for path in retired))
                        for path in consumers:
                            source = (root / path).read_text(encoding="utf-8")
                            self.assertNotIn("crate::continuity", source)
                            self.assertNotIn("mod continuity;", source)
                """
            ),
            encoding="utf-8",
        )
        completed, payload = self.run_json(self.write_manifest())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])

    def test_unhashable_feature_id_reports_finding_through_cli(self) -> None:
        manifest = self.write_manifest()
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                'id = "feature"', 'id = ["feature"]'
            ),
            encoding="utf-8",
        )
        self.assert_finding(manifest, "missing-field")

    def test_valid_enabled_feature_passes_through_cli(self) -> None:
        completed, payload = self.run_json(self.write_manifest())
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue(payload["ok"])
        self.assertEqual(payload["statusCounts"], {"enabled": 1})
        self.assertEqual(payload["runtimeStatusCounts"], {})


if __name__ == "__main__":
    unittest.main()
