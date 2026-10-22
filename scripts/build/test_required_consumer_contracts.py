"""Declared producer changes must retain consumer checks, not just local owners."""

import contextlib
import io
import json
import os
import subprocess
from unittest import mock

from scripts.build import rust_test_runner as r
from scripts.build.test_rust_test_runner import RunnerTestCase, nextest_run_payload


class RequiredConsumerContractsTest(RunnerTestCase):
    def contract_manifest(self, **changes):
        declaration = {
            "paths": ["producer/src/api.rs"],
            "required_gates": ["consumer"],
            "consumer": "Consumer requires the original wire value",
        }
        declaration.update(changes)
        return self.manifest(
            gates={
                "local": {
                    "steps": [
                        {"target": "core_lib", "tests": ["positive"], "helpers": []}
                    ]
                },
                "consumer": {
                    "steps": [
                        {
                            "target": "core_lib",
                            "tests": ["wire_contract"],
                            "helpers": [],
                        }
                    ]
                },
            },
            contracts={"version": 1, "obligations": {"wire": declaration}},
        )

    def test_producer_requires_consumer_without_execution_or_module_discovery(self):
        manifest = self.contract_manifest()
        result = manifest.required_contracts(["producer/src/api.rs"], cwd=self.temp_dir)
        self.assertEqual(result["required_gates"], ["consumer"])
        self.assertEqual(
            result["required_obligations"]["wire"]["triggered_by"],
            ["producer/src/api.rs"],
        )
        runner, executor = self.runner(manifest=manifest)
        advice = runner.gates_for(["producer/src/api.rs"], cwd=self.temp_dir)
        self.assertEqual(advice["gates"], ["consumer"])
        self.assertEqual(advice["advisory_gates"], [])
        self.assertEqual(executor.calls, [])
        self.assertEqual(
            manifest.required_contracts(["README.md"], cwd=self.temp_dir)[
                "required_gates"
            ],
            [],
        )
        self.assertTrue(
            manifest.required_contracts(
                [str(self.temp_dir.parent / "outside.rs")], cwd=self.temp_dir
            )["unresolved_contract_paths"]
        )

    def test_cli_resolves_repository_relative_changed_paths_from_the_just_working_directory(
        self,
    ):
        # `just core-gate` runs the runner from codex-rs; the documented
        # repository-relative changed path must still add its consumer gate.
        manifest_path = self.temp_dir / "manifest.toml"
        manifest_path.write_bytes(b"manifest bytes")
        self.addCleanup(os.chdir, os.getcwd())
        os.chdir(r.REPO_ROOT / "codex-rs")
        with mock.patch.object(
            r.Manifest, "load", return_value=self.contract_manifest()
        ):
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                self.assertEqual(
                    r._main(
                        r.build_parser().parse_args(
                            [
                                "--manifest",
                                str(manifest_path),
                                "required-for",
                                "producer/src/api.rs",
                            ]
                        )
                    ),
                    0,
                )
            self.assertEqual(
                json.loads(stdout.getvalue())["required_gates"], ["consumer"]
            )
            args = r.build_parser().parse_args(
                [
                    "--manifest",
                    str(manifest_path),
                    "run-gate",
                    "--changed-path",
                    "producer/src/api.rs",
                    "local",
                ]
            )
            with (
                mock.patch.object(
                    r, "load_metadata", side_effect=r.RunnerError("stop before Cargo")
                ) as metadata,
                contextlib.redirect_stderr(io.StringIO()),
            ):
                self.assertEqual(r._main(args), 2)
        metadata.assert_called_once_with()
        self.assertEqual(args.names, ["local", "consumer"])

    def test_unknown_gate_and_unsupported_contract_version_fail_before_cargo(self):
        with self.assertRaises(r.RunnerError):
            self.contract_manifest(required_gates=["missing"])
        with self.assertRaises(r.RunnerError):
            self.manifest(contracts={"version": 2, "obligations": {}})
        for path in ("../api.rs", "/api.rs", "producer\\api.rs"):
            with self.subTest(path=path), self.assertRaises(r.RunnerError):
                self.contract_manifest(paths=[path])

    def test_irrelevant_green_cannot_certify_the_required_consumer(self):
        manifest = self.contract_manifest()
        runner, _ = self.runner(manifest=manifest)
        calls = []

        def execute(argv, **kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(
                argv,
                100,
                nextest_run_payload([("positive", "ok"), ("wire_contract", "failed")]),
                "",
            )

        runner.executor = execute
        required = manifest.required_contracts(
            ["producer/src/api.rs"], cwd=self.temp_dir
        )
        with (
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            with self.assertRaises(r.RunnerError) as caught:
                runner.run_gates(["local", *required["required_gates"]])
        self.assertNotIn("consumer", caught.exception.completed_gates)
        self.assertEqual(
            len(calls), 1, "compatible local and consumer checks share one execution"
        )
