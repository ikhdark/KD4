param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("clippy", "dead-code")]
    [string]$Analyzer,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ForwardedArgs
)

$ErrorActionPreference = "Stop"

$cargoLaneScript = Join-Path $PSScriptRoot "cargo-lane.ps1"
$v8SandboxPackage = "codex-code-mode"

function Invoke-CargoLane {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Lane,

        [Parameter(Mandatory = $true)]
        [string[]]$CargoArgs
    )

    & powershell -NoProfile -ExecutionPolicy Bypass -File $cargoLaneScript `
        -Lane $Lane cargo @CargoArgs
    return $LASTEXITCODE
}

function Remove-WorkspaceFeatureArgs {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Args
    )

    $filtered = [System.Collections.Generic.List[string]]::new()
    for ($index = 0; $index -lt $Args.Count; $index++) {
        $arg = $Args[$index]
        if ($arg -in @("--workspace", "--all-features")) {
            continue
        }
        if ($arg -eq "--exclude") {
            $index++
            continue
        }
        if ($arg.StartsWith("--exclude=")) {
            continue
        }
        $filtered.Add($arg)
    }
    return $filtered.ToArray()
}

$forwarded = @(
    $ForwardedArgs | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
)
$hasAllFeatures = $forwarded -contains "--all-features"
$hasExplicitPackage =
    ($forwarded -contains "-p") -or
    ($forwarded -contains "--package") -or
    ($forwarded -contains "--manifest-path") -or
    @($forwarded | Where-Object {
        $_.StartsWith("--package=") -or $_.StartsWith("--manifest-path=")
    }).Count -gt 0

if ($Analyzer -eq "clippy") {
    $lane = "auto"
    $cargoArgs = @("clippy", "--tests") + $forwarded
    $isWorkspace = $forwarded -contains "--workspace"
} else {
    $lane = "rust-dead-code-matrix"
    if ([string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
        $env:RUSTFLAGS = "-Ddead_code"
    }
    else {
        $env:RUSTFLAGS = "$($env:RUSTFLAGS) -Ddead_code"
    }
    $cargoArgs = @("check")
    if (-not $hasExplicitPackage) {
        $cargoArgs += "--workspace"
    }
    $cargoArgs += "--all-targets"
    $cargoArgs += $forwarded
    $isWorkspace = -not $hasExplicitPackage
}

$needsWindowsV8Fallback =
    ($env:OS -eq "Windows_NT") -and
    $hasAllFeatures -and
    $isWorkspace

if (-not $needsWindowsV8Fallback) {
    exit (Invoke-CargoLane -Lane $lane -CargoArgs $cargoArgs)
}

# rusty_v8 does not publish a Windows archive for the ptrcomp+sandbox feature
# combination. Analyze the complete workspace with the forwarding packages
# excluded, then analyze those packages without that unavailable upstream
# feature. Their Rust sources contain no sandbox-gated code.
Write-Warning (
    "rusty_v8 has no Windows ptrcomp+sandbox archive; " +
    "checking the full workspace while omitting only that upstream feature."
)

$workspaceArgs = $cargoArgs + @("--exclude", $v8SandboxPackage)
$exitCode = Invoke-CargoLane -Lane $lane -CargoArgs $workspaceArgs
if ($exitCode -ne 0) {
    exit $exitCode
}

$packageForwarded = Remove-WorkspaceFeatureArgs -Args $forwarded
if ($Analyzer -eq "clippy") {
    $packageArgs = @("clippy", "--tests")
} else {
    $packageArgs = @("check", "--all-targets")
}
$packageArgs += @("--package", $v8SandboxPackage)
$packageArgs += $packageForwarded

exit (Invoke-CargoLane -Lane $lane -CargoArgs $packageArgs)
