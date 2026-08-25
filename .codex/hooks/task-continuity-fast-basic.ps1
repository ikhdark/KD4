param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('UserPromptSubmit', 'Stop')]
    [string]$ExpectedEvent,
    [AllowNull()][string]$TaskContinuityRepositoryRoot
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

# Compatibility entry point. The canonical handler owns parsing, fast-path
# selection, state observation, and output so a fallback never reparses input.
& (Join-Path $PSScriptRoot 'task-continuity.ps1') `
    -TaskContinuityRawInput $TaskContinuityRawInput `
    -TaskContinuityExpectedEvent $ExpectedEvent `
    -TaskContinuityRepositoryRoot $TaskContinuityRepositoryRoot
