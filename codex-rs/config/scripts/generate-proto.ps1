[CmdletBinding()]
param(
    [switch]$Check,
    [string]$ProtocPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $scriptDir '../../..'))
$sourceProtoDir = Join-Path $repoRoot 'codex-rs/config/src/thread_config/proto'
$sourceProto = Join-Path $sourceProtoDir 'codex.thread_config.v1.proto'
$checkedGenerated = Join-Path $sourceProtoDir 'codex.thread_config.v1.rs'
$cargoLane = Join-Path $repoRoot 'scripts/cargo-lane.ps1'
$tempRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath())
$tmpDir = [System.IO.Path]::GetFullPath(
    (Join-Path $tempRoot ('codex-generate-proto-' + [guid]::NewGuid().ToString('N')))
)
$protoDir = Join-Path $tmpDir 'proto'
$generated = Join-Path $protoDir 'codex.thread_config.v1.rs'
$clippyAllow = '#![allow(clippy::trivially_copy_pass_by_ref)]'
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$originalProtoc = $env:PROTOC

function Write-LfFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][AllowEmptyString()][string[]]$Lines
    )

    $contents = if ($Lines.Count -eq 0) {
        ''
    }
    else {
        ([string]::Join("`n", $Lines) + "`n")
    }
    [System.IO.File]::WriteAllText($Path, $contents, $utf8NoBom)
}

function Test-FilesEqual {
    param(
        [Parameter(Mandatory = $true)][string]$Left,
        [Parameter(Mandatory = $true)][string]$Right
    )

    $leftBytes = [System.IO.File]::ReadAllBytes($Left)
    $rightBytes = [System.IO.File]::ReadAllBytes($Right)
    if ($leftBytes.Length -ne $rightBytes.Length) {
        return $false
    }
    for ($i = 0; $i -lt $leftBytes.Length; $i++) {
        if ($leftBytes[$i] -ne $rightBytes[$i]) {
            return $false
        }
    }
    return $true
}

function Install-GeneratedFileAtomically {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    $destinationDirectory = Split-Path -Parent $Destination
    $destinationName = Split-Path -Leaf $Destination
    $temporaryPath = Join-Path $destinationDirectory (
        ".${destinationName}." + [guid]::NewGuid().ToString('N') + '.tmp'
    )
    $backupPath = $temporaryPath + '.bak'
    try {
        [System.IO.File]::Copy($Source, $temporaryPath, $false)
        if ([System.IO.File]::Exists($Destination)) {
            [System.IO.File]::Replace($temporaryPath, $Destination, $backupPath)
        }
        else {
            [System.IO.File]::Move($temporaryPath, $Destination)
        }
    }
    finally {
        if ([System.IO.File]::Exists($temporaryPath)) {
            [System.IO.File]::Delete($temporaryPath)
        }
        if ([System.IO.File]::Exists($backupPath)) {
            [System.IO.File]::Delete($backupPath)
        }
    }
}

function Resolve-ProtocPath {
    param(
        [string]$ExplicitPath
    )

    $candidates = [System.Collections.Generic.List[string]]::new()
    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        $candidates.Add($ExplicitPath)
    }
    if (-not [string]::IsNullOrWhiteSpace($env:PROTOC)) {
        $candidates.Add($env:PROTOC)
    }
    $protocCommand = Get-Command protoc -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -ne $protocCommand) {
        $candidates.Add($protocCommand.Source)
    }

    $cargoHome = $env:CARGO_HOME
    if ([string]::IsNullOrWhiteSpace($cargoHome)) {
        $userHome = if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
            $env:USERPROFILE
        }
        else {
            $env:HOME
        }
        if (-not [string]::IsNullOrWhiteSpace($userHome)) {
            $cargoHome = Join-Path $userHome '.cargo'
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($cargoHome)) {
        $vendoredPattern = Join-Path $cargoHome (
            'registry/src/*/protoc-bin-vendored-win32-*/bin/protoc.exe'
        )
        Get-ChildItem -Path $vendoredPattern -File -ErrorAction SilentlyContinue |
            Sort-Object FullName -Descending |
            ForEach-Object { $candidates.Add($_.FullName) }
    }

    foreach ($candidate in $candidates) {
        $resolved = [System.IO.Path]::GetFullPath($candidate)
        if ([System.IO.File]::Exists($resolved)) {
            return $resolved
        }
    }

    throw (
        'protoc was not found. Install Protocol Buffers, set PROTOC, or pass ' +
        '-ProtocPath <path-to-protoc.exe>.'
    )
}

