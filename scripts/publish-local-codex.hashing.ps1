function Get-FileSha256 {
    param(
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return "<missing>"
    }

    $stream = $null
    $sha256 = $null
    try {
        $stream = [IO.File]::Open(
            $Path,
            [IO.FileMode]::Open,
            [IO.FileAccess]::Read,
            [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
        )
        $sha256 = [Security.Cryptography.SHA256]::Create()
        $hash = $sha256.ComputeHash($stream)
        return [BitConverter]::ToString($hash).Replace("-", "").ToLowerInvariant()
    }
    finally {
        if ($null -ne $stream) {
            $stream.Dispose()
        }
        if ($null -ne $sha256) {
            $sha256.Dispose()
        }
    }
}

function Get-TextSha256 {
    param(
        [string]$Value
    )

    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($Value))
        return [BitConverter]::ToString($hash).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha256.Dispose()
    }
}

if (-not (Get-Variable -Name FileSha256CachePrunedDirs -Scope Script -ErrorAction SilentlyContinue)) {
    $script:FileSha256CachePrunedDirs = @{}
}

function Get-FileIdentity {
    param(
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }

    $item = Get-Item -LiteralPath $Path
    return [pscustomobject]@{
        Length = $item.Length
        LastWriteTimeUtc = $item.LastWriteTimeUtc
    }
}

function Remove-StaleFileSha256CacheEntries {
    param(
        [string]$CacheDir
    )

    $cacheKey = [IO.Path]::GetFullPath($CacheDir)
    if ($script:FileSha256CachePrunedDirs.ContainsKey($cacheKey)) {
        return
    }
    $script:FileSha256CachePrunedDirs[$cacheKey] = $true

    if (-not [IO.Directory]::Exists($cacheKey)) {
        return
    }

    foreach ($cachePath in [IO.Directory]::EnumerateFiles($cacheKey, "*.sha256.json")) {
        try {
            $cached = [IO.File]::ReadAllText($cachePath) | ConvertFrom-Json
            $pathProperty = $cached.PSObject.Properties["path"]
            if ($null -eq $pathProperty) {
                continue
            }
            $cachedPath = [string]$pathProperty.Value
            if (-not [string]::IsNullOrWhiteSpace($cachedPath) -and -not [IO.File]::Exists($cachedPath)) {
                [IO.File]::Delete($cachePath)
                continue
            }
            if (-not [string]::IsNullOrWhiteSpace($cachedPath)) {
                $expectedName = "$(Get-TextSha256 -Value ([IO.Path]::GetFullPath($cachedPath))).sha256.json"
                if ([IO.Path]::GetFileName($cachePath) -cne $expectedName) {
                    [IO.File]::Delete($cachePath)
                }
            }
        }
        catch {
        }
    }
}

function Get-FileSha256Cached {
    param(
        [string]$Path,
        [string]$CacheDir
    )

    Remove-StaleFileSha256CacheEntries -CacheDir $CacheDir

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return "<missing>"
    }

    $identity = Get-FileIdentity -Path $Path
    $safeName = Get-TextSha256 -Value ([IO.Path]::GetFullPath($Path))
    $cachePath = Join-Path $CacheDir "$safeName.sha256.json"

    if (Test-Path -LiteralPath $cachePath -PathType Leaf) {
        try {
            $cached = [IO.File]::ReadAllText($cachePath) | ConvertFrom-Json
            # A plain [DateTime] cast converts the stored round-trip string to
            # local time (Kind=Local), so -eq never matched LastWriteTimeUtc on
            # non-UTC machines and the cache never hit. Parse with
            # RoundtripKind and compare in UTC.
            $cachedLastWriteUtc = [DateTime]::Parse(
                [string]$cached.lastWriteUtc,
                [System.Globalization.CultureInfo]::InvariantCulture,
                [System.Globalization.DateTimeStyles]::RoundtripKind
            ).ToUniversalTime()
            if (
                [int64]$cached.length -eq [int64]$identity.Length -and
                $cachedLastWriteUtc -eq $identity.LastWriteTimeUtc -and
                -not [string]::IsNullOrWhiteSpace([string]$cached.sha256)
            ) {
                return ([string]$cached.sha256).ToLowerInvariant()
            }
        }
        catch {
        }
    }

    $hash = Get-FileSha256 -Path $Path
    New-Item -ItemType Directory -Path $CacheDir -Force | Out-Null
    [pscustomobject]@{
        path = [IO.Path]::GetFullPath($Path)
        length = $identity.Length
        lastWriteUtc = $identity.LastWriteTimeUtc.ToString("o")
        sha256 = $hash
    } | ConvertTo-Json -Compress | Set-Content -LiteralPath $cachePath -Encoding UTF8
    return $hash
}
