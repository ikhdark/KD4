param(
    [Parameter(Mandatory = $true)]
    [ValidateSet(
        'UserPromptSubmit',
        'PreCompact',
        'PostCompact',
        'SessionStart',
        'Stop'
    )]
    [string]$ExpectedEvent
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$InformationPreference = 'SilentlyContinue'
$VerbosePreference = 'SilentlyContinue'
$WarningPreference = 'SilentlyContinue'

$emptyOutput = '{}'
$rawInput = [Console]::In.ReadToEnd()

try {
    $repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
    $expectedHookDirectory = [System.IO.Path]::GetFullPath(
        (Join-Path $repositoryRoot '.codex\hooks')
    )
    if (-not [string]::Equals(
        $expectedHookDirectory.TrimEnd([char[]]@('\', '/')),
        ([System.IO.Path]::GetFullPath($PSScriptRoot)).TrimEnd([char[]]@('\', '/')),
        [StringComparison]::OrdinalIgnoreCase
    )) {
        throw 'repository root validation failed'
    }
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $lockIdentity = [BitConverter]::ToString(
            $sha256.ComputeHash(
                [Text.Encoding]::UTF8.GetBytes(
                    [System.IO.Path]::GetFullPath($PSScriptRoot).ToLowerInvariant()
                )
            )
        ).Replace('-', '')
    }
    finally {
        $sha256.Dispose()
    }
    $stateMutex = [Threading.Mutex]::new(
        $false,
        "Local\KD4TaskContinuity-$lockIdentity"
    )
    $lockTaken = $false
    try {
        $lockTaken = $stateMutex.WaitOne([TimeSpan]::FromSeconds(15))
        if (-not $lockTaken) {
            throw 'timed out waiting for the task-continuity state lease'
        }
        & (Join-Path $PSScriptRoot 'task-continuity.ps1') `
            -TaskContinuityRawInput $rawInput `
            -TaskContinuityExpectedEvent $ExpectedEvent `
            -TaskContinuityRepositoryRoot $repositoryRoot
    }
    finally {
        if ($lockTaken) {
            $stateMutex.ReleaseMutex()
        }
        $stateMutex.Dispose()
    }
}
catch {
    try {
        [Console]::Error.WriteLine("task-continuity: $($_.Exception.Message)")
    }
    catch {
        # Diagnostics must never interfere with fail-open output.
    }
    [Console]::Out.Write($emptyOutput)
}
exit 0
