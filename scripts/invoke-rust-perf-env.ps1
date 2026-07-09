param(
    [switch]$NoSccache,
    [string]$WorkingDirectory,
    [string]$CargoTargetLane,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ProgramArgs
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

. (Join-Path $PSScriptRoot "common-rust-env.ps1")

function ConvertTo-SafeCargoLaneName {
    param(
        [string]$Value
    )

    $safe = ([string]$Value -replace "[^A-Za-z0-9_.-]", "-").Trim("-")
    if ([string]::IsNullOrWhiteSpace($safe)) {
        throw "Cargo target lane cannot be empty."
    }
    return $safe
}

function Format-EnvProofValue {
    param(
        [string]$Name
    )

    $value = [System.Environment]::GetEnvironmentVariable($Name, "Process")
    if ($null -eq $value) {
        return "<unset>"
    }
    if ($value -eq "") {
        return "<empty>"
    }
    return $value
}

function Test-SccacheWrapper {
    param(
        [AllowNull()]
        [string]$Value
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    if ($Value -in @("sccache", "sccache.exe")) {
        return $true
    }
    $leaf = Split-Path -Leaf $Value
    return $leaf -in @("sccache", "sccache.exe")
}

function Test-ProgramArgsHaveCargoTargetDir {
    param(
        [string[]]$CommandArgs
    )

    $subcommandIndex = Get-CargoSubcommandIndex -CommandArgs $CommandArgs
    if ($subcommandIndex -lt 0) {
        return $false
    }

    $startIndex = $subcommandIndex + 1
    if ($CommandArgs[$subcommandIndex] -eq "nextest") {
        $nextestCommandIndex = $subcommandIndex + 1
        if (
            $nextestCommandIndex -lt $CommandArgs.Count -and
            $CommandArgs[$nextestCommandIndex] -in @("archive", "run")
        ) {
            $startIndex = $nextestCommandIndex + 1
        }
    }

    return Test-CargoTargetDirArgumentPresent -CommandArgs $CommandArgs -StartIndex $startIndex
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
if ($null -eq $ProgramArgs -or $ProgramArgs.Count -eq 0) {
    throw "No command was provided."
}

$oldSccacheBaseDir = $env:SCCACHE_BASEDIR
$oldSccacheCacheSize = $env:SCCACHE_CACHE_SIZE
$oldCargoIncremental = $env:CARGO_INCREMENTAL
$oldCargoTargetDir = $env:CARGO_TARGET_DIR
$oldRustcWrapper = $env:RUSTC_WRAPPER
$oldWindowsMsvcLinker = $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER
$oldNativeCommandUseErrorActionPreference = Get-Variable -Name PSNativeCommandUseErrorActionPreference -ValueOnly -ErrorAction SilentlyContinue
$hadNativeCommandUseErrorActionPreference = $null -ne (Get-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue)
$didPushLocation = $false
$locationStackName = "codex-rust-perf-env-$PID"
$exitCode = 1

try {
    $PSNativeCommandUseErrorActionPreference = $false
    if ($NoSccache) {
        Remove-Item Env:SCCACHE_BASEDIR -ErrorAction SilentlyContinue
        Remove-Item Env:SCCACHE_CACHE_SIZE -ErrorAction SilentlyContinue
        $env:RUSTC_WRAPPER = ""
    }
    elseif ([string]::IsNullOrWhiteSpace($env:RUSTC_WRAPPER) -and (Get-Command sccache -ErrorAction SilentlyContinue)) {
        Set-CodexRustSccacheEnvironment -RepoRoot $repoRoot
        $env:CARGO_INCREMENTAL = "0"
        $env:RUSTC_WRAPPER = "sccache"
        Ensure-CodexRustSccacheServer -RepoRoot $repoRoot
    }
    elseif (Test-SccacheWrapper -Value $env:RUSTC_WRAPPER) {
        Set-CodexRustSccacheEnvironment -RepoRoot $repoRoot
        $env:CARGO_INCREMENTAL = "0"
        Ensure-CodexRustSccacheServer -RepoRoot $repoRoot
    }
    Set-CodexRustMsvcLinkerEnvironment

    $cargoTargetDirProof = Format-EnvProofValue -Name "CARGO_TARGET_DIR"
    if (-not [string]::IsNullOrWhiteSpace($CargoTargetLane)) {
        $laneName = ConvertTo-SafeCargoLaneName -Value $CargoTargetLane
        $laneTargetDir = Join-Path $repoRoot "codex-rs\target\lanes\$laneName"
        $hasExplicitCargoTargetDir = Test-ProgramArgsHaveCargoTargetDir -CommandArgs $ProgramArgs
        $programArgsWithTargetDir = @(Add-CargoTargetDirArgument -CommandArgs $ProgramArgs -TargetDir $laneTargetDir)
        if ($hasExplicitCargoTargetDir) {
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
            $cargoTargetDirProof = "<explicit command argument>"
        }
        elseif ($programArgsWithTargetDir.Count -eq $ProgramArgs.Count) {
            $env:CARGO_TARGET_DIR = $laneTargetDir
            $cargoTargetDirProof = $laneTargetDir
        }
        else {
            # An exported CARGO_TARGET_DIR lands in sccache's cache key even
            # when cargo itself uses --target-dir, so drop any inherited value.
            Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
            $ProgramArgs = $programArgsWithTargetDir
            $cargoTargetDirProof = $laneTargetDir
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($WorkingDirectory)) {
        Push-Location $WorkingDirectory -StackName $locationStackName
        $didPushLocation = $true
    }

    Write-Output ("rustPerfEnv: rustcWrapper={0}; cargoIncremental={1}; sccacheBaseDir={2}; cargoTargetDir={3}; windowsMsvcLinker={4}" -f `
            (Format-EnvProofValue -Name "RUSTC_WRAPPER"),
            (Format-EnvProofValue -Name "CARGO_INCREMENTAL"),
            (Format-EnvProofValue -Name "SCCACHE_BASEDIR"),
            $cargoTargetDirProof,
            (Format-EnvProofValue -Name "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"))

    $global:LASTEXITCODE = $null
    $program = $ProgramArgs[0]
    $arguments = @($ProgramArgs | Select-Object -Skip 1)
    & $program @arguments
    if ($null -eq $LASTEXITCODE) {
        if ($?) {
            $exitCode = 0
        }
        else {
            $exitCode = 1
        }
    }
    else {
        $exitCode = $LASTEXITCODE
    }
}
finally {
    if ($didPushLocation) {
        Pop-Location -StackName $locationStackName
    }
    $env:SCCACHE_BASEDIR = $oldSccacheBaseDir
    $env:SCCACHE_CACHE_SIZE = $oldSccacheCacheSize
    $env:CARGO_INCREMENTAL = $oldCargoIncremental
    $env:CARGO_TARGET_DIR = $oldCargoTargetDir
    $env:RUSTC_WRAPPER = $oldRustcWrapper
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $oldWindowsMsvcLinker
    if ($hadNativeCommandUseErrorActionPreference) {
        $PSNativeCommandUseErrorActionPreference = $oldNativeCommandUseErrorActionPreference
    }
    else {
        Remove-Variable -Name PSNativeCommandUseErrorActionPreference -ErrorAction SilentlyContinue
    }
}

exit $exitCode
