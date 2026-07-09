#!/usr/bin/env python3

from pathlib import Path
import json
import os
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "run-powershell-script.ps1"
RUN_TIMEOUT_SECONDS = 30


def powershell() -> str | None:
    return shutil.which("pwsh") or shutil.which("powershell")


class RunPowerShellScriptTest(unittest.TestCase):
    def setUp(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        self.shell = shell

    def run_helper(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.shell, "-NoLogo", "-NoProfile", "-File", str(SCRIPT), *args],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
        )

    def test_helper_script_parses_as_powershell(self) -> None:
        command = """
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile($env:CODEX_TEST_SCRIPT, [ref]$tokens, [ref]$errors) | Out-Null
if ($errors.Count -ne 0) {
    $errors | ForEach-Object { Write-Error $_.Message }
    exit 1
}
"""
        result = subprocess.run(
            [self.shell, "-NoLogo", "-NoProfile", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            env={**os.environ, "CODEX_TEST_SCRIPT": str(SCRIPT)},
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )

    def test_script_body_runs_through_encoded_command(self) -> None:
        result = self.run_helper("-ScriptBody", "Write-Output 'body mode ok'")

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), "body mode ok")

    def test_script_body_forwards_remaining_arguments(self) -> None:
        result = self.run_helper(
            "-ScriptBody",
            "Write-Output ($args -join '|')",
            "first value",
            "second value",
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), "first value|second value")

    def test_script_file_runs_through_encoded_command(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script_file = Path(temp_dir) / "quoted script.ps1"
            script_file.write_text("Write-Output 'file mode ok'\n", encoding="utf-8")

            result = self.run_helper("-ScriptFile", str(script_file))

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), "file mode ok")

    def test_script_file_forwards_remaining_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script_file = Path(temp_dir) / "quoted script.ps1"
            script_file.write_text("Write-Output ($args -join '|')\n", encoding="utf-8")

            result = self.run_helper(
                "-ScriptFile",
                str(script_file),
                "first value",
                "second value",
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), "first value|second value")

    def test_script_file_preserves_file_execution_context(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            script_dir = Path(temp_dir)
            script_file = script_dir / "context script.ps1"
            script_file.write_text(
                """
[pscustomobject]@{
  PSScriptRoot = $PSScriptRoot
  PSCommandPath = $PSCommandPath
  MyCommandPath = $MyInvocation.MyCommand.Path
} | ConvertTo-Json -Compress
""".lstrip(),
                encoding="utf-8",
            )

            result = self.run_helper("-ScriptFile", str(script_file))

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        output = json.loads(result.stdout)
        self.assertEqual(Path(output["PSScriptRoot"]), script_dir)
        self.assertEqual(Path(output["PSCommandPath"]), script_file)
        self.assertEqual(Path(output["MyCommandPath"]), script_file)


if __name__ == "__main__":
    unittest.main()
