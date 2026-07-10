#!/usr/bin/env python3

import json
from pathlib import Path
import hashlib
import os
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "publish-local-codex.ps1"
HASHING_HELPER = Path(__file__).resolve().parent / "publish-local-codex.hashing.ps1"
PUBLISH_HELPERS = (
    Path(__file__).resolve().parent / "publish-local-codex.proof.ps1",
    Path(__file__).resolve().parent / "publish-local-codex.desktop.ps1",
    Path(__file__).resolve().parent / "publish-local-codex.build.ps1",
    Path(__file__).resolve().parent / "publish-local-codex.apply.ps1",
)
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


def ps_single_quote(value: str | Path) -> str:
    return "'" + str(value).replace("'", "''") + "'"


PUBLISH_ENV_VARS = (
    "CODEX_LOCAL_PUBLISH_DIR",
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


def publish_source_text() -> str:
    return "\n".join(
        [
            SCRIPT.read_text(encoding="utf-8"),
            *(path.read_text(encoding="utf-8") for path in PUBLISH_HELPERS),
        ]
    )


class PublishLocalCodexSourceLayoutTest(unittest.TestCase):
    def test_publish_helpers_are_dot_sourced_once(self) -> None:
        entrypoint = SCRIPT.read_text(encoding="utf-8")
        composed = publish_source_text()

        for helper in PUBLISH_HELPERS:
            self.assertEqual(entrypoint.count(f'"{helper.name}"'), 1)
        for function_name in (
            "Get-RepoRoot",
            "Get-RunningCodexTargetProcesses",
            "Set-ProcessEnvironmentVariable",
            "Publish-CodexBinary",
        ):
            self.assertEqual(composed.count(f"function {function_name}"), 1)

    def test_hashing_helper_is_dot_sourced_without_duplicate_definitions(self) -> None:
        publish_script = publish_source_text()
        hashing_helper = HASHING_HELPER.read_text(encoding="utf-8")

        self.assertIn(
            '. (Join-Path $PSScriptRoot "publish-local-codex.hashing.ps1")',
            publish_script,
        )
        self.assertIn("function Get-FileSha256Cached", hashing_helper)
        self.assertNotIn("function Get-FileSha256Cached", publish_script)
        self.assertNotIn("function Get-CachedFileSha256", publish_script)
        self.assertNotIn("function Remove-StaleFileSha256CacheEntries", publish_script)

    def test_publish_binary_proof_uses_direct_hashing_not_metadata_cache(self) -> None:
        publish_script = publish_source_text()

        self.assertIn('$sourceSha256Mode = "hashed"', publish_script)
        self.assertIn("$sourceSha256 = Get-FileSha256 $SourceExe", publish_script)
        self.assertIn(
            "$sourceCodeModeHostSha256 = Get-FileSha256 $SourceCodeModeHostExe",
            publish_script,
        )
        self.assertIn(
            "$targetBeforeSha256 = Get-FileSha256 $targetPath", publish_script
        )
        self.assertIn(
            "$codeModeHostTargetBeforeSha256 = Get-FileSha256 $codeModeHostTargetPath",
            publish_script,
        )
        self.assertNotIn("Get-FileSha256Cached $SourceExe", publish_script)
        self.assertNotIn("Get-FileSha256Cached $targetPath", publish_script)
        self.assertIn("$sourceSha256,\n            $targetSha256", publish_script)
        self.assertIn(
            'Write-ProofLine "codexPostPublishVerify" "sha256 ok"', publish_script
        )
        self.assertIn("running-target process detection failed", publish_script)

    def test_publish_build_includes_cli_and_code_mode_host_packages(self) -> None:
        publish_script = publish_source_text()

        self.assertIn(
            '$publishPackages = @("-p", "codex-cli", "-p", "codex-code-mode-host")',
            publish_script,
        )
        self.assertIn("Get-BuiltCodeModeHostPath", publish_script)
        self.assertIn(
            'Join-Path $InstallDir "codex-code-mode-host.exe"', publish_script
        )

    @unittest.skipUnless(os.name == "nt", "PowerShell helper is Windows-only")
    def test_hashing_helper_initializes_cache_state_when_dot_sourced_directly(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = Path(temp_dir)
            source = temp_path / "codex.exe"
            cache_dir = temp_path / "cache"
            source.write_bytes(b"codex")
            expected = hashlib.sha256(source.read_bytes()).hexdigest()
            cache_dir.mkdir()
            legacy_cache = cache_dir / "legacy-path-key.sha256.json"
            legacy_cache.write_text(
                json.dumps({"path": str(source.resolve())}), encoding="utf-8"
            )

            result = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    (
                        # Mirror the production session: the publish script
                        # dot-sources this helper under StrictMode Latest.
                        "Set-StrictMode -Version Latest; "
                        "$ErrorActionPreference = 'Stop'; "
                        f". {ps_single_quote(HASHING_HELPER)}; "
                        f"Get-FileSha256Cached -Path {ps_single_quote(source)} "
                        f"-CacheDir {ps_single_quote(cache_dir)}"
                    ),
                ],
                text=True,
                capture_output=True,
                check=False,
                creationflags=CREATE_NO_WINDOW,
            )
            cache_names = sorted(path.name for path in cache_dir.iterdir())
            expected_cache_name = (
                hashlib.sha256(str(source.resolve()).encode("utf-8")).hexdigest()
                + ".sha256.json"
            )

        self.assertEqual(
            result.returncode,
            0,
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}",
        )
        self.assertEqual(result.stdout.strip(), expected)
        self.assertEqual(cache_names, [expected_cache_name])
        self.assertEqual(len(expected_cache_name), 76)

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

    def test_just_publish_recipes_configure_desktop_local_cli(self) -> None:
        justfile = (SCRIPT.parent.parent / "justfile").read_text(encoding="utf-8")

        self.assertIn(
            'publish-local-codex.ps1" -DryRun -ConfigureDesktopLocalCli', justfile
        )
        self.assertIn(
            'publish-local-codex.ps1" -DryRun -SkipBuild -FailOnStaleSourceBuild -ConfigureDesktopLocalCli',
            justfile,
        )
        self.assertIn(
            'publish-local-codex.ps1" -AutoSkipBuild -ConfigureDesktopLocalCli',
            justfile,
        )
        self.assertIn("publish-local-codex-final *args:", justfile)
        self.assertIn(
            "-AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-final-dry-run *args:", justfile)
        self.assertIn(
            "-DryRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-runtime-proof *args:", justfile)
        self.assertIn(
            "-DryRun -SkipBuild -RunDoctor -RuntimeProof -FailOnStaleSourceBuild",
            justfile,
        )
        self.assertIn("publish-local-codex-final-test-run *args:", justfile)
        self.assertIn(
            "-TestRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30",
            justfile,
        )
        self.assertIn("publish-local-codex-build-only *args:", justfile)
        self.assertIn('publish-local-codex.ps1" -BuildOnly', justfile)
        self.assertIn(
            "-ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User", justfile
        )

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
$ast = [System.Management.Automation.Language.Parser]::ParseFile('{PUBLISH_HELPERS[0]}', [ref]$tokens, [ref]$errors)
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


if __name__ == "__main__":
    unittest.main()
