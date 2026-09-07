"""Real reconciliation-worker regressions for the V2 selection boundary."""

from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid
from pathlib import Path

import tomllib

from scripts import completion_proof_inventory_v2 as contracts
from scripts.completion_proof import reconcile_inventory
from scripts.completion_proof_v2_live import MEMBERS

ROOT = Path(__file__).resolve().parents[1]


class InventoryV2LiveWorkerTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="kd4-v2-live-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.validation = self.root / ".codex/validation"
        self.validation.mkdir(parents=True)
        self.generation = self.validation / "inventory-generations/fixture"
        self.generation.mkdir(parents=True)
        for name in MEMBERS:
            shutil.copyfile(ROOT / ".codex/validation" / name, self.generation / name)
        for name in ("frozen-test-inventory-v1.json", "test-replacements-v1.json"):
            shutil.copyfile(ROOT / ".codex/validation" / name, self.validation / name)
        config = (ROOT / ".codex/validation/completion-proof.toml").read_text()
        parts = config.split("[[validation]]")
        config = parts[0] + "".join(
            "[[validation]]" + part
            for part in parts[1:]
            if tomllib.loads(part)["runner"] not in {"typed-validation", "rust-gate"}
        )
        config = re.sub(
            r"(?m)^frozen_inventory = .*$",
            'frozen_inventory = ".codex/validation/inventory-generations/fixture/frozen-test-inventory-v2.json"',
            config,
        )
        config = re.sub(
            r"(?m)^replacement_ledger = .*$",
            'replacement_ledger = ".codex/validation/inventory-generations/fixture/test-replacements-v2.json"',
            config,
        )
        self.config = self.validation / "completion-proof.toml"
        self.config.write_text(config)
        v1 = json.loads((self.validation / "test-replacements-v1.json").read_bytes())
        successors = {
            identity
            for row in v1["rows"]
            for identity in row.get("replacement_ids", [])
        }
        v2 = json.loads(
            (self.generation / "frozen-test-inventory-v2.json").read_bytes()
        )
        successors.update(
            item["entry"]["executable_identity"]["test_id"]
            for item in v2["declaration_universe"]
            if item["kind"] == "post-baseline-current"
        )
        rows = []
        for identity in sorted(successors):
            framework = (
                identity.split("::", 1)[0] if "::" in identity else "python-unittest"
            )
            native = identity.split("::", 1)[-1]
            rows.append(
                {
                    "baseline_id": identity
                    if "::" in identity
                    else f"python-unittest::{identity}",
                    "framework": framework,
                    "native_id": native,
                    "source": "tests/fixture.py",
                    "ignored": False,
                    "platforms": ["windows"],
                }
            )
        self.input = self.root / "input.json"
        self.payload = {
            "schema_version": 1,
            "execution_id": str(uuid.uuid4()),
            "current_rows": rows,
            "known_validation_ids": [
                item["id"] for item in tomllib.loads(config)["validation"]
            ],
        }
        self.input.write_text(json.dumps(self.payload))

    def run_worker(
        self, transition: bool, *, by_path: bool = False
    ) -> subprocess.CompletedProcess[str]:
        if by_path:
            # Launch the runner by file path from outside the repository, exactly
            # like Just's runpy launcher and the worker child do: the `scripts`
            # package is not importable there, so sibling imports must resolve.
            command = [
                sys.executable,
                "-c",
                "import runpy, sys; module = runpy.run_path(sys.argv[1]); "
                "raise SystemExit(module['_dispatch'](sys.argv[2:], allow_test_config=True))",
                str(ROOT / "scripts/completion_proof.py"),
            ]
            cwd = self.root
        else:
            command = [
                sys.executable,
                "-c",
                "from scripts.completion_proof import _dispatch; import sys; sys.exit(_dispatch(sys.argv[1:], allow_test_config=True))",
            ]
            cwd = ROOT
        command += [
            "--config",
            str(self.config),
            "reconciliation-worker",
            "--input",
            str(self.input),
        ]
        if transition:
            command.append("--transition-readiness")
        return subprocess.run(
            command, cwd=cwd, text=True, capture_output=True, timeout=120, check=False
        )

    def test_real_worker_selects_one_v2_generation_and_retains_unadmitted_obligations(
        self,
    ) -> None:
        before = {name: (self.generation / name).read_bytes() for name in MEMBERS}
        ready = self.run_worker(True)
        self.assertEqual(ready.returncode, 0, ready.stderr)
        self.assertTrue(json.loads(ready.stdout)["transition_readiness"])
        by_path = self.run_worker(True, by_path=True)
        self.assertEqual(by_path.returncode, 0, by_path.stderr)
        self.assertTrue(json.loads(by_path.stdout)["transition_readiness"])
        full = self.run_worker(False)
        self.assertEqual(full.returncode, 2, full.stderr)
        self.assertIn("replacement remains unadmitted", full.stderr)
        self.assertEqual(
            before, {name: (self.generation / name).read_bytes() for name in MEMBERS}
        )

    def test_real_worker_rejects_missing_recovery_child_even_with_recomputed_hashes(
        self,
    ) -> None:
        ledger_path = self.generation / "test-replacements-v2.json"
        ledger = json.loads(ledger_path.read_bytes())
        child = next(
            row
            for row in ledger["rows"]
            if row["baseline_id"] is None and row["disposition"]["kind"] == "unresolved"
        )
        ledger["rows"].remove(child)
        ledger.pop("semantic_sha256")
        ledger.pop("self_hash")
        ledger["semantic_sha256"] = contracts.proof_hash(
            "kd4.test-replacement-ledger.v2.semantic", ledger
        )
        ledger["self_hash"] = contracts.proof_hash(
            "kd4.test-replacement-ledger.v2.self", ledger
        )
        ledger_path.write_bytes(contracts.canonical_jcs(ledger))
        result = self.run_worker(True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("obligation", result.stderr)

    def test_real_worker_rejects_mixed_inventory_and_ledger_generations(self) -> None:
        shutil.copyfile(
            self.generation / "test-replacements-v2.json",
            self.validation / "test-replacements-v2.json",
        )
        self.config.write_text(
            self.config.read_text().replace(
                "inventory-generations/fixture/test-replacements-v2.json",
                "test-replacements-v2.json",
            )
        )
        before = self.config.read_bytes()
        result = self.run_worker(True)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("one generation", result.stderr)
        self.assertEqual(self.config.read_bytes(), before)

    def test_v2_selection_executes_the_native_unittest_through_the_real_runner(
        self,
    ) -> None:
        selection = reconcile_inventory(
            self.root,
            tomllib.loads(self.config.read_text()),
            self.payload["current_rows"],
            known_validation_ids=set(self.payload["known_validation_ids"]),
            transition_readiness=True,
        )
        native = "scripts.test_check_kd4_features.CheckKd4FeaturesTest.test_desktop_runtime_receipt_feature_is_absent_through_cli"
        selected = [
            identity
            for identity in selection.required_by_framework["python-unittest"]
            if identity.endswith(native)
        ]
        self.assertEqual(len(selected), 1)
        expected = self.root / "expected.json"
        expected.write_text(json.dumps(selected))
        report_path = self.root / "runner.json"
        result = subprocess.run(
            [
                "uv",
                "run",
                "--offline",
                "--frozen",
                "--project",
                "scripts",
                "python",
                str(ROOT / "scripts/completion_proof_unittest.py"),
                "run",
                "--expected-file",
                str(expected),
                "--output",
                str(report_path),
                "--proof-attempt-id",
                str(uuid.uuid4()),
                "--proof-execution-id",
                str(uuid.uuid4()),
                "--proof-receipt-nonce",
                str(uuid.uuid4()),
                "--proof-scope",
                "focused",
            ],
            cwd=ROOT,
            text=True,
            capture_output=True,
            timeout=120,
            check=False,
        )
        report = json.loads(report_path.read_bytes())
        self.assertEqual(result.returncode, 0, (result.stderr, report))
        for field in ("selected_ids", "started_ids", "terminal_ids", "executed_ids"):
            self.assertEqual(report[field], [native])
        for framework, selected_ids in selection.required_by_framework.items():
            expected_ids = sorted(
                row["native_id"]
                for row in self.payload["current_rows"]
                if row["framework"] == framework
            )
            self.assertEqual(sorted(selected_ids), expected_ids)
