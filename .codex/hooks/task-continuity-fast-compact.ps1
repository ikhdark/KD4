param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('PreCompact', 'PostCompact')]
    [string]$ExpectedEvent,
    [AllowNull()][string]$TaskContinuityRepositoryRoot
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# Compatibility entry point. Event-specific behavior lives in the canonical
# handler, which carries its first parsed request into the validated path.
& (Join-Path $PSScriptRoot 'task-continuity.ps1') `
    -TaskContinuityRawInput $TaskContinuityRawInput `
    -TaskContinuityExpectedEvent $ExpectedEvent `
    -TaskContinuityRepositoryRoot $TaskContinuityRepositoryRoot
