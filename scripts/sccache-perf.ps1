param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("stats", "reset", "restart")]
    [string]$Action
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "common-rust-env.ps1")

function Get-SccacheCommandPath {
    $command = Get-Command sccache -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        throw "sccache is required for this command. Install it with Scoop or remove the sccache recipe from your local workflow."
    }
    if (-not [string]::IsNullOrWhiteSpace($command.Source)) {
        return $command.Source
    }
    return $command.Name
}

function Invoke-SccachePerfCommand {
    param(
        [string]$SccachePath,
        [string[]]$Arguments,
        [switch]$IgnoreFailure,
        [switch]$SuppressStderr
    )

    # Windows PowerShell 5.1 turns redirected native stderr into terminating
    # errors while $ErrorActionPreference is "Stop", which would defeat
    # -IgnoreFailure before the exit-code check below runs.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($SuppressStderr) {
            & $SccachePath @Arguments 2>$null
        }
        else {
            & $SccachePath @Arguments
        }
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    $exitCode = if ($null -eq $LASTEXITCODE) { 0 } else { $LASTEXITCODE }
    if ($exitCode -ne 0 -and -not $IgnoreFailure) {
        throw "sccache $($Arguments -join ' ') failed with exit code $exitCode."
    }
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$sccache = Get-SccacheCommandPath
Set-CodexRustSccacheEnvironment -RepoRoot $repoRoot
if ($Action -in @("stats", "reset")) {
    Ensure-CodexRustSccacheServer -RepoRoot $repoRoot
}

switch ($Action) {
    "stats" {
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--show-stats")
    }
    "reset" {
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--zero-stats")
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--show-stats")
    }
    "restart" {
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--stop-server") -IgnoreFailure -SuppressStderr
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--start-server")
        Invoke-SccachePerfCommand -SccachePath $sccache -Arguments @("--show-stats")
    }
}
