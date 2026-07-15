from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("verify_ci_decision.py")
SPEC = importlib.util.spec_from_file_location("verify_ci_decision", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class VerifyCiDecisionTest(unittest.TestCase):
    def artifact(self, directory: Path, run: bool = True) -> tuple[Path, str]:
        body = {
            "schema_version": 1,
            "workflows": [{"id": "rust-ci", "run": run}],
            "matrix": {"rust_packages": ["a"], "rust_shards": ["rust-000"]},
        }
        payload = json.dumps(body, indent=2).encode() + b"\n"
        path = directory / "decision.json"
        path.write_bytes(payload)
        return path, "sha256:" + hashlib.sha256(payload).hexdigest()

    def test_hashes_exact_bytes_before_parsing(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path, decision_id = self.artifact(Path(temp))
            consumed = MODULE.consume(path, decision_id, "rust-ci")
            self.assertEqual(consumed["consumed_decision_id"], decision_id)
            path.write_bytes(path.read_bytes() + b" ")
            with self.assertRaises(SystemExit):
                MODULE.consume(path, decision_id, "rust-ci")

    def test_rejects_expected_skip_and_unknown_workflow(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path, decision_id = self.artifact(Path(temp), run=False)
            with self.assertRaises(SystemExit):
                MODULE.consume(path, decision_id, "rust-ci")
            with self.assertRaises(SystemExit):
                MODULE.consume(path, decision_id, "sdk")

    def test_rejects_artifact_body_containing_decision_id(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path, _ = self.artifact(Path(temp))
            body = json.loads(path.read_bytes())
            body["decision_id"] = "forbidden"
            payload = json.dumps(body).encode()
            path.write_bytes(payload)
            decision_id = "sha256:" + hashlib.sha256(payload).hexdigest()
            with self.assertRaises(SystemExit):
                MODULE.consume(path, decision_id, "rust-ci")


if __name__ == "__main__":
    unittest.main()
