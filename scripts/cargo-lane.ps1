Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

. (Join-Path $PSScriptRoot "common-rust-env.ps1")

function Parse-CargoLaneArguments {
    param(
        [object[]]$RawArgs
    )

    $parsedLane = $null
    $parsedLanesRoot = $null
    $parsedIsolateCargoHome = $false
    $parsedFetch = $false
    $commandStart = $RawArgs.Count

    for ($i = 0; $i -lt $RawArgs.Count; $i++) {
        $arg = [string]$RawArgs[$i]
        if ($arg -eq "--") {
            $commandStart = $i + 1
            break
        }
        if ($arg -eq "-Lane") {
            $i++
            if ($i -ge $RawArgs.Count) {
                throw "-Lane requires a value."
            }
            $parsedLane = [string]$RawArgs[$i]
            continue
        }
        if ($arg.StartsWith("-Lane:", [StringComparison]::OrdinalIgnoreCase)) {
            $parsedLane = $arg.Substring("-Lane:".Length)
            continue
        }
        if ($arg -eq "-LanesRoot") {
            $i++
            if ($i -ge $RawArgs.Count) {
                throw "-LanesRoot requires a value."
            }
            $parsedLanesRoot = [string]$RawArgs[$i]
            continue
        }
        if ($arg.StartsWith("-LanesRoot:", [StringComparison]::OrdinalIgnoreCase)) {
            $parsedLanesRoot = $arg.Substring("-LanesRoot:".Length)
            continue
        }
        if ($arg -eq "-IsolateCargoHome") {
            $parsedIsolateCargoHome = $true
            continue
        }
        if ($arg -eq "-Fetch") {
            $parsedFetch = $true
            continue
        }
        if ($null -eq $parsedLane -and -not $arg.StartsWith("-", [StringComparison]::Ordinal)) {
            $parsedLane = $arg
            continue
        }
        $commandStart = $i
        break
    }

    if ([string]::IsNullOrWhiteSpace($parsedLane)) {
        throw "-Lane is required."
    }
    if ($parsedLane -notmatch "^[A-Za-z0-9_.-]+$") {
        throw "Lane '$parsedLane' contains unsupported characters."
    }
    if ($parsedLane -match "^\.\.?$") {
        # "." and ".." pass the character filter but resolve the lane dir to
        # the lanes root or the shared target root, escaping lane isolation.
        throw "Lane '$parsedLane' is not a valid lane name."
    }

    return [pscustomobject]@{
        Lane = $parsedLane
        LanesRoot = $parsedLanesRoot
        IsolateCargoHome = $parsedIsolateCargoHome
        Fetch = $parsedFetch
        Command = @($RawArgs | Select-Object -Skip $commandStart)
    }
}

$parsedArgs = Parse-CargoLaneArguments -RawArgs $args
$Lane = $parsedArgs.Lane
$LanesRoot = $parsedArgs.LanesRoot
$IsolateCargoHome = [bool]$parsedArgs.IsolateCargoHome
$Fetch = [bool]$parsedArgs.Fetch
$Command = @($parsedArgs.Command)

