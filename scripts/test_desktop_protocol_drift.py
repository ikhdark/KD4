#!/usr/bin/env python3

import json
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
                        "unknown variant `project/list`, expected one of `initialize`\"",
                        "error [host] unknown variant `thread/queue/list`",
                        "error [host] unknown variant `project/list`",
                    ]
                ),
                encoding="utf-8",
            )
            rejected = drift.rejected_methods([log])
            missing, present = drift.drift_report(
                rejected, drift.schema_methods(schema_with_methods("initialize", "project/list"))
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
            schema.write_text(json.dumps(schema_with_methods("initialize")), encoding="utf-8")
            self.assertEqual(
                drift.main(["--logs", str(logs), "--schema", str(schema), "--json"]), 1
            )
            schema.write_text(
                json.dumps(schema_with_methods("initialize", "thread/realtime/listVoices")),
                encoding="utf-8",
            )
            self.assertEqual(
                drift.main(["--logs", str(logs), "--schema", str(schema), "--json"]), 0
            )


if __name__ == "__main__":
    unittest.main()
