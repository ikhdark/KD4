$ErrorActionPreference = "Stop"
# Windows PowerShell's parameter binder treats a forwarded `--` as an
# ambiguous empty parameter. Consume our selector and keep Cargo's tokens raw.
$analyzerIndex = if ($args.Count -gt 0 -and $args[0] -eq "-Analyzer") { 1 } else { 0 }
if ($args.Count -le $analyzerIndex -or $args[$analyzerIndex] -notin @("clippy", "dead-code")) {
    throw "-Analyzer must be clippy or dead-code."
}
$Analyzer = [string]$args[$analyzerIndex]
$ForwardedArgs = @($args | Select-Object -Skip ($analyzerIndex + 1))

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
    $script:CargoLaneExitCode = $LASTEXITCODE
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
$separator = [Array]::IndexOf($forwarded, "--")
$compilerArgs = @()
if ($separator -ge 0) {
    $compilerArgs = @($forwarded | Select-Object -Skip $separator)
    $forwarded = @($forwarded | Select-Object -First $separator)
}
$excludedSandbox = $forwarded -contains "--exclude=$v8SandboxPackage"
for ($index = 0; $index -lt $forwarded.Count - 1; $index++) {
    if ($forwarded[$index] -eq "--exclude" -and $forwarded[$index + 1] -eq $v8SandboxPackage) {
        $excludedSandbox = $true
    }
}
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
    if (Test-Path Env:CARGO_ENCODED_RUSTFLAGS) {
        $env:CARGO_ENCODED_RUSTFLAGS = (@($env:CARGO_ENCODED_RUSTFLAGS, "-Ddead_code") | Where-Object { $_ -ne "" }) -join [char]0x1f
    }
    elseif ([string]::IsNullOrWhiteSpace($env:RUSTFLAGS)) {
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
    Invoke-CargoLane -Lane $lane -CargoArgs ($cargoArgs + $compilerArgs)
    exit $script:CargoLaneExitCode
}

# rusty_v8 does not publish a Windows archive for the ptrcomp+sandbox feature
# combination. Analyze the complete workspace with the forwarding packages
# excluded, then analyze those packages without that unavailable upstream
# feature. Their Rust sources contain no sandbox-gated code.
Write-Warning (
    "rusty_v8 has no Windows ptrcomp+sandbox archive; " +
    "checking the full workspace while omitting only that upstream feature."
)

$workspaceArgs = $cargoArgs
if (-not $excludedSandbox) {
    $workspaceArgs += @("--exclude", $v8SandboxPackage)
}
Invoke-CargoLane -Lane $lane -CargoArgs ($workspaceArgs + $compilerArgs)
if ($script:CargoLaneExitCode -ne 0 -or $excludedSandbox) {
    exit $script:CargoLaneExitCode
}

$packageForwarded = Remove-WorkspaceFeatureArgs -Args $forwarded
if ($Analyzer -eq "clippy") {
    $packageArgs = @("clippy", "--tests")
} else {
    $packageArgs = @("check", "--all-targets")
}
$packageArgs += @("--package", $v8SandboxPackage)
$packageArgs += $packageForwarded

Invoke-CargoLane -Lane $lane -CargoArgs ($packageArgs + $compilerArgs)
exit $script:CargoLaneExitCode
