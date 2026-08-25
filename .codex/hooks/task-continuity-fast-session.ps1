param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('SessionStart')]
    [string]$ExpectedEvent,
    [AllowNull()][string]$TaskContinuityRepositoryRoot
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# Compatibility entry point. Recovery rendering and repository observation
# remain authoritative in the canonical handler.
& (Join-Path $PSScriptRoot 'task-continuity.ps1') `
    -TaskContinuityRawInput $TaskContinuityRawInput `
    -TaskContinuityExpectedEvent $ExpectedEvent `
    -TaskContinuityRepositoryRoot $TaskContinuityRepositoryRoot
