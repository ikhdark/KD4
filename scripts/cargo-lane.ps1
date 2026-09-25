Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$CargoLanesRootMarkerName = ".codex-cargo-lanes-root"
$CargoLanesRootMarkerContent = "codex-kd cargo lanes root v1"

. (Join-Path $PSScriptRoot "common-rust-env.ps1")
. (Join-Path $PSScriptRoot "cargo-lane-patterns.ps1")

function Test-CargoLaneCommandToken {
    param(
        [string]$Value
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    if ($Value -match "[\\/]") {
        return $true
    }

    $leaf = [System.IO.Path]::GetFileNameWithoutExtension($Value)
    return $leaf -in @("cargo", "just", "rustup", "powershell", "pwsh")
}

# Mirror rust_build_status._safe_lane_name for every lane that reaches the
# filesystem, explicit or derived. The character class is case-sensitive:
# PowerShell's default matching admits non-ASCII letters such as U+212A.
function Assert-CargoLaneName {
    param(
        [string]$Lane
    )

    if ($Lane -cnotmatch "^[A-Za-z0-9_.-]+\z") {
        throw "Lane '$Lane' contains unsupported characters."
    }
    if ($Lane -match "\.trash-\d{17}$") {
        throw "Lane names ending in a trash timestamp are reserved for cleanup."
    }
    if ($Lane -match "^\.+$") {
        # Pure-dot names pass the character filter but Windows path
        # normalization can collapse them to a parent directory, escaping lane
        # isolation.
        throw "Lane '$Lane' is not a valid lane name."
    }
}

function Parse-CargoLaneArguments {
    param(
        [object[]]$RawArgs
    )

    $parsedLane = $null
    $parsedLanesRoot = $null
    $parsedIsolateCargoHome = $false
    $parsedFetch = $false
    $maintenanceOnly = $false
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
            if ($parsedLane.StartsWith("-", [StringComparison]::Ordinal)) {
                throw "-Lane requires a value that does not start with '-'."
            }
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
        if ($arg -eq "-MaintenanceOnly") {
            $maintenanceOnly = $true
            continue
        }
        if ($null -eq $parsedLane -and -not $arg.StartsWith("-", [StringComparison]::Ordinal)) {
            if (Test-CargoLaneCommandToken -Value $arg) {
                throw "First positional argument '$arg' looks like a command. Pass -Lane <name> before the command."
            }
            $parsedLane = $arg
            continue
        }
        $commandStart = $i
        break
    }

    if ([string]::IsNullOrWhiteSpace($parsedLane)) {
        throw "-Lane is required."
    }
    if ($parsedLane.StartsWith("-", [StringComparison]::Ordinal)) {
        throw "-Lane requires a value that does not start with '-'."
    }
    Assert-CargoLaneName -Lane $parsedLane

    return [pscustomobject]@{
        Lane = $parsedLane
        LanesRoot = $parsedLanesRoot
        IsolateCargoHome = $parsedIsolateCargoHome
        Fetch = $parsedFetch
        MaintenanceOnly = $maintenanceOnly
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

function Get-NormalizedCargoLanePath {
    param(
        [string]$Path
    )

    $trimChars = [char[]]@(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    return [System.IO.Path]::GetFullPath($Path).TrimEnd($trimChars)
}

function Test-CargoLanesRootMarker {
    param(
        [string]$LanesRoot
    )

    $markerPath = Join-Path $LanesRoot $CargoLanesRootMarkerName
    if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf)) {
        return $false
    }
    try {
        $marker = Get-Item -LiteralPath $markerPath -Force
        if (($marker.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            return $false
        }
        return [IO.File]::ReadAllText($markerPath).Trim() -ceq $CargoLanesRootMarkerContent
    }
    catch {
        return $false
    }
}

function Test-CargoLanesRootReparsePoint {
    param(
        [string]$LanesRoot
    )

    $component = $LanesRoot
    while (-not [string]::IsNullOrEmpty($component)) {
        if (Test-Path -LiteralPath $component) {
            $rootItem = Get-Item -LiteralPath $component -Force -ErrorAction Stop
            if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                return $true
            }
        }
        $component = [IO.Path]::GetDirectoryName($component)
    }
    return $false
}

function Test-CargoLanesRootForPrune {
    param(
        [string]$RepoRoot,
        [string]$LanesRoot
    )

    if (-not (Test-Path -LiteralPath $LanesRoot -PathType Container)) {
        return $false
    }
    if (Test-CargoLanesRootReparsePoint -LanesRoot $LanesRoot) {
        return $false
    }
    $defaultLanesRoot = Join-Path $RepoRoot "codex-rs\target\lanes"
    if ((Get-NormalizedCargoLanePath $LanesRoot) -ieq (Get-NormalizedCargoLanePath $defaultLanesRoot)) {
        return $true
    }
    return Test-CargoLanesRootMarker -LanesRoot $LanesRoot
}

function Initialize-CargoLanesRoot {
    param(
        [string]$RepoRoot,
        [string]$LanesRoot
    )

    if (Test-CargoLanesRootReparsePoint -LanesRoot $LanesRoot) {
        throw "Cargo lanes root must not be a reparse point or junction: $LanesRoot"
    }

    if (Test-CargoLanesRootMarker -LanesRoot $LanesRoot) {
        return
    }

    $defaultLanesRoot = Join-Path $RepoRoot "codex-rs\target\lanes"
    $isDefaultRoot = (Get-NormalizedCargoLanePath $LanesRoot) -ieq (Get-NormalizedCargoLanePath $defaultLanesRoot)
    if (-not (Test-Path -LiteralPath $LanesRoot -PathType Container)) {
        New-Item -ItemType Directory -Force -Path $LanesRoot | Out-Null
    }
    elseif (-not $isDefaultRoot) {
        $existingEntries = @(Get-ChildItem -LiteralPath $LanesRoot -Force -ErrorAction Stop)
        if ($existingEntries.Count -gt 0) {
            return
        }
    }

    $markerPath = Join-Path $LanesRoot $CargoLanesRootMarkerName
    [IO.File]::WriteAllText($markerPath, "$CargoLanesRootMarkerContent`n")
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

    if (-not (Test-Path Env:RUSTC_WRAPPER)) {
        $env:RUSTC_WRAPPER = "sccache"
        Set-CodexRustSccacheEnvironment -RepoRoot $RepoRoot
    }
    elseif (Test-SccacheWrapper -Value $env:RUSTC_WRAPPER) {
        Set-CodexRustSccacheEnvironment -RepoRoot $RepoRoot
    }
}

function ConvertTo-SafeLaneName {
    param(
        [string]$Value
    )

    $safe = ([string]$Value -creplace "[^A-Za-z0-9_.-]", "-").Trim("-")
    if ([string]::IsNullOrWhiteSpace($safe)) {
        return "auto"
    }
    return $safe
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
    # Include package selectors inside Cargo watch's --exec/-x command strings.
    $packages = @(Get-CodexCargoPackageSpecs -CommandArgs ($signature -split "\s+"))
    if ($packages.Count -gt 0) {
        $base = ConvertTo-SafeLaneName $packages[0]
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
            if (($lane.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
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

    foreach ($name in @(Get-CargoLaneNamesFromCommandLines -CommandLines $lines)) {
        [void]$names.Add((ConvertTo-SafeLaneName $name))
    }

    return @($names)
}

function Test-CargoLockBusy {
    param(
        [string]$TargetDir
    )

    if (-not (Test-Path -LiteralPath $TargetDir -PathType Container)) {
        return $false
    }
    try {
        foreach ($child in Get-ChildItem -LiteralPath $TargetDir -Directory -Force -ErrorAction Stop) {
            if (($child.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $true }
            if (Test-ExclusiveLaneFileBusy -TargetDir $child.FullName -LockFileName ".cargo-lock") { return $true }
            foreach ($profile in Get-ChildItem -LiteralPath $child.FullName -Directory -Force -ErrorAction Stop) {
                if (($profile.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { return $true }
                if (Test-ExclusiveLaneFileBusy -TargetDir $profile.FullName -LockFileName ".cargo-lock") { return $true }
            }
        }
    }
    catch { return $true }
    return $false
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
        # Probe like rust_build_status.py: share the handle and test the
        # byte-range lock. An exclusive open makes a Cargo build that is just
        # starting in this lane fail with a sharing violation (os error 32).
        $stream = [System.IO.File]::Open(
            $lockPath,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::ReadWrite,
            ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete)
        )
        $stream.Lock(0, 1)
        $stream.Unlock(0, 1)
        return $false
    }
    catch [System.IO.IOException] {
        if (Test-IsCargoLaneLockContention -Exception $_.Exception) {
            return $true
        }
        throw
    }
    finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
    }
}

function Test-IsCargoLaneLockContention {
    param(
        [System.IO.IOException]$Exception
    )

    $nativeCode = $Exception.HResult -band 0xFFFF
    return $nativeCode -eq 32 -or $nativeCode -eq 33
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
        [string]$LanesRoot,
        [switch]$Prune
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
    if (-not $Prune -and $firstTrash.Count -eq 0) {
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
        $workerArgs = @(
            "-NoLogo",
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            ('"{0}"' -f $workerPath),
            "-LanesRoot",
            ('"{0}"' -f $LanesRoot)
        )
        if ($Prune) { $workerArgs += "-Prune" }
        Start-Process -FilePath $shell -WindowStyle Hidden -ArgumentList $workerArgs | Out-Null
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
        [string[]]$ExcludedNames = @(),
        [switch]$Force
    )

    if (-not (Test-Path -LiteralPath $LanesRoot -PathType Container)) {
        return
    }
    if (-not (Test-CargoLanesRootForPrune -RepoRoot $RepoRoot -LanesRoot $LanesRoot)) {
        Write-Warning "Skipping Cargo lane pruning for unrecognized lanes root '$LanesRoot'; custom roots must be new, empty, or contain $CargoLanesRootMarkerName."
        return
    }

    Start-CargoLaneTrashCleanup -LanesRoot $LanesRoot

    $intervalHours = Get-EnvIntValue -Name "CODEX_CARGO_LANE_GC_INTERVAL_HOURS" -DefaultValue 1 -MinimumValue 0
    $stampPath = Join-Path $LanesRoot ".gc-stamp"
    if (-not $Force -and $intervalHours -gt 0 -and (Test-Path -LiteralPath $stampPath -PathType Leaf)) {
        $stampAge = (Get-Date) - (Get-Item -LiteralPath $stampPath).LastWriteTime
        if ($stampAge.TotalHours -lt $intervalHours) {
            return
        }
    }

    # Share the Python entrypoint's nonblocking maintenance lock. Keep the
    # coordination lock free while disk accounting scans the target tree.
    $gcStream = $null
    try {
        try {
            $gcStream = [IO.File]::Open((Join-Path $LanesRoot ".lane-gc.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        }
        catch [IO.IOException] {
            if (Test-IsCargoLaneLockContention -Exception $_.Exception) { return }
            throw
        }
        if (-not $Force -and $intervalHours -gt 0 -and (Test-Path -LiteralPath $stampPath -PathType Leaf)) {
            $stampAge = (Get-Date) - (Get-Item -LiteralPath $stampPath).LastWriteTime
            if ($stampAge.TotalHours -lt $intervalHours) { return }
        }
        $retryStampPath = Join-Path $LanesRoot ".gc-retry"
        if (-not $Force -and (Test-Path -LiteralPath $retryStampPath -PathType Leaf)) {
            $retryAge = (Get-Date) - (Get-Item -LiteralPath $retryStampPath).LastWriteTime
            if ($retryAge.TotalSeconds -lt 60) { return }
        }

        $maxAgeDays = Get-EnvIntValue -Name "CODEX_CARGO_LANE_MAX_AGE_DAYS" -DefaultValue 7 -MinimumValue 1
        $maxLaneBytes = Get-EnvInt64Value -Name "CODEX_CARGO_LANE_MAX_LANE_BYTES" -DefaultValue 0 -MinimumValue 0
        # Recursive disk accounting is opt-in on the build path. Use target-prune
        # with aggregate limits for explicit disk maintenance.
        $maxTotalLaneBytes = Get-EnvInt64Value -Name "CODEX_CARGO_LANE_MAX_TOTAL_BYTES" -DefaultValue 0 -MinimumValue 0
        $maxTotalTargetBytes = Get-EnvInt64Value -Name "CODEX_CARGO_TARGET_MAX_TOTAL_BYTES" -DefaultValue 0 -MinimumValue 0
        $maxLaneArgs = @()
        if ($maxLaneBytes -gt 0) {
            $maxLaneArgs = @("--max-lane-bytes", ([string]$maxLaneBytes))
        }
        $maxTotalLaneArgs = @()
        if ($maxTotalLaneBytes -gt 0) {
            $maxTotalLaneArgs = @("--max-total-lane-bytes", ([string]$maxTotalLaneBytes))
        }
        $maxTotalTargetArgs = @()
        if ($maxTotalTargetBytes -gt 0) {
            $maxTotalTargetArgs = @("--max-total-target-bytes", ([string]$maxTotalTargetBytes))
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
        $pruneSucceeded = $false
        $pruneFailure = $null
        try {
            $scriptPath = Join-Path $RepoRoot "scripts\rust_build_status.py"
            $python = Get-Command python -ErrorAction SilentlyContinue
            if ($null -eq $python) {
                $pruneFailure = "python executable was not found"
            }
            else {
                $global:LASTEXITCODE = $null
                & $python.Source $scriptPath prune --skip-disk-report --keep-warm-per-base 1 --max-age-days $maxAgeDays @maxLaneArgs @maxTotalLaneArgs @maxTotalTargetArgs | Out-Null
                $pruneSucceeded = $LASTEXITCODE -eq 0
                if (-not $pruneSucceeded) {
                    $pruneFailure = "prune command exited with code $LASTEXITCODE"
                }
            }
        }
        catch {
            $pruneFailure = $_.Exception.Message
        }
        finally {
            $env:CODEX_CARGO_LANE_ACTIVE_NAMES = $previousActiveNames
            $env:CODEX_CARGO_LANES_ROOT = $previousLanesRoot
        }
        if ($pruneSucceeded) {
            [IO.File]::WriteAllText($stampPath, (Get-Date).ToUniversalTime().ToString("o"))
            if (Test-Path -LiteralPath $retryStampPath) {
                Remove-Item -LiteralPath $retryStampPath -Force
            }
        }
        else {
            [IO.File]::WriteAllText($retryStampPath, (Get-Date).ToUniversalTime().ToString("o"))
            $detail = if ([string]::IsNullOrWhiteSpace($pruneFailure)) { "" } else { " ($pruneFailure)" }
            Write-Warning "Cargo lane pruning failed$detail; leaving the GC stamp unchanged; maintenance can retry after 60 seconds."
        }
        # Pruning may leave a uniquely renamed tree when Windows still has a file
        # open. Start the deferred worker again after the rename phase.
        Start-CargoLaneTrashCleanup -LanesRoot $LanesRoot
    }
    finally {
        if ($null -ne $gcStream) { $gcStream.Dispose() }
    }
}

function Get-CargoLaneLastUsed {
    param([System.IO.DirectoryInfo]$Lane)

    $stamp = Join-Path $Lane.FullName ".lane-last-used"
    if (Test-Path -LiteralPath $stamp -PathType Leaf) {
        return (Get-Item -LiteralPath $stamp -Force).LastWriteTimeUtc
    }
    $newest = $Lane.LastWriteTimeUtc
    foreach ($child in Get-ChildItem -LiteralPath $Lane.FullName -Force -ErrorAction SilentlyContinue) {
        if ($child.LastWriteTimeUtc -gt $newest) { $newest = $child.LastWriteTimeUtc }
    }
    return $newest
}

function Acquire-CargoLaneReservation {
    param(
        [string]$LaneRoot,
        [string]$BaseLane,
        [switch]$PreferWarm,
        [string[]]$ActiveNames = @()
    )

    $coordinationPath = Join-Path $LaneRoot ".lane-coordination.lock"
    $coordinationStream = $null
    $coordinationDeadline = [DateTime]::UtcNow.AddSeconds(30)
    while ($null -eq $coordinationStream) {
        try {
            $coordinationStream = [IO.File]::Open(
                $coordinationPath,
                [IO.FileMode]::OpenOrCreate,
                [IO.FileAccess]::ReadWrite,
                [IO.FileShare]::None
            )
        }
        catch [IO.IOException] {
            if (-not (Test-IsCargoLaneLockContention -Exception $_.Exception)) {
                throw
            }
            if ([DateTime]::UtcNow -ge $coordinationDeadline) {
                throw "Timed out waiting for Cargo lane coordination lock."
            }
            Start-Sleep -Milliseconds 50
        }
    }

    try {
        $active = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
        foreach ($name in @($ActiveNames)) {
            if (-not [string]::IsNullOrWhiteSpace($name)) {
                [void]$active.Add($name)
            }
        }

        $candidates = @()
        if ($PreferWarm) {
            $candidates += @(Get-ChildItem -LiteralPath $LaneRoot -Directory -ErrorAction Stop |
                Where-Object { ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0 -and ($_.Name -eq $BaseLane -or $_.Name -match "^$([regex]::Escape($BaseLane))-\d+$") } |
                Sort-Object -Property @{ Expression = { Get-CargoLaneLastUsed -Lane $_ }; Descending = $true }, Name |
                ForEach-Object { $_.Name })
        }
        $candidates += @($BaseLane) + @(2..65 | ForEach-Object { "$BaseLane-$_" })
        foreach ($candidate in @($candidates | Select-Object -Unique)) {
            if ($active.Contains($candidate)) {
                continue
            }
            $target = Join-Path $LaneRoot $candidate
            if (Test-CargoLanesRootReparsePoint -LanesRoot $target) { continue }
            # rust_build_status.py quarantines a lane whose owned process tree
            # was not confirmed stopped; never hand that lane to new work.
            if (Test-Path -LiteralPath (Join-Path $target ".lane-cleanup-unconfirmed")) { continue }
            New-Item -ItemType Directory -Force -Path $target | Out-Null
            if (Test-CargoLanesRootReparsePoint -LanesRoot $target) { continue }
            # The earlier process/lock snapshot can be stale while waiting for
            # coordination. Recheck Cargo's profile locks before reservation.
            if (Test-CargoLockBusy -TargetDir $target) { continue }
            $lockPath = Join-Path $target ".lane-active.lock"
            $stream = $null
            try {
                # Exclusive but inheritable: the process_owner.py that runs
                # the command co-holds the reservation until its tree is gone.
                $stream = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::Inheritable)
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
            catch [IO.IOException] {
                if ($null -ne $stream) {
                    $stream.Dispose()
                }
                if (-not (Test-IsCargoLaneLockContention -Exception $_.Exception)) {
                    throw
                }
            }
            catch {
                if ($null -ne $stream) {
                    $stream.Dispose()
                }
                throw
            }
        }

        throw "Unable to reserve an idle Cargo lane for '$BaseLane'."
    }
    finally {
        if ($null -ne $coordinationStream) {
            $coordinationStream.Dispose()
        }
    }
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

function Enable-SccacheForCargoHome {
    param(
        [string]$CargoHome,
        [string]$RepoRoot
    )

    $null = Copy-UserCargoConfig -CargoHome $CargoHome

    if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
        return
    }

    Enable-SccacheEnvironment -RepoRoot $RepoRoot
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

function Get-CargoLaneOwnedCommand {
    param(
        [string]$TargetDir,
        [string[]]$CommandArgs
    )

    # Run lane work under scripts/process_owner.py, as rust_build_status.py
    # run-lane does: it stops the command's whole process tree when the
    # command ends or this host dies, and it inherits the reservation handle,
    # so the lane stays reserved until that tree is gone.
    $python = Get-Command python -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $python) {
        throw "Lane commands need python for scripts\process_owner.py, which owns their process tree."
    }
    $command = Get-Command -Name $CommandArgs[0] -CommandType Application, ExternalScript -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $command) {
        throw "Lane command '$($CommandArgs[0])' is not a program or script."
    }
    $owned = @($command.Source) + @($CommandArgs | Select-Object -Skip 1)
    if ($command.CommandType -eq [Management.Automation.CommandTypes]::ExternalScript) {
        $owned = @((Get-Process -Id $PID).Path, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File") + $owned
    }
    return @(
        $python.Source,
        (Join-Path $PSScriptRoot "process_owner.py"),
        "--parent-pid",
        [string]$PID,
        "--cleanup-failed-marker",
        (Join-Path $TargetDir ".lane-cleanup-unconfirmed"),
        "--"
    ) + $owned
}

function Update-CargoLaneLastUsed {
    param(
        [string]$TargetDir
    )

    [IO.File]::WriteAllText(
        (Join-Path $TargetDir ".lane-last-used"),
        [DateTime]::UtcNow.ToString("o", [Globalization.CultureInfo]::InvariantCulture)
    )
}

$repoRoot = Get-RepoRoot
$rustRoot = Join-Path $repoRoot "codex-rs"
if (-not [string]::IsNullOrWhiteSpace($LanesRoot)) {
    $cargoLanesRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LanesRoot)
}
elseif (-not [string]::IsNullOrWhiteSpace($env:CODEX_CARGO_LANES_ROOT)) {
    $cargoLanesRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($env:CODEX_CARGO_LANES_ROOT)
}
else {
    $cargoLanesRoot = Join-Path $rustRoot "target\lanes"
}
Initialize-CargoLanesRoot -RepoRoot $repoRoot -LanesRoot $cargoLanesRoot
if ($parsedArgs.MaintenanceOnly) {
    Invoke-CargoLanePrune -RepoRoot $repoRoot -LanesRoot $cargoLanesRoot -ActiveNames @()
    exit 0
}
$commandArgs = @($Command)
if ($commandArgs.Count -eq 1 -and [string]::IsNullOrWhiteSpace($commandArgs[0])) {
    $commandArgs = @()
}

# Use the validated name verbatim, as rust_build_status.py does, so a lane
# names the same directory from either entrypoint.
$requestedLane = $Lane
$activeLaneNames = @(Get-ActiveCargoLaneNames -LanesRoot $cargoLanesRoot)
$candidateLane = if ($requestedLane -ceq "auto") { Get-AffinityLaneBase -CommandArgs $commandArgs } else { $requestedLane }
# Affinity names come from command text; hold them to the explicit-name rules.
Assert-CargoLaneName -Lane $candidateLane
$previousLaneTargetDir = $env:CODEX_CARGO_LANE_TARGET_DIR
$didPushLocation = $false
$reservation = Acquire-CargoLaneReservation -LaneRoot $cargoLanesRoot -BaseLane $candidateLane -ActiveNames $activeLaneNames -PreferWarm:($requestedLane -ceq "auto")
try {
    $resolvedLane = $reservation.Lane
    $targetDir = $reservation.TargetDir
    Push-Location $rustRoot
    $didPushLocation = $true
    $commandArgs = @(Add-CargoTargetDirArgument -CommandArgs $commandArgs -TargetDir $targetDir)
    $env:CODEX_CARGO_LANE_TARGET_DIR = $targetDir
    if ($requestedLane -cne "auto" -and $resolvedLane -ne $requestedLane) {
        Write-Warning "Requested Cargo lane '$requestedLane' is busy; using '$resolvedLane'."
    }
    Update-CargoLaneLastUsed -TargetDir $targetDir
    try {
        if ($env:CODEX_CARGO_LANE_MAINTENANCE_SYNC -eq "1") {
            Invoke-CargoLanePrune -RepoRoot $repoRoot -LanesRoot $cargoLanesRoot -ActiveNames $activeLaneNames -ExcludedNames @($resolvedLane)
        }
        else {
            $stamp = Join-Path $cargoLanesRoot ".gc-stamp"
            $retry = Join-Path $cargoLanesRoot ".gc-retry"
            $interval = Get-EnvIntValue -Name "CODEX_CARGO_LANE_GC_INTERVAL_HOURS" -DefaultValue 1 -MinimumValue 0
            $due = -not (Test-Path -LiteralPath $stamp) -or ((Get-Date) - (Get-Item -LiteralPath $stamp).LastWriteTime).TotalHours -ge $interval
            $retryReady = -not (Test-Path -LiteralPath $retry) -or ((Get-Date) - (Get-Item -LiteralPath $retry).LastWriteTime).TotalSeconds -ge 60
            Start-CargoLaneTrashCleanup -LanesRoot $cargoLanesRoot -Prune:($due -and $retryReady)
        }
    }
    catch {
        Write-Warning "Cargo lane pruning failed unexpectedly ($($_.Exception.Message)); continuing without pruning."
    }

    if ([string]::IsNullOrWhiteSpace($env:RUST_MIN_STACK)) {
        $env:RUST_MIN_STACK = "8388608"
    }

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
    else {
        Enable-SccacheForLane -RepoRoot $repoRoot
    }

    if ($Fetch) {
        $fetch = @(Get-CargoLaneOwnedCommand -TargetDir $targetDir -CommandArgs @("cargo", "fetch"))
        & $fetch[0] @($fetch | Select-Object -Skip 1)
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
        if (-not [string]::IsNullOrWhiteSpace($env:CARGO_INCREMENTAL)) {
            Write-Output "CARGO_INCREMENTAL=$env:CARGO_INCREMENTAL"
        }
        if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
            Write-Output "CARGO_HOME=$env:CARGO_HOME"
        }
        Write-Output "Core test example: just core-test-lane core_lib"
        Write-Output "Generic package example: just test-lane-package codex-tui"
        Write-Output "Check example: just check-lane codex-core"
        exit 0
    }

    # An exported CARGO_TARGET_DIR lands in sccache's cache key even when
    # cargo itself uses --target-dir, so never export the lane and drop any
    # inherited value; cargo commands receive the lane as an argument instead.
    Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue
    # Invoke at script level so the command writes straight to this host's
    # output handles instead of being captured as function output.
    $owned = @(Get-CargoLaneOwnedCommand -TargetDir $targetDir -CommandArgs $commandArgs)
    $program = $owned[0]
    $arguments = @($owned | Select-Object -Skip 1)
    & $program @arguments
    exit $LASTEXITCODE
}
finally {
    try {
        Update-CargoLaneLastUsed -TargetDir $targetDir
    }
    finally {
        $env:CODEX_CARGO_LANE_TARGET_DIR = $previousLaneTargetDir
        if ($null -ne $reservation -and $null -ne $reservation.Stream) {
            $reservation.Stream.Dispose()
        }
        # Keep routine cleanup hourly-throttled; use target-prune when an
        # unusually large build requires immediate disk-budget enforcement.
        if ($didPushLocation) { Pop-Location }
    }
}
