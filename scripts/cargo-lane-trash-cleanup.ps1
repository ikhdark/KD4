[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$LanesRoot,
    [int]$MaxPasses = 3,
    [int]$RetryDelaySeconds = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$cargoLanesRootMarkerName = ".codex-cargo-lanes-root"
$cargoLanesRootMarkerContent = "codex-kd cargo lanes root v1"

$root = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LanesRoot)
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
    exit 0
}
try {
    $rootItem = Get-Item -LiteralPath $root -Force -ErrorAction Stop
    if (($rootItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
        exit 0
    }
}
catch {
    exit 0
}
$rootMarkerPath = Join-Path $root $cargoLanesRootMarkerName
if (-not (Test-Path -LiteralPath $rootMarkerPath -PathType Leaf)) {
    exit 0
}
try {
    $rootMarker = Get-Item -LiteralPath $rootMarkerPath -Force
    if (
        (($rootMarker.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) -or
        ([IO.File]::ReadAllText($rootMarkerPath).Trim() -cne $cargoLanesRootMarkerContent)
    ) {
        exit 0
    }
}
catch {
    exit 0
}
if ($MaxPasses -lt 1) {
    exit 0
}
if ($RetryDelaySeconds -lt 0) {
    $RetryDelaySeconds = 0
}

$logPath = Join-Path $root ".trash-cleanup.log"
$lockPath = Join-Path $root ".cargo-lane-trash-cleanup.lock"

function Test-IsChildPath {
    param(
        [string]$ChildPath,
        [string]$ParentPath
    )

    $trimChars = [char[]]@(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $resolvedParent = [System.IO.Path]::GetFullPath($ParentPath).TrimEnd($trimChars)
    $resolvedChild = [System.IO.Path]::GetFullPath($ChildPath)
    $prefix = $resolvedParent + [System.IO.Path]::DirectorySeparatorChar
    return $resolvedChild.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)
}

function Write-CleanupLog {
    param([string]$Message)

    try {
        $timestamp = [DateTime]::UtcNow.ToString("o", [Globalization.CultureInfo]::InvariantCulture)
        Add-Content -LiteralPath $logPath -Encoding UTF8 -Value "[$timestamp] $Message" -ErrorAction SilentlyContinue
    }
    catch {
        return
    }
}

function Get-SafeTrashDirectory {
    param([string]$CandidatePath)

    try {
        $item = Get-Item -LiteralPath $CandidatePath -Force -ErrorAction Stop
        if ($item.Name -notmatch "\.trash-\d{17}$") {
            Write-CleanupLog ("skipped path with a non-trash name: {0}" -f $item.FullName)
            return $null
        }
        if (-not $item.PSIsContainer) {
            Write-CleanupLog ("skipped non-directory trash path: {0}" -f $item.FullName)
            return $null
        }
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            Write-CleanupLog ("skipped reparse-point trash directory: {0}" -f $item.FullName)
            return $null
        }
        if (-not (Test-IsChildPath -ChildPath $item.FullName -ParentPath $root)) {
            Write-CleanupLog ("skipped trash directory outside lanes root: {0}" -f $item.FullName)
            return $null
        }
        return $item
    }
    catch [System.Management.Automation.ItemNotFoundException] {
        return $null
    }
    catch {
        Write-CleanupLog ("skipped unreadable trash path {0}: {1}" -f $CandidatePath, $_.Exception.Message)
        return $null
    }
}

function Get-SafeTrashDirectories {
    $safe = @()
    $candidates = @(Get-ChildItem -LiteralPath $root -Force -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "\.trash-\d{17}$" })
    foreach ($candidate in $candidates) {
        $current = Get-SafeTrashDirectory -CandidatePath $candidate.FullName
        if ($null -ne $current) {
            $safe += $current
        }
    }
    return @($safe | Sort-Object -Property LastWriteTimeUtc)
}

$lockStream = $null
try {
    try {
        $lockStream = [IO.File]::Open($lockPath, [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $lockStream.SetLength(0)
        $lockText = "pid=$PID`nstarted=$([DateTime]::UtcNow.ToString("o", [Globalization.CultureInfo]::InvariantCulture))`n"
        $lockBytes = [Text.Encoding]::UTF8.GetBytes($lockText)
        $lockStream.Write($lockBytes, 0, $lockBytes.Length)
        $lockStream.Flush()
    }
    catch {
        exit 0
    }

    for ($pass = 1; $pass -le $MaxPasses; $pass++) {
        $trashDirs = @(Get-SafeTrashDirectories)
        if ($trashDirs.Count -eq 0) {
            break
        }

        foreach ($trash in $trashDirs) {
            try {
                # Re-fetch and revalidate immediately before the destructive
                # operation instead of trusting the earlier enumeration.
                $currentTrash = Get-SafeTrashDirectory -CandidatePath $trash.FullName
                if ($null -eq $currentTrash) {
                    continue
                }
                Remove-Item -LiteralPath $currentTrash.FullName -Recurse -Force -ErrorAction Stop
            }
            catch {
                Write-CleanupLog ("failed attempt {0}/{1}: {2}: {3}" -f $pass, $MaxPasses, $trash.FullName, $_.Exception.Message)
            }
        }

        $remainingAfterPass = @(Get-SafeTrashDirectories)
        if ($remainingAfterPass.Count -eq 0) {
            break
        }
        if ($pass -lt $MaxPasses) {
            Start-Sleep -Seconds $RetryDelaySeconds
        }
    }

    $remaining = @(Get-SafeTrashDirectories)
    if ($remaining.Count -gt 0) {
        Write-CleanupLog ("remaining trash directories after cleanup worker: {0}" -f $remaining.Count)
    }
}
catch {
    Write-CleanupLog ("trash cleanup worker failed: {0}" -f $_.Exception.Message)
}
finally {
    if ($null -ne $lockStream) {
        $lockStream.Dispose()
        Remove-Item -LiteralPath $lockPath -Force -ErrorAction SilentlyContinue
    }
}
