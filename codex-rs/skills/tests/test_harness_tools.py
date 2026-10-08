"""Exercise the actually installed bundle, not imports from the source checkout."""

import ast
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import unittest


BUNDLE = Path(sys.argv.pop(1)).resolve()
LAUNCHER = BUNDLE / "scripts/harness_tools.py"


class InstalledHarnessToolsTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.repo = self.root / "unrelated project"
        self.repo.mkdir()
        self.env = dict(os.environ, PYTHONPATH=str(self.repo), HOME=str(self.root),
                        USERPROFILE=str(self.root), CODEX_HOME=str(self.root / "home"))
        # Neither project modules nor PYTHONPATH may replace bundled imports.
        (self.repo / "scripts").mkdir()
        for path in (self.repo / "scripts/__init__.py", self.repo / "atomic_json.py",
                     self.repo / "sitecustomize.py"):
            path.write_text("raise RuntimeError('imported target repository code')\n", encoding="utf-8")

    def invoke(self, *args, status=0, launcher=LAUNCHER):
        result = subprocess.run(
            [sys.executable, "-I", "-B", str(launcher), *map(str, args)],
            cwd=self.repo, env=self.env, capture_output=True, text=True,
            encoding="utf-8", timeout=60,
        )
        self.assertEqual(result.returncode, status, result.stderr + result.stdout[:2000])
        return result

    def rollout(self):
        path = self.repo / "session.jsonl"
        records = [
            {"type": "session_meta", "payload": {"cwd": str(self.repo)}},
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "one"}},
        ]
        for i in range(12):
            records.extend([
                {"type": "response_item", "payload": {"type": "custom_tool_call",
                 "call_id": str(i), "name": "exec", "input": "read source"}},
                {"type": "response_item", "payload": {"type": "custom_tool_call_output",
                 "call_id": str(i), "output": "evidence λ " * 1000}},
            ])
        records.append({"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "one"}})
        path.write_text("".join(json.dumps(r) + "\n" for r in records), encoding="utf-8")
        return path

    def test_catalog_and_all_public_contracts_are_available_without_checkout(self):
        result = self.invoke("--describe")
        catalog = json.loads(result.stdout)
        self.assertEqual(catalog["format"], "codex_harness_tools_v1")
        self.assertEqual(Path(catalog["launcher"][-1]), LAUNCHER)
        self.assertEqual(set(catalog["commands"]), {
            "inventory", "session-audit", "tool-results", "validation", "snapshot",
        })
        for command, contract in catalog["commands"].items():
            with self.subTest(command=command):
                output = self.invoke(command, contract["contract"])
                self.assertTrue(output.stdout)
        self.assertFalse(list(BUNDLE.rglob("__pycache__")))

    def test_bundle_contains_the_local_import_closure(self):
        library = BUNDLE / "scripts/lib"
        names = {path.stem for path in library.glob("*.py")}
        self.assertIn("rollout_audit_cache", names)
        for path in library.glob("*.py"):
            for node in ast.walk(ast.parse(path.read_text(encoding="utf-8"))):
                if isinstance(node, ast.ImportFrom) and node.module:
                    if node.module == "scripts":
                        self.assertTrue({alias.name for alias in node.names} <= names)
                    elif node.module.startswith("scripts."):
                        self.assertIn(node.module.split(".")[1], names)

    def test_inventory_scans_target_and_reuses_state_without_rescanning(self):
        # A real Git fixture is required to verify the inventory's source-enumeration contract.
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True,
                       capture_output=True, timeout=30)
        (self.repo / "hello.txt").write_text("hello λ", encoding="utf-8")
        subprocess.run(["git", "-C", str(self.repo), "add", "--", "hello.txt"],
                       check=True, capture_output=True, timeout=30)
        (self.repo / "untracked.txt").write_text("untracked evidence", encoding="utf-8")
        query = self.root / "query.json"
        query.write_text(json.dumps({"categories": [
            {"name": "text", "paths": ["*.txt"], "verification": "path"},
        ]}), encoding="utf-8")
        state = self.root / "inventory.json"
        scanned = json.loads(self.invoke("inventory", "--root", self.repo,
            "--query", query, "--state", state, "--paths").stdout)
        self.assertEqual(scanned["paths"], ["hello.txt"])
        self.assertEqual(json.loads(state.read_bytes())["output"]["untracked_paths"], ["untracked.txt"])
        self.assertTrue(scanned["ready_to_render"])
        original = state.read_bytes()
        (self.repo / "hello.txt").unlink()
        replay = json.loads(self.invoke("inventory", "--root", self.repo,
            "--state", state, "--render-only", "--paths").stdout)
        self.assertEqual(replay["paths"], ["hello.txt"])
        self.assertEqual(state.read_bytes(), original)

    def test_session_audit_retains_report_and_compares_without_live_source(self):
        source = self.rollout()
        result = self.invoke("session-audit", source.name, "--repo-root", self.repo,
                             "--cache-dir", self.root / "reports", "--summary-json")
        summary = json.loads(result.stdout)
        self.assertIn("summaryBudget", summary)
        receipt = json.loads(result.stderr)["savedReport"]
        report = Path(receipt["path"])
        saved = report.read_bytes()
        full = json.loads(saved)
        self.assertNotIn("summaryBudget", full)
        self.assertEqual(Path(full["repoRoot"]), self.repo)
        self.assertIn(hashlib.sha256(saved).hexdigest(), json.dumps(receipt))
        source.unlink()
        replay = self.invoke("session-audit", "--from-report", report, "--baseline", report,
                             "--summary-json")
        self.assertIn("baselineComparison", json.loads(replay.stdout))
        self.assertIn("freshness is not checked", replay.stderr)
        self.assertEqual(report.read_bytes(), saved)
        self.invoke("session-audit", "--from-report", report, "--repo-root", self.repo, status=2)

    def test_tool_ledger_publication_replay_and_no_overwrite(self):
        source = self.rollout()
        report = self.root / "tools.json"
        result = self.invoke("tool-results", source.name, "--output", report)
        summary = json.loads(result.stdout)
        saved = report.read_bytes()
        full = json.loads(saved)
        self.assertEqual(len(full["sessions"][0]["results"]), 12)
        self.assertEqual(summary["report_bytes"], len(saved))
        self.assertEqual(summary["report_sha256"], hashlib.sha256(saved).hexdigest())
        source.unlink()
        replay = self.invoke("tool-results", "--from-report", report)
        self.assertEqual(json.loads(replay.stdout), summary)
        self.assertEqual(report.read_bytes(), saved)
        self.invoke("tool-results", source.name, "--output", report, status=1)
        self.assertEqual(report.read_bytes(), saved)

    def test_validation_reads_existing_receipts_without_running_tests(self):
        library = str(BUNDLE / "scripts/lib")
        sys.path.insert(0, library)
        try:
            owner = runpy.run_path(str(Path(library) / "validation_metrics.py"))
            ledger = owner["ValidationMetrics"](["project-test-command"])
            ledger.bind(self.repo)
            with contextlib.redirect_stderr(io.StringIO()):
                ledger.finish("passed")
        finally:
            sys.path.remove(library)
        saved = ledger.path.read_bytes()
        result = json.loads(self.invoke("validation", ledger.path, "--json").stdout)
        self.assertEqual(result["launches"], 1)
        self.assertEqual(result["child_commands"], 0)
        self.assertEqual(result["launches_producing_current_proof"], 0)
        self.assertEqual(ledger.path.read_bytes(), saved)

    def test_snapshot_copies_verified_external_payload_and_rejects_corruption(self):
        source = self.repo / "external.jsonl"
        data = json.dumps({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "external"}}).encode()
        digest = hashlib.sha256(data).hexdigest()
        payload_dir = self.repo / "rollout-payloads"
        payload_dir.mkdir()
        payload = payload_dir / (digest + ".json")
        payload.write_bytes(data)
        source.write_text(json.dumps({"type": "rollout_payload_artifact", "payload": {
            "sha256": digest, "bytes": len(data), "item_type": "event_msg",
        }}) + "\n", encoding="utf-8")
        output = self.root / "snapshot.jsonl"
        self.invoke("snapshot", source.name, "--output", output)
        self.assertEqual(output.read_bytes(), source.read_bytes())
        self.assertEqual((self.root / "rollout-payloads" / payload.name).read_bytes(), data)
        payload.write_bytes(b"wrong bytes")
        self.invoke("snapshot", source.name, "--output", self.root / "bad.jsonl", status=1)
        self.assertFalse((self.root / "bad.jsonl").exists())

    def test_missing_payload_and_unknown_command_fail_without_checkout_fallback(self):
        launcher = self.root / "standalone.py"
        shutil.copyfile(LAUNCHER, launcher)
        self.invoke("inventory", "--describe", launcher=launcher, status=2)
        self.invoke("unknown", status=2)


if __name__ == "__main__":
    unittest.main()