function Get-RepoRoot {
    return [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
}

function Test-SccacheWrapper {
    param(
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

function Enable-SccacheEnvironment {
    param(
        [string]$RepoRoot
    )

    if ([string]::IsNullOrWhiteSpace($env:RUSTC_WRAPPER)) {
        $env:RUSTC_WRAPPER = "sccache"
        Set-CodexRustSccacheEnvironment -RepoRoot $RepoRoot
        Ensure-CodexRustSccacheServer -RepoRoot $RepoRoot
    }
    elseif (Test-SccacheWrapper -Value $env:RUSTC_WRAPPER) {
        Set-CodexRustSccacheEnvironment -RepoRoot $RepoRoot
        Ensure-CodexRustSccacheServer -RepoRoot $RepoRoot
    }
}

function ConvertTo-SafeLaneName {
    param(
        [string]$Value
    )

    $safe = ([string]$Value -replace "[^A-Za-z0-9_.-]", "-").Trim("-")
    if ([string]::IsNullOrWhiteSpace($safe)) {
        return "auto"
    }
    return $safe
}

function Normalize-RequestedLaneName {
    param(
        [string]$Value
    )

    if ($Value -eq "auto") {
        return "auto"
    }

    $safe = ConvertTo-SafeLaneName $Value
    return ($safe -replace "-\d{14}$", "")
}

function Get-StableCommandHash {
    param(
        [string]$Value
    )

    $bytes = [System.Text.Encoding]::UTF8.GetBytes($Value)
    $sha1 = [System.Security.Cryptography.SHA1]::Create()
    try {
        $hash = $sha1.ComputeHash($bytes)
    }
    finally {
        $sha1.Dispose()
    }
    return ([System.BitConverter]::ToString($hash) -replace "-", "").Substring(0, 8).ToLowerInvariant()
}

function Get-AffinityLaneBase {
    param(
        [string[]]$CommandArgs
    )

    $signature = ([string]::Join(" ", @($CommandArgs))).Trim()
    $isRelease = $signature -match "(?:^|\s)(?:--release|-r|--profile(?:=|\s+)release)(?:\s|$)"
    if ($signature -match "(?:^|\s)--package(?:=|\s+)([A-Za-z0-9_.-]+)(?:\s|$)") {
        $base = ConvertTo-SafeLaneName $Matches[1]
        if ($isRelease) {
            return "$base-release"
        }
        return $base
    }
    if ($signature -match "(?:^|\s)-p\s+([A-Za-z0-9_.-]+)(?:\s|$)") {
        $base = ConvertTo-SafeLaneName $Matches[1]
        if ($isRelease) {
            return "$base-release"
        }
        return $base
    }

    if ($CommandArgs.Count -gt 0 -and -not [string]::IsNullOrWhiteSpace($CommandArgs[0])) {
        $program = [System.IO.Path]::GetFileNameWithoutExtension($CommandArgs[0])
        $base = ConvertTo-SafeLaneName "$program-$(Get-StableCommandHash $signature)"
        if ($isRelease) {
            return "$base-release"
        }
        return $base
    }

    return "auto"
}

function Get-ActiveCargoLaneNames {
    param(
        [string]$LanesRoot
    )

    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_CARGO_LANE_ACTIVE_NAMES)) {
        return @($env:CODEX_CARGO_LANE_ACTIVE_NAMES -split "[,;\s]+" | Where-Object {
                -not [string]::IsNullOrWhiteSpace($_)
            } | ForEach-Object { ConvertTo-SafeLaneName $_ })
    }

    $names = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    if (Test-Path -LiteralPath $LanesRoot -PathType Container) {
        foreach ($lane in @(Get-ChildItem -LiteralPath $LanesRoot -Directory -ErrorAction SilentlyContinue)) {
            if ((Test-CargoLockBusy -TargetDir $lane.FullName) -or (Test-LaneActiveLockBusy -TargetDir $lane.FullName)) {
                [void]$names.Add((ConvertTo-SafeLaneName $lane.Name))
            }
        }
    }

    if ($names.Count -gt 0 -or $env:CODEX_CARGO_LANE_WMI_FALLBACK -ne "1") {
        return @($names)
    }

    $lines = @()
    try {
        $lines = @(Get-CimInstance Win32_Process -ErrorAction Stop |
            Where-Object { $_.Name -match "^(cargo(?:-[A-Za-z0-9_.-]+)?|clippy-driver|rustc|rustup|just|powershell|pwsh)(\.exe)?$" } |
            Where-Object { $_.ProcessId -ne $PID } |
            ForEach-Object { [string]$_.CommandLine })
    }
    catch {
        return @()
    }

    # Keep these lane forms in sync with scripts/rust_build_status.py; that
    # Python tool is the canonical build-health/prune implementation.
    $lanePathPattern = "target[\\/]+lanes[\\/]+([A-Za-z0-9_.-]+)"
    $justLanePattern = "\b(?:test-lane(?:-fast)?|cargo-lane(?:-(?:home|isolated-home))?|test-lane-package|check-lane|clippy-lane|watch-lane|coverage-lane|fix-lane)\s+([A-Za-z0-9_.-]+)\b"
    $justFixedLanePattern = "\b(test-lane-main|cargo-lane-main|release-lane)\b"
    $justFixedLaneNames = @{
        "test-lane-main" = "main"
        "cargo-lane-main" = "main"
        "release-lane" = "release"
    }
    $scriptLanePattern = "(?:^|\s)-Lane\s+([A-Za-z0-9_.-]+)(?:\s|$)"

    foreach ($line in $lines) {
        foreach ($match in [regex]::Matches($line, $lanePathPattern)) {
            [void]$names.Add((ConvertTo-SafeLaneName $match.Groups[1].Value))
        }
        foreach ($match in [regex]::Matches($line, $justLanePattern)) {
            [void]$names.Add((ConvertTo-SafeLaneName $match.Groups[1].Value))
        }
        foreach ($match in [regex]::Matches($line, $justFixedLanePattern)) {
            [void]$names.Add((ConvertTo-SafeLaneName $justFixedLaneNames[$match.Groups[1].Value]))
        }
        foreach ($match in [regex]::Matches($line, $scriptLanePattern)) {
            [void]$names.Add((ConvertTo-SafeLaneName $match.Groups[1].Value))
        }
    }

    return @($names)
}

function Test-CargoLockBusy {
    param(
        [string]$TargetDir
    )

    return Test-ExclusiveLaneFileBusy -TargetDir $TargetDir -LockFileName ".cargo-lock"
}

function Test-LaneActiveLockBusy {
    param(
        [string]$TargetDir
    )

    return Test-ExclusiveLaneFileBusy -TargetDir $TargetDir -LockFileName ".lane-active.lock"
}

function Test-ExclusiveLaneFileBusy {
    param(
        [string]$TargetDir,
        [string]$LockFileName
    )

    $lockPath = Join-Path $TargetDir $LockFileName
    if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
        return $false
    }

    $stream = $null
    try {
        $stream = [System.IO.File]::Open(
            $lockPath,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            [System.IO.FileShare]::None
        )
        return $false
    }
    catch [System.IO.IOException] {
        return $true
    }
    finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
    }
}