function Normalize-GeneratedProto {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    $lines = [System.IO.File]::ReadAllLines($Path)
    if ($lines.Count -eq 0) {
        throw "generate-proto produced an empty file: $Path"
    }

    $withAllow = [System.Collections.Generic.List[string]]::new()
    $withAllow.Add($lines[0])
    if ($lines.Count -lt 2 -or $lines[1] -notmatch 'clippy::trivially_copy_pass_by_ref') {
        $withAllow.Add($clippyAllow)
    }
    for ($i = 1; $i -lt $lines.Count; $i++) {
        $withAllow.Add($lines[$i])
    }
    Write-LfFile -Path $Path -Lines $withAllow.ToArray()

    & rustfmt --edition 2024 $Path
    if ($LASTEXITCODE -ne 0) {
        throw "rustfmt failed with exit code $LASTEXITCODE"
    }

    $lines = [System.IO.File]::ReadAllLines($Path)
    $formatted = [System.Collections.Generic.List[string]]::new()
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if (
            $i -eq 2 -and
            $lines[$i - 1] -match 'clippy::trivially_copy_pass_by_ref' -and
            $lines[$i] -ne ''
        ) {
            $formatted.Add('')
        }
        $formatted.Add($lines[$i])
    }
    Write-LfFile -Path $Path -Lines $formatted.ToArray()
}

if (-not [System.IO.File]::Exists($sourceProto)) {
    throw "config proto source not found: $sourceProto"
}
if (-not [System.IO.File]::Exists($cargoLane)) {
    throw "Cargo lane wrapper not found: $cargoLane"
}

$resolvedProtoc = Resolve-ProtocPath -ExplicitPath $ProtocPath
$env:PROTOC = $resolvedProtoc
Write-Output "Using protoc: $resolvedProtoc"

try {
    [System.IO.Directory]::CreateDirectory($protoDir) | Out-Null
    [System.IO.File]::Copy(
        $sourceProto,
        (Join-Path $protoDir (Split-Path -Leaf $sourceProto)),
        $true
    )

    & $cargoLane -Lane auto cargo run --locked -p codex-config --example generate-proto -- $protoDir
    if ($LASTEXITCODE -ne 0) {
        throw "generate-proto failed with exit code $LASTEXITCODE"
    }
    if (-not [System.IO.File]::Exists($generated)) {
        throw "generate-proto did not create expected output: $generated"
    }

    Normalize-GeneratedProto -Path $generated

    $isCurrent = [System.IO.File]::Exists($checkedGenerated) -and
        (Test-FilesEqual -Left $generated -Right $checkedGenerated)
    if ($Check) {
        if (-not $isCurrent) {
            throw "Generated config proto is stale. Run: just generate-config-proto"
        }
        Write-Output "Config proto is up to date: $checkedGenerated"
    }
    elseif ($isCurrent) {
        Write-Output "Config proto is already current: $checkedGenerated"
    }
    else {
        Install-GeneratedFileAtomically -Source $generated -Destination $checkedGenerated
        Write-Output "Updated config proto: $checkedGenerated"
    }
}
finally {
    if ($null -eq $originalProtoc) {
        Remove-Item Env:PROTOC -ErrorAction SilentlyContinue
    }
    else {
        $env:PROTOC = $originalProtoc
    }
    if (-not $tmpDir.StartsWith($tempRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to clean up unexpected temporary path: $tmpDir"
    }
    if ([System.IO.Directory]::Exists($tmpDir)) {
        Remove-Item -LiteralPath $tmpDir -Recurse -Force
    }
}
