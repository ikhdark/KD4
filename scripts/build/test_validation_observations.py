"""Content attribution controls; no Cargo or analyzed source is executed."""

import argparse
import contextlib
import hashlib
import io
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from dataclasses import replace
from unittest import mock
from scripts.build import rust_test_runner
from scripts.build.test_rust_test_runner import RunnerTestCase, nextest_run_payload
from scripts.build.validation_observations import (
    artifact_observations,
    comparison,
    source_observations,
)


class ValidationObservationsTest(unittest.TestCase):
    def test_overlapping_roots_share_observations_and_budgets(self):
        from scripts.build import validation_observations as owner

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            nested = root / "tests"
            nested.mkdir()
            source = nested / "lib.rs"
            source.write_bytes(b"x" * 1024)
            expected = [
                source_observations([path])[0] for path in (root, nested)
            ]
            with (
                mock.patch.object(owner, "file_observation", wraps=owner.file_observation) as read,
                mock.patch.object(owner.os, "scandir", wraps=owner.os.scandir) as scan,
            ):
                actual = source_observations(
                    [nested, root, nested], max_entries=2, max_bytes=1536
                )
            self.assertEqual(actual, expected)
            self.assertEqual(read.call_count, 1)
            self.assertEqual(scan.call_count, 2)

    def test_same_size_restored_mtime_source_and_artifact_changes_are_observed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "lib.rs"
            source.write_text("fn alpha() {}")
            binary = root / "target" / "helper"
            binary.parent.mkdir()
            binary.write_bytes(b"old")
            before = source_observations([root])
            self.assertEqual(
                comparison(before, source_observations([root])), "observed_unchanged"
            )
            stamp = source.stat()
            source.write_text("fn bravo() {}")
            os.utime(source, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
            self.assertEqual(comparison(before, source_observations([root])), "changed")
            artifacts = artifact_observations({"helper": binary})
            binary.write_bytes(b"new")
            self.assertEqual(
                comparison(artifacts, artifact_observations({"helper": binary})),
                "changed",
            )
            binary.unlink()
            self.assertEqual(
                artifact_observations({"helper": binary})["helper"]["state"],
                "unavailable",
            )

    def test_unknown_scope_never_claims_an_unchanged_identity(self):
        self.assertEqual(comparison([], []), "unknown")
        self.assertEqual(comparison({}, {}), "unknown")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "ignored_authored.toml").write_text("mode = 1")
            before = source_observations([root])
            (root / "ignored_authored.toml").write_text("mode = 2")
            self.assertEqual(comparison(before, source_observations([root])), "changed")
            for limited in (
                source_observations([root], max_entries=0),
                source_observations([root], max_bytes=0),
            ):
                self.assertEqual(limited[0]["state"], "incomplete")
                self.assertIsNone(limited[0]["sha256"])
                self.assertEqual(comparison(limited, limited), "unknown")
            missing = source_observations([root / "missing"])
            self.assertEqual(comparison(missing, missing), "unknown")