function Get-EnvIntValue {
    param(
        [string]$Name,
        [int]$DefaultValue,
        [int]$MinimumValue
    )

    $raw = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($raw)) {
        return $DefaultValue
    }

    $parsed = 0
    if (-not [int]::TryParse($raw, [ref]$parsed)) {
        return $DefaultValue
    }
    if ($parsed -lt $MinimumValue) {
        return $MinimumValue
    }
    return $parsed
}

function Get-EnvInt64Value {
    param(
        [string]$Name,
        [int64]$DefaultValue,
        [int64]$MinimumValue
    )

    $raw = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($raw)) {
        return $DefaultValue
    }

    $parsed = [int64]0
    if (-not [int64]::TryParse($raw, [ref]$parsed)) {
        return $DefaultValue
    }
    if ($parsed -lt $MinimumValue) {
        return $MinimumValue
    }
    return $parsed
}

function Get-PowerShellExecutable {
    $command = Get-Command pwsh -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }
    $command = Get-Command powershell -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }
    return $null
}

function Write-CargoLaneTrashCleanupLog {
    param(
        [string]$LogPath,
        [string]$Message
    )

    try {
        $timestamp = [DateTime]::UtcNow.ToString("o", [Globalization.CultureInfo]::InvariantCulture)
        Add-Content -LiteralPath $LogPath -Encoding UTF8 -Value "[$timestamp] $Message" -ErrorAction SilentlyContinue
    }
    catch {
        return
    }
}

