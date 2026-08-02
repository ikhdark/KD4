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
$fastHelperName = switch ($ExpectedEvent) {
    { $_ -in @('UserPromptSubmit', 'Stop') } { 'task-continuity-fast-basic.ps1'; break }
    { $_ -in @('PreCompact', 'PostCompact') } { 'task-continuity-fast-compact.ps1'; break }
    'SessionStart' { 'task-continuity-fast-session.ps1'; break }
}

try {
    $fastHelper = Join-Path $PSScriptRoot $fastHelperName
    & $fastHelper -TaskContinuityRawInput $rawInput -ExpectedEvent $ExpectedEvent
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
