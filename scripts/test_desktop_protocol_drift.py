#!/usr/bin/env python3

import contextlib
import ctypes
import io
import json
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

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


def rejected_request(method: str) -> str:
    return (
        f"Invalid request: unknown variant `{method}`, "
        "expected one of `initialize`, `thread/start`"
    )


class DesktopProtocolDriftTest(unittest.TestCase):
    def test_just_recipe_uses_repository_schema_and_preserves_arguments(self) -> None:
        repo = Path(__file__).resolve().parents[1]
        with tempfile.TemporaryDirectory(prefix="desktop drift ") as temp_dir:
            root = Path(temp_dir)
            logs = root / "Desktop logs"
            logs.mkdir()
            log = logs / "desktop.log"
            log.write_text(rejected_request("initialize"), encoding="utf-8")
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

            log.write_text(rejected_request("audit/missingMethod"), encoding="utf-8")
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
                        rejected_request("thread/queue/list"),
                        rejected_request("project/list"),
                    ]
                ),
                encoding="utf-8",
            )
            scan = drift.rejected_methods([log])
            rejected = scan.rejected
            missing, present = drift.drift_report(
                rejected,
                drift.schema_methods(schema_with_methods("initialize", "project/list")),
            )

        self.assertEqual([entry.method for entry in missing], ["thread/queue/list"])
        self.assertEqual([entry.method for entry in present], ["project/list"])
        self.assertEqual(rejected["project/list"].count, 2)
        self.assertEqual(rejected["thread/queue/list"].count, 1)
        self.assertEqual(scan.unclassified, {})
        self.assertEqual(scan.errors, [])

    def test_main_exit_code_reflects_drift(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            logs = root / "Logs"
            logs.mkdir()
            (logs / "a.log").write_text(
                rejected_request("thread/realtime/listVoices"), encoding="utf-8"
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

    def test_no_recent_logs_is_missing_evidence_not_a_pass(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            logs = root / "Logs"
            logs.mkdir()
            stale = logs / "old.log"
            stale.write_text(rejected_request("audit/missingMethod"), encoding="utf-8")
            month_ago = time.time() - 30 * 86_400
            os.utime(stale, (month_ago, month_ago))
            schema = root / "ClientRequest.json"
            schema.write_text(
                json.dumps(schema_with_methods("initialize")), encoding="utf-8"
            )
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                code = drift.main(
                    ["--logs", str(logs), "--schema", str(schema), "--days", "7"]
                )

        self.assertEqual(code, 2)
        self.assertIn(
            "no Desktop logs modified in the last 7 day(s)", stderr.getvalue()
        )

    def test_parameter_and_truncated_rejections_are_not_missing_methods(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            log = root / "desktop.log"
            log.write_text(
                "\n".join(
                    [
                        'method=thread/start error="Invalid request: unknown variant '
                        '`newPolicy`, expected one of `never`, `on-request`"',
                        "unknown variant `truncated/method`",
                        rejected_request("real/missing"),
                    ]
                ),
                encoding="utf-8",
            )
            schema = root / "schema.json"
            schema.write_text(json.dumps(schema_with_methods("thread/start")))
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                code = drift.main(
                    ["--logs", str(root), "--schema", str(schema), "--json"]
                )
            report = json.loads(stdout.getvalue())
        self.assertEqual(code, 2)
        self.assertEqual([m["method"] for m in report["missing"]], ["real/missing"])
        self.assertEqual(
            [m["variant"] for m in report["unclassified_rejections"]],
            ["newPolicy", "truncated/method"],
        )
        self.assertEqual(report["scan_errors"], [])

    @unittest.skipUnless(os.name == "nt", "Windows exclusive file sharing")
    def test_locked_log_is_incomplete_and_preserves_other_findings(self):
        api = ctypes.WinDLL("kernel32", use_last_error=True)
        api.CreateFileW.argtypes = [
            ctypes.c_wchar_p,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_void_p,
            ctypes.c_uint32,
            ctypes.c_uint32,
            ctypes.c_void_p,
        ]
        api.CreateFileW.restype = ctypes.c_void_p
        api.CloseHandle.argtypes = [ctypes.c_void_p]
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            locked = root / "locked.log"
            locked.write_text(rejected_request("hidden/method"))
            (root / "readable.log").write_text(rejected_request("visible/method"))
            schema = root / "schema.json"
            schema.write_text(json.dumps(schema_with_methods("initialize")))
            handle = api.CreateFileW(str(locked), 0x80000000, 0, None, 3, 0x80, None)
            self.assertNotEqual(handle, ctypes.c_void_p(-1).value)
            try:
                with contextlib.redirect_stdout(io.StringIO()) as stdout:
                    code = drift.main(
                        ["--logs", str(root), "--schema", str(schema), "--json"]
                    )
            finally:
                api.CloseHandle(handle)
            report = json.loads(stdout.getvalue())
            self.assertEqual(code, 2)
            self.assertEqual(
                [m["method"] for m in report["missing"]], ["visible/method"]
            )
            self.assertEqual(len(report["scan_errors"]), 1)
            self.assertIn(str(locked), report["scan_errors"][0])
            with contextlib.redirect_stdout(io.StringIO()) as stdout:
                code = drift.main(
                    ["--logs", str(root), "--schema", str(schema), "--json"]
                )
            report = json.loads(stdout.getvalue())
            self.assertEqual(code, 1)
            self.assertEqual(
                [m["method"] for m in report["missing"]],
                ["hidden/method", "visible/method"],
            )
            self.assertEqual(report["scan_errors"], [])

    def test_discovery_failure_is_not_a_clean_scan(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            schema = root / "schema.json"
            schema.write_text(json.dumps(schema_with_methods("initialize")))
            with (
                mock.patch.object(
                    drift, "recent_log_files", side_effect=PermissionError("denied")
                ),
                contextlib.redirect_stderr(io.StringIO()) as stderr,
            ):
                code = drift.main(["--logs", str(root), "--schema", str(schema)])
            self.assertEqual(code, 2)
            self.assertIn("could not enumerate Desktop logs", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