function Start-CargoLaneTrashCleanup {
    param(
        [string]$LanesRoot
    )

    if ($env:CODEX_CARGO_LANE_DISABLE_BACKGROUND_DELETE -eq "1") {
        return
    }
    if (-not (Test-Path -LiteralPath $LanesRoot -PathType Container)) {
        return
    }

    $firstTrash = @(Get-ChildItem -LiteralPath $LanesRoot -Directory -Filter "*.trash-*" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -match "\.trash-\d{17}$" } |
        Select-Object -First 1)
    if ($firstTrash.Count -eq 0) {
        return
    }

    $shell = Get-PowerShellExecutable
    $logPath = Join-Path $LanesRoot ".trash-cleanup.log"
    if ([string]::IsNullOrWhiteSpace($shell)) {
        Write-CargoLaneTrashCleanupLog -LogPath $logPath -Message "failed to start trash cleanup worker: PowerShell executable not found"
        return
    }

    $workerPath = Join-Path $PSScriptRoot "cargo-lane-trash-cleanup.ps1"
    if (-not (Test-Path -LiteralPath $workerPath -PathType Leaf)) {
        Write-CargoLaneTrashCleanupLog -LogPath $logPath -Message "failed to start trash cleanup worker: worker script not found"
        return
    }

    try {
        # Start-Process joins -ArgumentList with spaces without quoting under
        # Windows PowerShell 5.1, so paths containing spaces must be quoted
        # explicitly or the worker's parameters are split and never bind.
        Start-Process -FilePath $shell -WindowStyle Hidden -ArgumentList @(
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            ('"{0}"' -f $workerPath),
            "-LanesRoot",
            ('"{0}"' -f $LanesRoot)
        ) | Out-Null
    }
    catch {
        Write-CargoLaneTrashCleanupLog -LogPath $logPath -Message ("failed to start trash cleanup worker: {0}" -f $_.Exception.Message)
    }
}
function Invoke-CargoLanePrune {
    param(
        [string]$RepoRoot,
        [string]$LanesRoot,
        [string[]]$ActiveNames,
        [string[]]$ExcludedNames = @()
    )

    if (-not (Test-Path -LiteralPath $LanesRoot -PathType Container)) {
        return
    }

    Start-CargoLaneTrashCleanup -LanesRoot $LanesRoot

    $intervalHours = Get-EnvIntValue -Name "CODEX_CARGO_LANE_GC_INTERVAL_HOURS" -DefaultValue 1 -MinimumValue 0
    $stampPath = Join-Path $LanesRoot ".gc-stamp"
    if ($intervalHours -gt 0 -and (Test-Path -LiteralPath $stampPath -PathType Leaf)) {
        $stampAge = (Get-Date) - (Get-Item -LiteralPath $stampPath).LastWriteTime
        if ($stampAge.TotalHours -lt $intervalHours) {
            return
        }
    }

    $maxAgeDays = Get-EnvIntValue -Name "CODEX_CARGO_LANE_MAX_AGE_DAYS" -DefaultValue 7 -MinimumValue 1
    $maxLaneBytes = Get-EnvInt64Value -Name "CODEX_CARGO_LANE_MAX_LANE_BYTES" -DefaultValue 0 -MinimumValue 0
    $maxLaneArgs = @()
    if ($maxLaneBytes -gt 0) {
        $maxLaneArgs = @("--max-lane-bytes", ([string]$maxLaneBytes))
    }

    $active = [System.Collections.Generic.List[string]]::new()
    foreach ($name in @($ActiveNames + $ExcludedNames)) {
        if (-not [string]::IsNullOrWhiteSpace($name)) {
            [void]$active.Add($name)
        }
    }

    $previousActiveNames = $env:CODEX_CARGO_LANE_ACTIVE_NAMES
    $previousLanesRoot = $env:CODEX_CARGO_LANES_ROOT
    $env:CODEX_CARGO_LANE_ACTIVE_NAMES = ($active | Select-Object -Unique) -join ";"
    $env:CODEX_CARGO_LANES_ROOT = $LanesRoot
    try {
        $scriptPath = Join-Path $RepoRoot "scripts\rust_build_status.py"
        & python $scriptPath prune --skip-disk-report --keep-warm-per-base 1 --max-age-days $maxAgeDays @maxLaneArgs | Out-Null
    }
    finally {
        $env:CODEX_CARGO_LANE_ACTIVE_NAMES = $previousActiveNames
        $env:CODEX_CARGO_LANES_ROOT = $previousLanesRoot
        [IO.File]::WriteAllText($stampPath, (Get-Date).ToUniversalTime().ToString("o"))
    }
}

function Resolve-CargoLaneName {
    param(
        [string]$RequestedLane,
        [string[]]$CommandArgs,
        [string]$LanesRoot,
        [string[]]$ActiveNames = @()
    )

    if ($RequestedLane -ne "auto") {
        return $RequestedLane
    }

    $baseLane = Get-AffinityLaneBase -CommandArgs $CommandArgs
    $active = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($name in @($ActiveNames)) {
        [void]$active.Add($name)
    }

    $warmLanes = @()
    if (Test-Path -LiteralPath $LanesRoot -PathType Container) {
        $warmLanes = @(Get-ChildItem -LiteralPath $LanesRoot -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -eq $baseLane -or $_.Name -match "^$([regex]::Escape($baseLane))-\d+$" } |
            Sort-Object -Property LastWriteTimeUtc -Descending)
    }

    foreach ($lane in $warmLanes) {
        if (-not $active.Contains($lane.Name)) {
            return $lane.Name
        }
    }

    if (-not $active.Contains($baseLane)) {
        return $baseLane
    }

    for ($i = 2; $i -le 64; $i++) {
        $candidate = "$baseLane-$i"
        if (-not $active.Contains($candidate)) {
            return $candidate
        }
    }

    throw "Could not find an idle cargo lane for '$baseLane'."
}

