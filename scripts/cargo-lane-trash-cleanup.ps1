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

$root = [System.IO.Path]::GetFullPath($LanesRoot)
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
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
        $trashDirs = @(Get-ChildItem -LiteralPath $root -Directory -Filter "*.trash-*" -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "\.trash-\d{17}$" } | Sort-Object -Property LastWriteTimeUtc)
        if ($trashDirs.Count -eq 0) {
            break
        }

        foreach ($trash in $trashDirs) {
            try {
                if (-not (Test-IsChildPath -ChildPath $trash.FullName -ParentPath $root)) {
                    Write-CleanupLog ("skipped trash directory outside lanes root: {0}" -f $trash.FullName)
                    continue
                }
                Remove-Item -LiteralPath $trash.FullName -Recurse -Force -ErrorAction Stop
            }
            catch {
                Write-CleanupLog ("failed attempt {0}/{1}: {2}: {3}" -f $pass, $MaxPasses, $trash.FullName, $_.Exception.Message)
            }
        }

        $remainingAfterPass = @(Get-ChildItem -LiteralPath $root -Directory -Filter "*.trash-*" -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "\.trash-\d{17}$" })
        if ($remainingAfterPass.Count -eq 0) {
            break
        }
        if ($pass -lt $MaxPasses) {
            Start-Sleep -Seconds $RetryDelaySeconds
        }
    }

    $remaining = @(Get-ChildItem -LiteralPath $root -Directory -Filter "*.trash-*" -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "\.trash-\d{17}$" })
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
