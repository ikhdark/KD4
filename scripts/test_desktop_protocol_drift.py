#!/usr/bin/env python3

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from scripts import desktop_protocol_drift as drift


def schema_with_methods(*methods: str) -> dict:
    return {
        "oneOf": [
            {
                "properties": {"method": {"enum": [method]}, "params": {}},
                "required": ["method", "params"],
            }
            for method in methods
        ]
    }


class DesktopProtocolDriftTest(unittest.TestCase):
    def test_just_recipe_uses_repository_schema_and_preserves_arguments(self) -> None:
        repo = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory(prefix="desktop drift ") as temp_dir:
            root = Path(temp_dir)
            logs = root / "Desktop logs"
            logs.mkdir()
            log = logs / "desktop.log"
            log.write_text("unknown variant `initialize`", encoding="utf-8")
            command = ["just", "desktop-protocol-drift", "--logs", str(logs), "--json"]

            def run(*extra: str) -> subprocess.CompletedProcess[str]:
                return subprocess.run(
                    [*command, *extra],
                    cwd=repo / "scripts",
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    timeout=30,
                    check=False,
                )

            present = run()
            self.assertEqual(present.returncode, 0, present.stderr)
            self.assertEqual(json.loads(present.stdout)["missing"], [])
            self.assertEqual(
                json.loads(present.stdout)["registered_since"],
                [{"method": "initialize", "count": 1}],
            )

            log.write_text("unknown variant `audit/missingMethod`", encoding="utf-8")
            missing = run()
            self.assertEqual(missing.returncode, 1, missing.stderr)
            self.assertEqual(
                json.loads(missing.stdout)["missing"],
                [{"method": "audit/missingMethod", "count": 1, "logs": [str(log)]}],
            )
            schema = root / "custom schema.json"
            schema.write_text(
                json.dumps(schema_with_methods("audit/missingMethod")), encoding="utf-8"
            )
            overridden = run("--schema", str(schema))
            self.assertEqual(overridden.returncode, 0, overridden.stderr)
            self.assertEqual(json.loads(overridden.stdout)["missing"], [])
            absent = run("--logs", str(root / "absent logs"))
            self.assertEqual(absent.returncode, 2, absent.stderr)
            self.assertIn("log directory not found", absent.stderr)

    def test_schema_methods_collects_every_method_enum(self) -> None:
        schema = schema_with_methods("initialize", "thread/start", "project/list")
        self.assertEqual(
            drift.schema_methods(schema), {"initialize", "thread/start", "project/list"}
        )

    def test_missing_methods_are_those_rejected_but_unregistered(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            log = Path(temp_dir) / "codex-desktop-1.log"
            log.write_text(
                "\n".join(
                    [
                        "info [host] request_routed",
                        'error [host-app-server-projects] errorMessage="Invalid request: '
                        'unknown variant `project/list`, expected one of `initialize`"',
                        "error [host] unknown variant `thread/queue/list`",
                        "error [host] unknown variant `project/list`",
                    ]
                ),
                encoding="utf-8",
            )
            rejected = drift.rejected_methods([log])
            missing, present = drift.drift_report(
                rejected,
                drift.schema_methods(schema_with_methods("initialize", "project/list")),
            )

        self.assertEqual([entry.method for entry in missing], ["thread/queue/list"])
        self.assertEqual([entry.method for entry in present], ["project/list"])
        self.assertEqual(rejected["project/list"].count, 2)
        self.assertEqual(rejected["thread/queue/list"].count, 1)

    def test_main_exit_code_reflects_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            logs = root / "Logs"
            logs.mkdir()
            (logs / "a.log").write_text(
                "unknown variant `thread/realtime/listVoices`", encoding="utf-8"
            )
            schema = root / "ClientRequest.json"
            schema.write_text(
                json.dumps(schema_with_methods("initialize")), encoding="utf-8"
            )
            self.assertEqual(
                drift.main(["--logs", str(logs), "--schema", str(schema), "--json"]), 1
            )
            schema.write_text(
                json.dumps(
                    schema_with_methods("initialize", "thread/realtime/listVoices")
                ),
                encoding="utf-8",
            )
            self.assertEqual(
                drift.main(["--logs", str(logs), "--schema", str(schema), "--json"]), 0
            )


if __name__ == "__main__":
    unittest.main()