function Acquire-CargoLaneReservation {
    param(
        [string]$LaneRoot,
        [string]$BaseLane,
        [string[]]$ActiveNames = @()
    )

    $active = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($name in @($ActiveNames)) {
        if (-not [string]::IsNullOrWhiteSpace($name)) {
            [void]$active.Add($name)
        }
    }

    for ($i = 0; $i -le 64; $i++) {
        $candidate = if ($i -eq 0) { $BaseLane } else { "$BaseLane-$($i + 1)" }
        if ($active.Contains($candidate)) {
            continue
        }
        $target = Join-Path $LaneRoot $candidate
        New-Item -ItemType Directory -Force -Path $target | Out-Null
        $lockPath = Join-Path $target ".lane-active.lock"
        $stream = $null
        try {
            $stream = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            $stream.SetLength(0)
            $lockText = "pid=$PID`nlane=$candidate`nstarted=$([DateTime]::UtcNow.ToString("o", [Globalization.CultureInfo]::InvariantCulture))`n"
            $bytes = [Text.Encoding]::UTF8.GetBytes($lockText)
            $stream.Write($bytes, 0, $bytes.Length)
            $stream.Flush()
            return [pscustomobject]@{
                Lane = $candidate
                TargetDir = $target
                Stream = $stream
            }
        }
        catch {
            if ($null -ne $stream) {
                $stream.Dispose()
            }
        }
    }

    throw "Unable to reserve an idle Cargo lane for '$BaseLane'."
}

function Add-PathPrefix {
    param(
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return
    }

    $existing = @($env:PATH -split [System.IO.Path]::PathSeparator)
    if ($existing -notcontains $Path) {
        $env:PATH = "$Path$([System.IO.Path]::PathSeparator)$env:PATH"
    }
}

function Copy-UserCargoConfig {
    param(
        [string]$CargoHome
    )

    $configPath = Join-Path $CargoHome "config.toml"
    if (Test-Path -LiteralPath $configPath -PathType Leaf) {
        return $configPath
    }
    if ([string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        return $configPath
    }

    $sourceConfig = Join-Path $env:USERPROFILE ".cargo\config.toml"
    if (Test-Path -LiteralPath $sourceConfig -PathType Leaf) {
        Copy-Item -LiteralPath $sourceConfig -Destination $configPath -Force
    }
    return $configPath
}

function Set-CargoConfigRustcWrapper {
    param(
        [string]$ConfigPath
    )

    $lines = @()
    if (Test-Path -LiteralPath $ConfigPath -PathType Leaf) {
        $lines = @([IO.File]::ReadAllLines($ConfigPath))
    }

    if (@($lines | Where-Object { $_ -match "^\s*rustc-wrapper\s*=" }).Count -gt 0) {
        return
    }

    $buildIndex = -1
    for ($i = 0; $i -lt $lines.Count; $i++) {
        if ($lines[$i] -match "^\s*\[build\]\s*$") {
            $buildIndex = $i
            break
        }
    }

    if ($buildIndex -ge 0) {
        $updated = [System.Collections.Generic.List[string]]::new()
        foreach ($line in $lines) {
            [void]$updated.Add($line)
            if ($updated.Count -eq ($buildIndex + 1)) {
                [void]$updated.Add('rustc-wrapper = "sccache"')
            }
        }
        [IO.File]::WriteAllLines($ConfigPath, [string[]]$updated)
        return
    }

    $updatedLines = @($lines)
    if ($updatedLines.Count -gt 0 -and -not [string]::IsNullOrWhiteSpace($updatedLines[-1])) {
        $updatedLines += ""
    }
    $updatedLines += "[build]"
    $updatedLines += 'rustc-wrapper = "sccache"'
    [IO.File]::WriteAllLines($ConfigPath, [string[]]$updatedLines)
}

function Enable-SccacheForCargoHome {
    param(
        [string]$CargoHome,
        [string]$RepoRoot
    )

    $configPath = Copy-UserCargoConfig -CargoHome $CargoHome

    if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
        return
    }

    Enable-SccacheEnvironment -RepoRoot $RepoRoot
    Set-CargoConfigRustcWrapper -ConfigPath $configPath
}

function Enable-SccacheForLane {
    param(
        [string]$RepoRoot
    )

    if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
        return
    }

    Enable-SccacheEnvironment -RepoRoot $RepoRoot
}

