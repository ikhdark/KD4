#!/usr/bin/env python3

import hashlib
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
    def test_desktop_executable_comes_from_package_manifest(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        with tempfile.TemporaryDirectory() as temp_dir:
            fixture = Path(temp_dir)
            app_dir = fixture / "app"
            app_dir.mkdir()
            (app_dir / "ChatGPT.exe").touch()
            (fixture / "AppxManifest.xml").write_text(
                """<?xml version="1.0" encoding="utf-8"?>
<Package>
  <Applications>
    <Application Id="App" Executable="app/ChatGPT.exe" />
  </Applications>
</Package>
""",
                encoding="utf-8",
            )
            command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-CodexDesktopPackage {{
    return [pscustomobject]@{{ InstallLocation = {ps_single_quote(fixture)} }}
}}
Get-CodexDesktopExecutableProof
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
                env=clean_env(),
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            Path(result.stdout.strip()),
            fixture / "app" / "ChatGPT.exe",
        )

    def test_desktop_process_lookup_uses_executable_name(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-Process {{
    [CmdletBinding()]
    param([string]$Name)
    if ($Name -ne 'ChatGPT') {{ throw "unexpected process name: $Name" }}
    return @(
        [pscustomobject]@{{ Id = 41; Path = 'C:\fixture\ChatGPT.exe' }},
        [pscustomobject]@{{ Id = 42; Path = 'C:\other\ChatGPT.exe' }}
    )
}}
@(Get-CodexDesktopProcessesForPath -DesktopPath 'C:\fixture\ChatGPT.exe') |
    Select-Object Id, Path |
    ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            env=clean_env(),
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"Id": 41, "Path": r"C:\fixture\ChatGPT.exe"},
        )

    def test_restart_waits_for_the_original_process_identity(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        fixture_home = self.enterContext(tempfile.TemporaryDirectory())
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
$script:Reads = 0
$script:Stopped = 0
$script:Launched = $false
$script:Start = [DateTime]::UtcNow
function Get-CodexDesktopExecutableProof {{ return 'C:\fixture\Desktop.exe' }}
function Get-CodexDesktopProcessesForPath {{
    return @([pscustomobject]@{{ Id = 91; Path = 'C:\fixture\Desktop.exe'; StartTime = $script:Start }})
}}
function Get-Process {{
    param([int]$Id)
    $script:Reads++
    $process = [pscustomobject]@{{ Id = $Id; Path = 'C:\fixture\Desktop.exe'; StartTime = $script:Start; HasExited = ($script:Reads -ge 3) }}
    $process | Add-Member -MemberType ScriptMethod -Name Dispose -Value {{}}
    return $process
}}
function Stop-Process {{
    param([object]$InputObject)
    if ($InputObject.Id -ne 91) {{ throw 'wrong process stopped' }}
    $script:Stopped++
}}
function Start-Process {{
    if ($script:Reads -lt 3) {{ throw 'activated before original process exited' }}
    $script:Launched = $true
}}
function Test-DesktopRuntimeProof {{ return $true }}
Restart-CodexDesktop -LocalCliPath 'C:\fixture\codex.exe' -LocalCodexHome {ps_single_quote(fixture_home)} -LocalCodexSqliteHome {ps_single_quote(fixture_home)}
[pscustomobject]@{{ Reads = $script:Reads; Stopped = $script:Stopped; Launched = $script:Launched }} | ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            env=clean_env(),
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout.splitlines()[-1]),
            {"Reads": 3, "Stopped": 1, "Launched": True},
        )

    def test_main_routing_preserves_already_current_values(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        with tempfile.TemporaryDirectory() as temp:
            command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
$ConfigureDesktopLocalCli = $true
$skipBuildBlockedByStaleSource = $false
$DryRun = $false
$DesktopCliEnvironmentTarget = 'Process'
$InstallDir = {ps_single_quote(Path(temp) / "bin")}
$targetPath = Join-Path $InstallDir 'codex.exe'
$LocalCodexHome = {ps_single_quote(temp)}
$LocalCodexSqliteHome = {ps_single_quote(temp)}
foreach ($entry in @(@('CODEX_CLI_PATH', $targetPath), @('CODEX_HOME', $LocalCodexHome), @('CODEX_SQLITE_HOME', $LocalCodexSqliteHome))) {{
    [Environment]::SetEnvironmentVariable($entry[0], $entry[1], 'Process')
}}
function Sync-OfficialDesktopEnvironmentCleanup {{ throw 'main must not clear routing before comparing it' }}
function Set-EnvironmentVariableForTarget {{ throw 'unchanged routing must not be rewritten' }}
function Send-EnvironmentChangedBroadcast {{ throw 'unchanged routing must not broadcast' }}
$source = Get-Content -LiteralPath {ps_single_quote(SCRIPT)} -Raw
$start = $source.IndexOf('$desktopRoutingResult = [pscustomobject]')
$end = $source.IndexOf('if ($DryRun) {{', $start)
. ([scriptblock]::Create($source.Substring($start, $end - $start)))
$desktopRoutingResult | ConvertTo-Json -Compress
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
                env=clean_env(),
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
                creationflags=CREATE_NO_WINDOW,
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout.splitlines()[-1]),
            {"Changed": False, "RestartRequired": False},
        )

    def test_backup_pruning_rejects_unmarked_directory(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        with tempfile.TemporaryDirectory() as temp_dir:
            backup = Path(temp_dir) / "codex-20000101T000000000Z.exe"
            backup.write_bytes(b"unrelated")
            command = r"""
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile(%s,[ref]$tokens,[ref]$errors)
$fn=$ast.FindAll({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Remove-OldCodexBackups'},$true)[0]
Invoke-Expression $fn.Extent.Text
try { Remove-OldCodexBackups -BackupDir %s -Keep 0; exit 9 } catch { if ($_.Exception.Message -notlike 'Refusing to prune an unmarked backup directory:*') { throw } }
if (-not (Test-Path -LiteralPath %s -PathType Leaf)) { exit 10 }
""" % (
                ps_single_quote(SCRIPT),
                ps_single_quote(Path(temp_dir)),
                ps_single_quote(backup),
            )
            completed = subprocess.run(
                [
                    shell,
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    command,
                ],
                text=True,
                capture_output=True,
                check=False,
                timeout=RUN_TIMEOUT_SECONDS,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_audit_publish_noop_routing_change_requires_doctor(self) -> None:
        publish_script = publish_source_text()
        noop_branch = publish_script.split("if (-not $binaryChanged) {", 1)[1].split(
            "$publishedCodeModeHost = $false", 1
        )[0]

        self.assertIn(
            "if ($DoctorOnNoop -or $desktopRoutingResult.Changed)", noop_branch
        )
        self.assertIn("Invoke-DoctorForPublish -TargetPath $targetPath", noop_branch)

    def test_cached_hash_reuses_verified_observation_without_nested_file_hash(
        self,
    ) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        with tempfile.TemporaryDirectory() as temp_dir:
            payload = Path(temp_dir) / "payload with spaces.bin"
            payload.write_bytes("hÃ©llo".encode())
            command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile({ps_single_quote(SCRIPT)}, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0) {{ throw $errors[0].Message }}
foreach ($name in @('Get-VerifiedFileHashObservation', 'Get-CachedLocalPublishFileSha256')) {{
    $functionAst = @($ast.FindAll({{
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and
            $node.Name -eq $name
    }}, $true))
    if ($functionAst.Count -ne 1) {{ throw "function $name was not found exactly once" }}
    Invoke-Expression $functionAst[0].Extent.Text
}}
$script:LocalPublishContentHashCache = @{{}}
function Test-Sha256Text {{ param($Value) return ([string]$Value) -cmatch '\A[0-9a-f]{{64}}\z' }}
function Get-FileSha256 {{ throw 'nested hash helper must not be called' }}
$script:hashObservations = 0
$script:realObservation = (Get-Command Get-VerifiedFileHashObservation).ScriptBlock
function Get-VerifiedFileHashObservation {{
    param($Path, $Before)
    $script:hashObservations++
    & $script:realObservation -Path $Path -Before $Before
}}
$first = Get-CachedLocalPublishFileSha256 -Path {ps_single_quote(payload)}
$second = Get-CachedLocalPublishFileSha256 -Path {ps_single_quote(payload)}
[pscustomobject]@{{ first = $first; second = $second; observations = $script:hashObservations }} | ConvertTo-Json -Compress
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
                capture_output=True,
                text=True,
                timeout=RUN_TIMEOUT_SECONDS,
                check=False,
                creationflags=CREATE_NO_WINDOW,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        expected = hashlib.sha256("hÃ©llo".encode()).hexdigest()
        self.assertEqual(
            output, {"first": expected, "second": expected, "observations": 1}
        )

    def test_shutdown_waits_on_verified_handles_without_polling(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
$ErrorActionPreference = 'Stop'
. {ps_single_quote(SCRIPT)} -ImportOnly
$script:events = [Collections.Generic.List[string]]::new()
$script:exited = $false
function Write-ProofLine {{}}
function Format-ProcessProof {{ return 'test process' }}
function Stop-Process {{ throw 'gracefully exited process must not be killed' }}
function Get-LiveProcessesById {{
    if ($script:exited) {{ return }}
    $process = [pscustomobject]@{{ MainWindowHandle = [IntPtr]1 }}
    $process | Add-Member ScriptMethod CloseMainWindow {{ $script:events.Add('close'); return $true }}
    $process | Add-Member ScriptMethod WaitForExit {{
        param($milliseconds)
        $script:events.Add("wait:$($milliseconds -ge 0 -and $milliseconds -le 1000)")
        $script:exited = $true
        return $true
    }}
    $process | Add-Member ScriptMethod Dispose {{ $script:events.Add('dispose') }}
    return $process
}}
Stop-RunningCodexTargetProcesses -Processes @([pscustomobject]@{{ Id = 123 }}) -TimeoutSeconds 1
ConvertTo-Json -InputObject @($script:events) -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), ["close", "wait:True", "dispose"])

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
$ast = [System.Management.Automation.Language.Parser]::ParseFile({ps_single_quote(SCRIPT)}, [ref]$tokens, [ref]$errors)
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

    def test_publish_mutex_excludes_contenders_and_releases(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
$ErrorActionPreference = 'Stop'
. {ps_single_quote(SCRIPT)} -ImportOnly
# Isolate the name while exercising the actual acquisition/release functions.
$script:mutexName = 'Local\CodexPublishTest-' + [Guid]::NewGuid().ToString('N')
$definition = (Get-Command Enter-CodexLocalPublishMutex).ScriptBlock.ToString()
if (-not $definition.Contains('Global\CodexLocalPublish')) {{ throw 'missing publisher mutex name' }}
Set-Item Function:Enter-CodexLocalPublishMutex ([ScriptBlock]::Create($definition.Replace('Global\CodexLocalPublish', $script:mutexName)))
Add-Type -TypeDefinition @'
using System;
using System.Threading;
public static class MutexContender {{
    public static bool TryAcquire(string name) {{
        bool acquired = false;
        var thread = new Thread(() => {{
            using (var mutex = new Mutex(false, name)) {{
                acquired = mutex.WaitOne(0);
                if (acquired) mutex.ReleaseMutex();
            }}
        }});
        thread.IsBackground = true;
        thread.Start();
        if (!thread.Join(5000)) throw new TimeoutException("mutex contender hung");
        return acquired;
    }}
}}
'@
$lock = Enter-CodexLocalPublishMutex
try {{ $blocked = -not [MutexContender]::TryAcquire($script:mutexName) }}
finally {{ Exit-CodexLocalPublishMutex -Lock $lock }}
$released = [MutexContender]::TryAcquire($script:mutexName)
[pscustomobject]@{{ blocked = $blocked; released = $released }} | ConvertTo-Json -Compress
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
            check=False,
            creationflags=CREATE_NO_WINDOW,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"blocked": True, "released": True})

    def test_publish_build_calls_shared_msvc_linker_setup(self) -> None:
        publish_script = publish_source_text()

        self.assertIn(
            '. (Join-Path $PSScriptRoot "common-rust-env.ps1")',
            publish_script,
        )
        self.assertIn("Set-CodexRustMsvcLinkerEnvironment", publish_script)

    def test_final_publish_dry_run_only_plans_local_publish(
        self,
    ) -> None:
        result = subprocess.run(
            [
                "just",
                "--justfile",
                str(SCRIPT.parent.parent / "justfile"),
                "--dry-run",
                "publish-local-codex-final",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = result.stdout + result.stderr
        self.assertEqual(len(commands.strip().splitlines()), 1, commands)
        self.assertNotIn("test-release-tooling", commands)
        self.assertNotIn("-RunDoctor", commands)
        self.assertNotIn("-DoctorOnNoop", commands)
        self.assertIn("publish-local-codex.ps1", commands)
        for argument in (
            "-AutoSkipBuild",
            "-Profile local-release",
            "-Concise",
            "-CloseRunningTargetTimeoutSeconds 30",
            "-ConfigureDesktopLocalCli",
            "-DesktopCliEnvironmentTarget User",
            "-RestartDesktop",
        ):
            with self.subTest(argument=argument):
                self.assertIn(argument, commands)
        self.assertNotIn("-SkipPreflightCheck", commands)

    def test_requested_restart_rejects_unavailable_desktop(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-CodexDesktopExecutableProof {{ return '<missing>' }}
try {{
    Restart-CodexDesktop
    throw 'expected unavailable Desktop restart to fail'
}}
catch {{
    if ($_.Exception.Message -eq 'expected unavailable Desktop restart to fail') {{
        throw
    }}
    $_.Exception.Message
}}
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
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
            env=clean_env(),
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Codex Desktop is unavailable", result.stdout)

    def test_restart_activates_packaged_desktop_and_restores_process_env(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-CodexDesktopExecutableProof {{ return 'C:\Program Files\WindowsApps\OpenAI.Codex\app\Codex.exe' }}
$script:Launched = $false
function Get-CodexDesktopProcessesForPath {{
    param([string]$DesktopPath)
    if ($script:Launched) {{
        return @([pscustomobject]@{{ Id = 91; Path = $DesktopPath }})
    }}
    return @()
}}
function Test-DesktopRuntimeProof {{ return $true }}
function Start-Process {{
    param([string]$FilePath, [object[]]$ArgumentList)
    $script:Launched = $true
    $script:Launch = [pscustomobject]@{{
        FilePath = $FilePath
        Argument = [string]$ArgumentList[0]
        CliPath = [Environment]::GetEnvironmentVariable('CODEX_CLI_PATH', 'Process')
        CodexHome = [Environment]::GetEnvironmentVariable('CODEX_HOME', 'Process')
        SqliteHome = [Environment]::GetEnvironmentVariable('CODEX_SQLITE_HOME', 'Process')
    }}
}}
[Environment]::SetEnvironmentVariable('CODEX_CLI_PATH', 'before-cli', 'Process')
[Environment]::SetEnvironmentVariable('CODEX_HOME', 'before-home', 'Process')
[Environment]::SetEnvironmentVariable('CODEX_SQLITE_HOME', 'before-sqlite', 'Process')
Restart-CodexDesktop `
    -LocalCliPath 'C:\local\codex.exe' `
    -LocalCodexHome 'C:\local\home' `
    -LocalCodexSqliteHome 'C:\local\sqlite'
[pscustomobject]@{{
    Launch = $script:Launch
    RestoredCliPath = [Environment]::GetEnvironmentVariable('CODEX_CLI_PATH', 'Process')
    RestoredCodexHome = [Environment]::GetEnvironmentVariable('CODEX_HOME', 'Process')
    RestoredSqliteHome = [Environment]::GetEnvironmentVariable('CODEX_SQLITE_HOME', 'Process')
}} | ConvertTo-Json -Compress -Depth 3
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
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
            env=clean_env(),
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout.splitlines()[-1])
        self.assertEqual(output["Launch"]["FilePath"], "explorer.exe")
        self.assertEqual(
            output["Launch"]["Argument"],
            r"shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App",
        )
        self.assertEqual(output["Launch"]["CliPath"], r"C:\local\codex.exe")
        self.assertEqual(output["Launch"]["CodexHome"], r"C:\local\home")
        self.assertEqual(output["Launch"]["SqliteHome"], r"C:\local\sqlite")
        self.assertEqual(output["RestoredCliPath"], "before-cli")
        self.assertEqual(output["RestoredCodexHome"], "before-home")
        self.assertEqual(output["RestoredSqliteHome"], "before-sqlite")
        self.assertIn("desktopRestart: restarted", result.stdout)

    def test_explorer_success_without_desktop_process_fails_restart(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
function Get-CodexDesktopExecutableProof {{ return 'C:\Program Files\WindowsApps\OpenAI.Codex\app\Codex.exe' }}
function Get-CodexDesktopProcessesForPath {{ return @() }}
function Start-Process {{}}
try {{
    Restart-CodexDesktop `
        -LocalCliPath 'C:\local\codex.exe' `
        -LocalCodexHome 'C:\local\home' `
        -LocalCodexSqliteHome 'C:\local\sqlite' `
        -ActivationTimeoutSeconds 1
    throw 'expected missing Desktop process to fail'
}}
catch {{
    if ($_.Exception.Message -eq 'expected missing Desktop process to fail') {{ throw }}
    $_.Exception.Message
}}
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
            env=clean_env(),
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("did not start", result.stdout)
        self.assertNotIn("desktopRestart: restarted", result.stdout)

    def test_runtime_probe_is_scoped_and_reports_failures(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        fixture = Path(self.enterContext(tempfile.TemporaryDirectory()))
        target = fixture / "probe.exe"
        source = r"""
using System;
using System.Threading;
public class RuntimeProbe {
    public static int Main(string[] args) {
        if (String.Join(" ", args) != "doctor --json --summary --runtime-only") {
            Thread.Sleep(10000);
            return 2;
        }
        if (Environment.GetEnvironmentVariable("CODEX_HOME") != Environment.GetEnvironmentVariable("EXPECTED_HOME") ||
            Environment.GetEnvironmentVariable("CODEX_CLI_PATH") != Environment.GetEnvironmentVariable("EXPECTED_TARGET")) {
            Console.Error.WriteLine("wrong probe routing");
            return 2;
        }
        string mode = Environment.GetEnvironmentVariable("PROBE_MODE");
        if (mode == "timeout") { Thread.Sleep(10000); }
        if (mode == "malformed") {
            Console.WriteLine("not JSON");
            Console.Error.WriteLine("unsupported probe option");
            return 2;
        }
        if (mode == "missing") { Console.WriteLine("{\"checks\":{}}"); return 0; }
        string status = mode == "mismatch" ? "warning" : "ok";
        Console.WriteLine("{\"checks\":{\"local_publish.readiness\":{\"status\":\"ok\"},\"desktop.runtime_chain\":{\"status\":\"" + status + "\",\"details\":[\"receipt CODEX_HOME does not match\"]}}}");
        return mode == "exit" ? 3 : 0;
    }
}
"""
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
Add-Type -TypeDefinition @'
{source}
'@ -OutputAssembly {ps_single_quote(target)} -OutputType ConsoleApplication
$env:EXPECTED_HOME = {ps_single_quote(fixture)}
$env:EXPECTED_TARGET = {ps_single_quote(target)}
$env:CODEX_HOME = 'wrong-home'
$env:CODEX_CLI_PATH = 'wrong-cli'
$results = foreach ($mode in @('ok', 'mismatch', 'malformed', 'missing', 'exit', 'timeout')) {{
    $env:PROBE_MODE = $mode
    $reason = 'stale failure'
    $timeout = if ($mode -eq 'timeout') {{ 100 }} else {{ 3000 }}
    $matched = Test-DesktopRuntimeProof -TargetPath $env:EXPECTED_TARGET `
        -LocalCodexHome $env:EXPECTED_HOME -TimeoutMilliseconds $timeout -FailureReason ([ref]$reason)
    [pscustomobject]@{{ Mode = $mode; Matched = $matched; Reason = $reason }}
}}
$reason = $null
$matched = Test-DesktopRuntimeProof -TargetPath {ps_single_quote(fixture / "missing.exe")} -FailureReason ([ref]$reason)
$results += [pscustomobject]@{{ Mode = 'absent'; Matched = $matched; Reason = $reason }}
[pscustomobject]@{{ Results = @($results); Home = $env:CODEX_HOME; Cli = $env:CODEX_CLI_PATH }} | ConvertTo-Json -Compress -Depth 5
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
            env=clean_env(),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output["Home"], "wrong-home")
        self.assertEqual(output["Cli"], "wrong-cli")
        results = {row["Mode"]: row for row in output["Results"]}
        self.assertTrue(results["ok"]["Matched"])
        self.assertIsNone(results["ok"]["Reason"])
        for mode, reason in {
            "mismatch": "receipt CODEX_HOME does not match",
            "malformed": "unsupported probe option",
            "missing": "missing local_publish.readiness",
            "exit": "exited with 3",
            "timeout": "timed out after 100 ms",
            "absent": "target is missing",
        }.items():
            with self.subTest(mode=mode):
                self.assertFalse(results[mode]["Matched"])
                self.assertIn(reason, results[mode]["Reason"])

    def test_live_desktop_with_mismatched_runtime_receipt_fails_restart(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")
        command = rf"""
. {ps_single_quote(SCRIPT)} -ImportOnly
$script:Launched = $false
function Get-CodexDesktopExecutableProof {{ return 'C:\Program Files\WindowsApps\OpenAI.Codex\app\Codex.exe' }}
function Get-CodexDesktopProcessesForPath {{
    param([string]$DesktopPath)
    if (-not $script:Launched) {{ return @() }}
    return @([pscustomobject]@{{ Id = 91; Path = $DesktopPath }})
}}
function Test-DesktopRuntimeProof {{
    param([string]$TargetPath, [string]$LocalCodexHome, [int]$TimeoutMilliseconds, [ref]$FailureReason)
    $FailureReason.Value = 'receipt CODEX_HOME does not match'
    return $false
}}
function Start-Process {{ $script:Launched = $true }}
try {{
    Restart-CodexDesktop `
        -LocalCliPath 'C:\local\codex.exe' `
        -LocalCodexHome 'C:\local\home' `
        -LocalCodexSqliteHome 'C:\local\sqlite' `
        -ActivationTimeoutSeconds 1
    throw 'expected mismatched receipt to fail'
}}
catch {{
    if ($_.Exception.Message -eq 'expected mismatched receipt to fail') {{ throw }}
    $_.Exception.Message
}}
"""
        result = subprocess.run(
            [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", command],
            text=True,
            capture_output=True,
            check=False,
            timeout=RUN_TIMEOUT_SECONDS,
            creationflags=CREATE_NO_WINDOW,
            env=clean_env(),
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "runtime could not be verified: receipt CODEX_HOME does not match",
            result.stdout,
        )
        self.assertNotIn("desktopRestart: restarted", result.stdout)

    def test_noop_restart_failure_is_terminal_after_committed_publish(self) -> None:
        publish_script = publish_source_text()
        noop_branch = publish_script.split("if (-not $binaryChanged) {", 1)[1].split(
            "$publishedCodeModeHost = $false", 1
        )[0]

        self.assertIn("Publish committed but Desktop restart failed", noop_branch)
        self.assertLess(
            noop_branch.index("Publish committed but Desktop restart failed"),
            noop_branch.index("exit 0"),
        )

    def test_default_local_publish_target_is_not_openai_appdata_bin(self) -> None:
        publish_script = publish_source_text()

        self.assertIn('Join-Path $env:USERPROFILE "Desktop\\LOCAL-KD"', publish_script)
        self.assertNotIn(
            'Join-Path $env:LOCALAPPDATA "OpenAI\\Codex\\bin\\codexKD-local"',
            publish_script,
        )

    def test_publish_doctor_allows_only_non_runtime_failures(self) -> None:
        shell = powershell()
        if shell is None:
            self.skipTest("PowerShell is not available")

        command = rf"""
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile({ps_single_quote(SCRIPT)}, [ref]$tokens, [ref]$errors)
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
$automationTerminal = '{{"checks":{{"terminal.env":{{"status":"fail","summary":"TERM=dumb - colors and cursor control are disabled"}},"local_publish.readiness":{{"status":"ok"}}}}}}'
$authAndAutomationTerminal = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"terminal.env":{{"status":"fail","summary":"TERM=dumb - colors and cursor control are disabled"}},"local_publish.readiness":{{"status":"ok"}}}}}}'
$terminalWithoutReadiness = '{{"checks":{{"terminal.env":{{"status":"fail","summary":"TERM=dumb - colors and cursor control are disabled"}}}}}}'
$differentTerminalFailure = '{{"checks":{{"terminal.env":{{"status":"fail","summary":"terminal metadata is invalid"}},"local_publish.readiness":{{"status":"ok"}}}}}}'
$configFailure = '{{"checks":{{"auth.credentials":{{"status":"fail"}},"config.load":{{"status":"fail"}}}}}}'
[pscustomobject]@{{
    authOnly = Test-DoctorFailureAllowedForPublish -OutputLines @($authOnly)
    automationTerminal = Test-DoctorFailureAllowedForPublish -OutputLines @($automationTerminal)
    authAndAutomationTerminal = Test-DoctorFailureAllowedForPublish -OutputLines @($authAndAutomationTerminal)
    terminalWithoutReadiness = Test-DoctorFailureAllowedForPublish -OutputLines @($terminalWithoutReadiness)
    differentTerminalFailure = Test-DoctorFailureAllowedForPublish -OutputLines @($differentTerminalFailure)
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
        self.assertTrue(output["automationTerminal"])
        self.assertTrue(output["authAndAutomationTerminal"])
        self.assertFalse(output["terminalWithoutReadiness"])
        self.assertFalse(output["differentTerminalFailure"])
        self.assertFalse(output["configFailure"])

    def test_publish_run_doctor_uses_publish_classifier(self) -> None:
        publish_script = publish_source_text()

        self.assertIn("function Invoke-DoctorForPublish", publish_script)
        self.assertIn("warning: allowed non-runtime doctor failure", publish_script)
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