class RunnerObservationHandoffTest(RunnerTestCase):
    def test_dispatch_retains_source_change_without_promoting_it_to_replay_proof(self):
        manifest = self.manifest()
        manifest.targets["core_lib"] = replace(
            manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
        )
        runner, _ = self.runner(manifest=manifest)
        package = self.temp_dir / "package"
        package.mkdir()
        cargo_manifest = package / "Cargo.toml"
        cargo_manifest.write_text("fixture")
        source = package / "lib.rs"
        source.write_text("fn before() {}")
        runner.metadata.packages["codex-core"]["manifest_path"] = str(cargo_manifest)
        calls = []

        def execute(args, **_kwargs):
            calls.append(args)
            source.write_text("fn after_() {}")
            return subprocess.CompletedProcess(
                args, 0, nextest_run_payload([("alpha", "ok")]), ""
            )

        runner.executor = execute
        args = argparse.Namespace(
            command="run-target",
            name="core_lib",
            names=[],
            manifest=cargo_manifest,
            admission_timeout_seconds=0,
        )
        stdout = io.StringIO()
        with (
            mock.patch.object(rust_test_runner.Manifest, "load", return_value=manifest),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(
                rust_test_runner._dispatch_with_admission(
                    args,
                    runner,
                    runner.metadata,
                    "a" * 64,
                    ["-E", "test(=alpha)"],
                    False,
                ),
                0,
            )
        receipt = next(
            json.loads(line)
            for line in stdout.getvalue().splitlines()
            if line.startswith("{")
            and json.loads(line).get("kind") == "codex_test_execution_v1"
        )
        observation = receipt["dependency_manifest"]["execution_observation"]
        self.assertEqual(observation["source_comparison"], "changed")
        self.assertNotEqual(
            observation["launch_sources"][0]["sha256"],
            observation["terminal_sources"][0]["sha256"],
        )
        self.assertFalse(observation["automatic_replay_allowed"])
        self.assertEqual(observation["compiler_read_correspondence"], "unverified")
        self.assertEqual(
            observation["test_binary_identity"], "unavailable_from_reporter"
        )
        self.assertEqual(receipt["executed_tests"], 1)
        self.assertEqual(
            len(calls), 1, "observations must not add discovery or duplicate execution"
        )


class ExecutionReceiptPublicationTest(RunnerTestCase):
    """Receipts reach the harness intact, under a lane-independent identity."""

    def publish(self, target_dir: Path, *, cargo_profile=None) -> list[dict]:
        manifest = self.manifest()
        manifest.targets["core_lib"] = replace(
            manifest.targets["core_lib"], helpers=(), helpers_by_test_prefix={}
        )
        target_dir.mkdir(parents=True, exist_ok=True)
        runner = rust_test_runner.RustTestRunner(
            manifest,
            self.metadata(),
            target_dir=target_dir,
            platform="windows",
            cargo_profile=cargo_profile,
            executor=lambda args, **_kwargs: subprocess.CompletedProcess(
                args, 0, nextest_run_payload([("alpha", "ok")]), ""
            ),
        )
        cargo_manifest = self.temp_dir / "package" / "Cargo.toml"
        cargo_manifest.parent.mkdir(exist_ok=True)
        cargo_manifest.write_text("fixture")
        runner.metadata.packages["codex-core"]["manifest_path"] = str(cargo_manifest)
        args = argparse.Namespace(
            command="run-target",
            name="core_lib",
            names=[],
            manifest=cargo_manifest,
            admission_timeout_seconds=0,
        )
        stdout = io.StringIO()
        with (
            mock.patch.object(rust_test_runner.Manifest, "load", return_value=manifest),
            contextlib.redirect_stdout(stdout),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            self.assertEqual(
                rust_test_runner._dispatch_with_admission(
                    args,
                    runner,
                    runner.metadata,
                    "a" * 64,
                    ["-E", "test(=alpha)"],
                    False,
                ),
                0,
            )
        return [
            json.loads(line)
            for line in stdout.getvalue().splitlines()
            if line.startswith("{")
        ]

    @staticmethod
    def receipt(lines: list[dict]) -> dict:
        return next(
            line for line in lines if line.get("kind") == "codex_test_execution_v1"
        )

    def test_large_receipt_is_retained_and_referenced_by_digest(self):
        # Harness output retention keeps a 512 KiB tail, so a longer receipt
        # line would be cut; it must arrive as one hash-bound reference.
        with mock.patch.object(rust_test_runner, "INLINE_RECEIPT_MAX_BYTES", 64):
            lines = self.publish(self.target_dir)
        kinds = [line.get("kind") for line in lines]
        self.assertNotIn("codex_test_execution_v1", kinds)
        self.assertEqual(kinds.count("codex_test_execution_ref_v1"), 1)
        reference = next(
            line for line in lines if line.get("kind") == "codex_test_execution_ref_v1"
        )
        self.assertEqual(reference["runner"], "rust_test_runner")
        retained = Path(reference["path"])
        self.assertTrue(retained.is_absolute())
        self.assertEqual(retained.parent.name, "test-runner-logs")
        self.assertTrue(retained.name.startswith("execution-receipt-"))
        data = retained.read_bytes()
        self.assertEqual(len(data), reference["bytes"])
        self.assertEqual(hashlib.sha256(data).hexdigest(), reference["sha256"])
        receipt = json.loads(data)
        self.assertEqual(receipt["kind"], "codex_test_execution_v1")
        self.assertEqual(receipt["executed_tests"], 1)
        self.assertEqual(
            [execution["test"] for execution in receipt["executions"]], ["alpha"]
        )
        self.assertEqual(reference["executed_tests"], receipt["executed_tests"])
        # Negative control: a receipt within the bound stays inline.
        kinds = [
            line.get("kind") for line in self.publish(self.temp_dir / "lanes" / "inline")
        ]
        self.assertEqual(kinds.count("codex_test_execution_v1"), 1)
        self.assertNotIn("codex_test_execution_ref_v1", kinds)

    def test_execution_configuration_is_lane_independent(self):
        # Admission may run a repair in a sibling lane of the same family; the
        # repair identity must not differ by build-cache location.
        first = self.receipt(self.publish(self.temp_dir / "lanes" / "core-tests"))
        second = self.receipt(self.publish(self.temp_dir / "lanes" / "core-tests-2"))
        self.assertNotIn("target_dir", first["execution_configuration"])
        self.assertEqual(
            first["execution_configuration"], second["execution_configuration"]
        )
        # The lane stays diagnosable in the admission record.
        self.assertNotEqual(
            first["admission"]["target_dir"], second["admission"]["target_dir"]
        )
        # Negative control: a real configuration difference still separates runs.
        profiled = self.receipt(
            self.publish(self.temp_dir / "lanes" / "profiled", cargo_profile="dev-small")
        )
        self.assertEqual(profiled["execution_configuration"]["cargo_profile"], "dev-small")
        self.assertNotEqual(
            first["execution_configuration"], profiled["execution_configuration"]
        )