$repoRoot = Get-RepoRoot
$rustRoot = Join-Path $repoRoot "codex-rs"
if (-not [string]::IsNullOrWhiteSpace($LanesRoot)) {
    $cargoLanesRoot = [System.IO.Path]::GetFullPath($LanesRoot)
}
elseif (-not [string]::IsNullOrWhiteSpace($env:CODEX_CARGO_LANES_ROOT)) {
    $cargoLanesRoot = [System.IO.Path]::GetFullPath($env:CODEX_CARGO_LANES_ROOT)
}
else {
    $cargoLanesRoot = Join-Path $rustRoot "target\lanes"
}
$commandArgs = @($Command)
if ($commandArgs.Count -eq 1 -and [string]::IsNullOrWhiteSpace($commandArgs[0])) {
    $commandArgs = @()
}

$requestedLane = Normalize-RequestedLaneName $Lane
$activeLaneNames = @(Get-ActiveCargoLaneNames -LanesRoot $cargoLanesRoot)
$candidateLane = Resolve-CargoLaneName -RequestedLane $requestedLane -CommandArgs $commandArgs -LanesRoot $cargoLanesRoot -ActiveNames $activeLaneNames
$reservation = Acquire-CargoLaneReservation -LaneRoot $cargoLanesRoot -BaseLane $candidateLane -ActiveNames $activeLaneNames
$resolvedLane = $reservation.Lane
$targetDir = $reservation.TargetDir
[IO.File]::WriteAllText(
    (Join-Path $targetDir ".lane-last-used"),
    (Get-Date).ToUniversalTime().ToString("o")
)
Invoke-CargoLanePrune -RepoRoot $repoRoot -LanesRoot $cargoLanesRoot -ActiveNames $activeLaneNames -ExcludedNames @($resolvedLane)

if ([string]::IsNullOrWhiteSpace($env:RUST_MIN_STACK)) {
    $env:RUST_MIN_STACK = "8388608"
}

Enable-SccacheForLane -RepoRoot $repoRoot
Set-CodexRustMsvcLinkerEnvironment

if ($IsolateCargoHome) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        throw "LOCALAPPDATA is not set. Pass a normal lane without -IsolateCargoHome."
    }

    $cargoHome = Join-Path $env:LOCALAPPDATA "cargo-lanes\codexKD\$resolvedLane"
    New-Item -ItemType Directory -Force -Path $cargoHome | Out-Null
    $env:CARGO_HOME = $cargoHome

    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        Add-PathPrefix (Join-Path $env:USERPROFILE ".cargo\bin")
    }
    Enable-SccacheForCargoHome -CargoHome $cargoHome -RepoRoot $repoRoot
}

Push-Location $rustRoot
try {
    if ($Fetch) {
        cargo fetch --locked
        if ($LASTEXITCODE -ne 0) {
            exit $LASTEXITCODE
        }
    }

    if ($commandArgs.Count -eq 0) {
        Write-Output "LANE=$resolvedLane"
        Write-Output "TARGET_DIR=$targetDir"
        Write-Output "RUST_MIN_STACK=$env:RUST_MIN_STACK"
        if (-not [string]::IsNullOrWhiteSpace($env:RUSTC_WRAPPER)) {
            Write-Output "RUSTC_WRAPPER=$env:RUSTC_WRAPPER"
        }
        if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
            Write-Output "CARGO_HOME=$env:CARGO_HOME"
        }
        Write-Output "Example: just test-lane-package codex-core"
        Write-Output "Example: just check-lane codex-core"
        Write-Output "Direct example: .\scripts\cargo-lane.ps1 -Lane auto --% cargo nextest run -p codex-core"
        exit 0
    }

    # An exported CARGO_TARGET_DIR lands in sccache's cache key even when
    # cargo itself uses --target-dir, so never export the lane and drop any
    # inherited value; cargo commands receive the lane as an argument instead.
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    $commandArgs = @(Add-CargoTargetDirArgument -CommandArgs $commandArgs -TargetDir $targetDir)
    $program = $commandArgs[0]
    $arguments = @($commandArgs | Select-Object -Skip 1)
    & $program @arguments
    if ($null -eq $LASTEXITCODE) {
        if ($?) {
            exit 0
        }
        exit 1
    }
    exit $LASTEXITCODE
}
finally {
    if ($null -ne $reservation -and $null -ne $reservation.Stream) {
        $reservation.Stream.Dispose()
    }
    Pop-Location
}
