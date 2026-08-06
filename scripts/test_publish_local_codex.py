#!/usr/bin/env python3

import json
from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
RUN_TIMEOUT_SECONDS = 120
FIXTURE_TIME = 946684900
FRESH_SOURCE_TIME = FIXTURE_TIME + 10_000


def powershell() -> str | None:
    # Prefer Windows PowerShell 5.1: production invokes publish-local-codex.ps1
    # via `powershell -NoProfile -File ...` from the justfile, and 5.1 has
    # stricter native-stderr and StrictMode semantics than pwsh 7 — bugs in
    # that class are invisible when the tests run under pwsh.
    return shutil.which("powershell") or shutil.which("pwsh")


PUBLISH_ENV_VARS = (
    "CODEX_LOCAL_PUBLISH_DIR",
    "CODEX_LOCAL_CODEX_HOME",
    "CODEX_LOCAL_CODEX_SQLITE_HOME",
    "CODEX_HOME",
    "CODEX_SQLITE_HOME",
    "CODEX_CLI_PATH",
)


def clean_env() -> dict[str, str]:
    # A prior -ConfigureDesktopLocalCli publish persists these at User scope,
    # so the inherited environment can carry them; the script prefers
    # CODEX_LOCAL_PUBLISH_DIR over the test's temp USERPROFILE, which makes
    # assertions machine-state-dependent unless they are stripped.
    env = os.environ.copy()
    for name in PUBLISH_ENV_VARS:
        env.pop(name, None)
    return env


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


def publish_source_text() -> str:
    return SCRIPT.read_text(encoding="utf-8")


class PublishLocalCodexSourceLayoutTest(unittest.TestCase):
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
        self.assertIn("running-target process detection failed", publish_script)

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
