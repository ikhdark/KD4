#!/usr/bin/env python3

import ast
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from scripts.publish_local_codex_test_support import clean_env
from scripts.publish_local_codex_test_support import powershell
from scripts.publish_local_codex_test_support import ps_single_quote


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


def publish_source_text() -> str:
    return SCRIPT.read_text(encoding="utf-8")


class PublishLocalCodexSourceLayoutTest(unittest.TestCase):
    def test_publish_test_helpers_are_shared_by_sibling_suites(self) -> None:
        helper_names = {"clean_env", "powershell", "ps_single_quote"}
        scripts_dir = Path(__file__).resolve().parent

        for filename in (
            "test_publish_local_codex.py",
            "test_publish_local_codex_apply.py",
            "test_publish_local_codex_build.py",
            "test_publish_local_codex_dry_run.py",
        ):
            module = ast.parse((scripts_dir / filename).read_text(encoding="utf-8"))
            local_helpers = {
                node.name
                for node in module.body
                if isinstance(node, ast.FunctionDef) and node.name in helper_names
            }
            self.assertEqual(local_helpers, set(), filename)

    def test_publish_implementation_is_consolidated_in_entrypoint(self) -> None:
        entrypoint = SCRIPT.read_text(encoding="utf-8")

        for helper_name in (
            "publish-local-codex.hashing.ps1",
            "publish-local-codex.proof.ps1",
            "publish-local-codex.desktop.ps1",
            "publish-local-codex.build.ps1",
            "publish-local-codex.apply.ps1",
        ):
            self.assertNotIn(helper_name, entrypoint)
        for function_name in (
            "Get-RepoRoot",
            "Get-RunningCodexTargetProcesses",
            "Set-ProcessEnvironmentVariable",
            "Publish-CodexBinary",
        ):
            self.assertEqual(entrypoint.count(f"function {function_name}"), 1)

    def test_hashing_and_metadata_cache_are_in_entrypoint(self) -> None:
        publish_script = publish_source_text()

        self.assertIn("function Get-FileSha256", publish_script)
        self.assertIn("LocalPublishContentHashCache", publish_script)
        self.assertIn("Get-CachedLocalPublishFileSha256", publish_script)
        self.assertIn("LastWriteTimeUtcTicks", publish_script)
        self.assertIn("$before.Length -ne $after.Length", publish_script)

    def test_build_input_snapshot_reuses_one_inventory_for_hash_and_newest_time(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as temp_dir:
            repo = Path(temp_dir) / "repo"
            repo.mkdir()
            first = repo / "first.txt"
            second = repo / "second.txt"
            first.write_text("first", encoding="utf-8")
            second.write_text("second", encoding="utf-8")
            for args in (
                ("init", "--quiet"),
                ("add", "first.txt", "second.txt"),
                (
                    "-c",
                    "user.name=Codex Test",
                    "-c",
                    "user.email=codex@example.com",
                    "commit",
                    "--quiet",
                    "-m",
                    "fixture",
                ),
            ):
                result = subprocess.run(
                    ["git", "-C", str(repo), *args],
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=RUN_TIMEOUT_SECONDS,
                )
                self.assertEqual(result.returncode, 0, result.stderr)

            command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile({ps_single_quote(SCRIPT)}, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) {{ throw "Failed to parse publish script: $($errors[0].Message)" }}
$functionAst = $ast.FindAll({{
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Get-LocalPublishBuildInputSnapshot'
}}, $true)
if (@($functionAst).Count -ne 1) {{ throw 'Snapshot function was not found exactly once.' }}
Invoke-Expression $functionAst[0].Extent.Text
$script:listCalls = 0
function Invoke-GitNulDelimitedList {{
    param([string]$GitPath, [string]$RepoRoot, [string[]]$Arguments)
    $script:listCalls++
    return [pscustomobject]@{{ ExitCode = 0; Records = @('first.txt', 'second.txt') }}
}}
function Test-LocalPublishBuildRelevantPath {{ param([string]$Path) return $true }}
function Get-CachedLocalPublishFileSha256 {{
    param([string]$Path, [switch]$ForceRefresh)
    return ('a' * 64)
}}
function Test-Sha256Text {{
    param([AllowNull()][object]$Value)
    return $null -ne $Value -and ([string]$Value) -cmatch '\A[0-9a-f]{{64}}\z'
}}
[IO.File]::SetLastWriteTimeUtc({ps_single_quote(first)}, [DateTime]::Parse('2000-01-01T00:00:00Z').ToUniversalTime())
[IO.File]::SetLastWriteTimeUtc({ps_single_quote(second)}, [DateTime]::Parse('2000-01-02T00:00:00Z').ToUniversalTime())
$snapshot = Get-LocalPublishBuildInputSnapshot -RepoRoot {ps_single_quote(repo)}
[pscustomobject]@{{
    listCalls = $script:listCalls
    fingerprint = $snapshot.Fingerprint
    newestMatches = $snapshot.NewestWriteUtc -eq [IO.File]::GetLastWriteTimeUtc({ps_single_quote(second)})
}} | ConvertTo-Json -Compress
"""
            result = subprocess.run(
                [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
                cwd=SCRIPT.parent.parent,
                capture_output=True,
                text=True,
                timeout=RUN_TIMEOUT_SECONDS,
                check=False,
                creationflags=CREATE_NO_WINDOW,
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            output = json.loads(result.stdout)
            self.assertEqual(output["listCalls"], 1)
            self.assertEqual(len(output["fingerprint"]), 64)
            self.assertTrue(output["newestMatches"])

    def test_publish_binary_proof_force_refreshes_cached_source_hashes(self) -> None:
        publish_script = publish_source_text()

        self.assertIn('$sourceSha256Mode = "hashed"', publish_script)
        self.assertIn(
            "$sourceSha256 = Get-CachedLocalPublishFileSha256 "
            "-Path $SourceExe -ForceRefresh",
            publish_script,
        )
        self.assertIn(
            "$sourceCodeModeHostSha256 = Get-CachedLocalPublishFileSha256 "
            "-Path $SourceCodeModeHostExe -ForceRefresh",
            publish_script,
        )
        self.assertIn(
            "$targetBeforeSha256 = Get-FileSha256 $targetPath", publish_script
        )
        self.assertIn(
            "$codeModeHostTargetBeforeSha256 = Get-FileSha256 $codeModeHostTargetPath",
            publish_script,
        )
        self.assertIn("$sourceSha256,\n            $targetSha256", publish_script)
        self.assertIn(
            'Write-ProofLine "codexPostPublishVerify" "sha256 ok"', publish_script
        )
        self.assertIn(
            "running-target process detection was indeterminate", publish_script
        )
        self.assertIn(
            "$script:RunningTargetProcessProbeWarnings.Count -gt 0", publish_script
        )
        self.assertIn("StartTimeUtcTicks", publish_script)
        self.assertIn("Stop-Process -InputObject $process", publish_script)
        self.assertIn(
            "Get-CachedLocalPublishFileSha256 -Path $path -ForceRefresh",
            publish_script,
        )
        self.assertIn('"ls-files", "-z"', publish_script)
        self.assertIn('"status", "--porcelain=v1", "-z"', publish_script)

    def test_process_revalidation_ignores_a_process_that_exited_before_path_read(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile('{SCRIPT}', [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) {{
    throw "Failed to parse publish script: $($errors[0].Message)"
}}
$functionAst = $ast.FindAll({{
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        $node.Name -eq 'Get-LiveProcessesById'
}}, $true)
if (@($functionAst).Count -ne 1) {{
    throw 'Get-LiveProcessesById was not found exactly once.'
}}
Invoke-Expression $functionAst[0].Extent.Text
$script:disposed = $false
function Get-Process {{
    [CmdletBinding()]
    param([int]$Id)
    $fake = [pscustomobject]@{{
        Id = $Id
        HasExited = $true
        Path = $null
        StartTime = [DateTime]::UtcNow
    }}
    $fake | Add-Member -MemberType ScriptMethod -Name Dispose -Value {{
        $script:disposed = $true
    }}
    return $fake
}}
$candidate = [pscustomobject]@{{
    Id = 29448
    Path = 'C:\\valid\\codex.exe'
    StartTimeUtcTicks = 1
}}
$live = @(Get-LiveProcessesById -Processes @($candidate))
if ($live.Count -ne 0) {{
    throw "Expected an exited process to be ignored; found $($live.Count)."
}}
if (-not $script:disposed) {{
    throw 'Expected the exited process handle to be disposed.'
}}
"ok"
"""
        result = subprocess.run(
            [
                shell,
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                command,
            ],
            cwd=SCRIPT.parent.parent,
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertIn("ok", result.stdout)

    def test_user_path_edits_preserve_expandable_registry_values(self) -> None:
        publish_script = publish_source_text()

        self.assertIn("DoNotExpandEnvironmentNames", publish_script)
        self.assertIn("RegistryValueKind]::ExpandString", publish_script)
        self.assertIn("ExpandEnvironmentVariables", publish_script)
        self.assertIn("foreach ($process in $candidates)", publish_script)
        self.assertIn(
            "post-close running-target process probe failed",
            SCRIPT.read_text(encoding="utf-8"),
        )

    def test_publish_build_includes_complete_windows_runtime_bundle(self) -> None:
        publish_script = publish_source_text()

        self.assertIn(
            '$publishPackages = @("-p", "codex-cli", "-p", "codex-code-mode-host", "-p", "codex-windows-sandbox")',
            publish_script,
        )
        self.assertIn("Get-BuiltCodeModeHostPath", publish_script)
        self.assertIn("Get-BuiltWindowsSandboxSetupPath", publish_script)
        self.assertIn("Get-BuiltCommandRunnerPath", publish_script)
        self.assertIn(
            'Join-Path $InstallDir "codex-code-mode-host.exe"', publish_script
        )
        self.assertIn('Join-Path $InstallDir "codex-resources"', publish_script)
        self.assertIn(
            'Join-Path $sandboxResourcesDir "codex-windows-sandbox-setup.exe"',
            publish_script,
        )
        self.assertIn(
            'Join-Path $sandboxResourcesDir "codex-command-runner.exe"',
            publish_script,
        )

    def test_publish_script_uses_global_publish_mutex(self) -> None:
        publish_script = publish_source_text()

        self.assertIn('"Global\\CodexLocalPublish"', publish_script)
        self.assertIn(".WaitOne([TimeSpan]::FromSeconds(30))", publish_script)
        self.assertIn(".ReleaseMutex()", publish_script)

    def test_publish_build_calls_shared_msvc_linker_setup(self) -> None:
        publish_script = publish_source_text()

        self.assertIn(
            '. (Join-Path $PSScriptRoot "common-rust-env.ps1")',
            publish_script,
        )
        self.assertIn("Set-CodexRustMsvcLinkerEnvironment", publish_script)

    def test_just_exposes_only_final_publish_recipe(self) -> None:
        justfile = (SCRIPT.parent.parent / "justfile").read_text(encoding="utf-8")

        self.assertEqual(justfile.count("publish-local-codex-final *args:"), 2)
        self.assertIn(
            "-AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertNotIn("-SkipPreflightCheck", justfile)
        self.assertIn(
            "-ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User", justfile
        )
        for recipe in (
            "publish-local-codex",
            "publish-local-codex-dry-run",
            "publish-local-codex-final-dry-run",
            "publish-local-codex-runtime-proof",
            "publish-local-codex-final-test-run",
            "publish-local-codex-build-only",
            "validate-local-publish",
        ):
            self.assertNotIn(f"{recipe} *args:", justfile)

    def test_default_local_publish_target_is_not_openai_appdata_bin(self) -> None:
        publish_script = publish_source_text()

        self.assertIn('Join-Path $env:USERPROFILE "Desktop\\LOCAL-KD"', publish_script)
        self.assertNotIn(
            'Join-Path $env:LOCALAPPDATA "OpenAI\\Codex\\bin\\codexKD-local"',
            publish_script,
        )

    def test_publish_doctor_allows_only_missing_auth_failure(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile('{SCRIPT}', [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) {{
    throw "Failed to parse publish script: $($errors[0].Message)"
}}
$functions = $ast.FindAll({{
    param($node)
    $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
        ($node.Name -eq 'ConvertFrom-DoctorOutput' -or
            $node.Name -eq 'Test-DoctorFailureAllowedForPublish')
}}, $true)
if (@($functions).Count -ne 2) {{
    throw 'Doctor publish classifier functions were not found.'
}}
foreach ($function in $functions) {{
    Invoke-Expression $function.Extent.Text
}}
$authOnly = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"network.websocket_reachability":{{"status":"warning"}}}}}}'
$configFailure = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"config.load":{{"status":"fail"}}}}}}'
[pscustomobject]@{{
    authOnly = Test-DoctorFailureAllowedForPublish -OutputLines @($authOnly)
    configFailure = Test-DoctorFailureAllowedForPublish -OutputLines @($configFailure)
}} | ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
        )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        output = json.loads(result.stdout)
        self.assertTrue(output["authOnly"])
        self.assertFalse(output["configFailure"])

    def test_publish_run_doctor_uses_publish_classifier(self) -> None:
        publish_script = publish_source_text()

        self.assertIn("function Invoke-DoctorForPublish", publish_script)
        self.assertIn("warning: auth.credentials missing", publish_script)
        self.assertEqual(
            publish_script.count("Invoke-DoctorForPublish -TargetPath $targetPath"),
            3,
        )

    def test_local_release_profile_is_minimal_release_inheritance(self) -> None:
        cargo_toml = (SCRIPT.parent.parent / "codex-rs" / "Cargo.toml").read_text(
            encoding="utf-8"
        )

        self.assertIn(
            '[profile.local-release]\ninherits = "release"\nlto = false',
            cargo_toml,
        )
        local_release_block = cargo_toml.split("[profile.local-release]", 1)[1].split(
            "[profile.",
            1,
        )[0]
        self.assertNotIn("incremental", local_release_block)
        self.assertNotIn("codegen-units", local_release_block)
        self.assertNotIn("debug", local_release_block)
        self.assertNotIn("strip", local_release_block)


class PublishLocalCodexHelperBehaviorTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.shell = powershell()
        if cls.shell is None:
            raise unittest.SkipTest("PowerShell is not available")

    def test_version_probe_drains_large_stderr_without_deadlock(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            helper_exe = Path(temp_dir) / "noisy-version.exe"
            source = (
                "using System; public static class Program { "
                "public static int Main(string[] args) { "
                "Console.Error.Write(new string('x', 65536)); "
                'Console.Out.WriteLine("codex noisy 1.0"); return 0; } }'
            )
            command = (
                f". {ps_single_quote(SCRIPT)} -ImportOnly; "
                f"Add-Type -TypeDefinition {ps_single_quote(source)} "
                f"-OutputAssembly {ps_single_quote(helper_exe)} "
                "-OutputType ConsoleApplication; "
                f"$lines = @(Get-VersionProofLines -Path {ps_single_quote(helper_exe)} "
                "-TimeoutMilliseconds 5000); Write-Output $lines[0]"
            )
            result = subprocess.run(
                [
                    self.shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                timeout=30,
                creationflags=CREATE_NO_WINDOW,
                env=clean_env(),
            )

            self.assertEqual(
                result.returncode,
                0,
                f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
            )
            self.assertEqual(result.stdout.strip(), "codex noisy 1.0")


if __name__ == "__main__":
    unittest.main()
