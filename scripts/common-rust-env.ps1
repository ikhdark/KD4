# One shared local default with scripts/just-shell.py and
# scripts/codex_package/cargo.py; override everywhere with
# CODEX_SCCACHE_CACHE_SIZE.
$script:CodexRustSccacheCacheSizeDefault = "80G"

function Get-CodexRustSccacheBaseDir {
    param(
        [string]$RepoRoot
    )

    return [System.IO.Path]::GetFullPath($RepoRoot)
}

function Get-CodexRustSccacheCacheSize {
    $override = $env:CODEX_SCCACHE_CACHE_SIZE
    if (-not [string]::IsNullOrWhiteSpace($override)) {
        return $override.Trim()
    }
    return $script:CodexRustSccacheCacheSizeDefault
}

function Set-CodexRustSccacheEnvironment {
    param(
        [string]$RepoRoot
    )

    $env:SCCACHE_BASEDIR = Get-CodexRustSccacheBaseDir -RepoRoot $RepoRoot
    $env:SCCACHE_CACHE_SIZE = Get-CodexRustSccacheCacheSize
}

function ConvertTo-CodexRustByteSize {
    param(
        [string]$Value
    )

    if ([string]::IsNullOrWhiteSpace($Value) -or $Value -notmatch "^\s*(\d+(?:\.\d+)?)\s*([KMGTPE]?)(?:i?B)?\s*$") {
        return $null
    }

    $number = [decimal]0
    if (-not [decimal]::TryParse(
            $matches[1],
            [Globalization.NumberStyles]::AllowDecimalPoint,
            [Globalization.CultureInfo]::InvariantCulture,
            [ref]$number
        )) {
        return $null
    }
    $multipliers = @{
        "" = [decimal]1
        "K" = [decimal]1024
        "M" = [decimal]1048576
        "G" = [decimal]1073741824
        "T" = [decimal]1099511627776
        "P" = [decimal]1125899906842624
        "E" = [decimal]1152921504606846976
    }
    $bytes = $number * $multipliers[$matches[2].ToUpperInvariant()]
    if ($bytes -ne [decimal]::Truncate($bytes) -or $bytes -gt [int64]::MaxValue) {
        return $null
    }
    return [int64]$bytes
}

function Get-CodexRustSccacheStatsMaxCacheSize {
    param(
        [string[]]$Stats
    )

    foreach ($line in $Stats) {
        if ($line -match "^Max cache size\s+(.+)$") {
            return $matches[1].Trim()
        }
    }
    return $null
}

function Test-CodexRustSccacheStatsCacheSize {
    param(
        [string[]]$Stats
    )

    $actual = Get-CodexRustSccacheStatsMaxCacheSize -Stats $Stats
    if ($null -eq $actual) {
        return $true
    }
    $expectedBytes = ConvertTo-CodexRustByteSize -Value (Get-CodexRustSccacheCacheSize)
    $actualBytes = ConvertTo-CodexRustByteSize -Value $actual
    if ($null -eq $expectedBytes -or $null -eq $actualBytes) {
        # Unknown formats should not bounce a shared server on every lane run.
        return $true
    }
    return $actualBytes -eq $expectedBytes
}

function Ensure-CodexRustSccacheServer {
    param(
        [string]$RepoRoot
    )

    if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
        return
    }

    Set-CodexRustSccacheEnvironment -RepoRoot $RepoRoot
    # Windows PowerShell 5.1 turns redirected native stderr into terminating
    # errors while $ErrorActionPreference is "Stop", which would bypass the
    # graceful $LASTEXITCODE fallbacks below.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $stats = @(sccache --show-stats 2>$null)
        if ($LASTEXITCODE -ne 0) {
            return
        }
        if (Test-CodexRustSccacheStatsCacheSize -Stats $stats) {
            return
        }

        sccache --stop-server 2>$null | Out-Null
        sccache --start-server 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) {
            return
        }
        $restartedStats = @(sccache --show-stats 2>$null)
        if (
            $LASTEXITCODE -ne 0 -or
            -not (Test-CodexRustSccacheStatsCacheSize -Stats $restartedStats)
        ) {
            return
        }
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
}

function Get-CodexRustLldLinkPath {
    $lldLink = Get-Command lld-link -ErrorAction SilentlyContinue
    if ($null -ne $lldLink) {
        return $lldLink.Source
    }

    $candidateRoots = @()
    if (-not [string]::IsNullOrWhiteSpace($env:SCOOP)) {
        $candidateRoots += $env:SCOOP
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $candidateRoots += (Join-Path $env:USERPROFILE "scoop")
    }

    foreach ($root in @($candidateRoots | Select-Object -Unique)) {
        $scoopLldLink = Join-Path $root "apps\llvm\current\bin\lld-link.exe"
        if (Test-Path -LiteralPath $scoopLldLink -PathType Leaf) {
            return $scoopLldLink
        }
    }

    $programFilesLldLink = "C:\Program Files\LLVM\bin\lld-link.exe"
    if (Test-Path -LiteralPath $programFilesLldLink -PathType Leaf) {
        return $programFilesLldLink
    }

    return $null
}

function Set-CodexRustMsvcLinkerEnvironment {
    $envName = "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER"
    if (-not [string]::IsNullOrWhiteSpace([System.Environment]::GetEnvironmentVariable($envName, "Process"))) {
        return
    }

    $lldLink = Get-CodexRustLldLinkPath
    if (-not [string]::IsNullOrWhiteSpace($lldLink)) {
        Set-Item -Path "Env:$envName" -Value $lldLink
    }
}

function Test-CargoProgram {
    param(
        [string]$Value
    )

    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $false
    }
    $leaf = [System.IO.Path]::GetFileNameWithoutExtension($Value)
    return $leaf -eq "cargo"
}

function Get-CargoSubcommandIndex {
    param(
        [string[]]$CommandArgs
    )

    if ($CommandArgs.Count -lt 2 -or -not (Test-CargoProgram -Value $CommandArgs[0])) {
        return -1
    }

    $index = 1
    if ($CommandArgs[$index].StartsWith("+")) {
        $index += 1
    }

    $globalOptionsWithValue = @("--color", "--config", "-C", "-Z")
    while ($index -lt $CommandArgs.Count) {
        $arg = $CommandArgs[$index]
        if ($arg -eq "--") {
            return -1
        }
        if (-not $arg.StartsWith("-")) {
            return $index
        }

        $optionName = ($arg -split "=", 2)[0]
        if ($globalOptionsWithValue -ccontains $optionName -and $arg -notmatch "=") {
            $index += 2
        }
        else {
            $index += 1
        }
    }

    return -1
}

function Format-CargoWatchExecTargetDir {
    param(
        [string]$TargetDir
    )

    if ($TargetDir -match "\s") {
        return '"' + ($TargetDir -replace '"', '\"') + '"'
    }
    return $TargetDir
}

function Assert-CargoTargetDirMatchesLane {
    param(
        [string]$Candidate,
        [string]$TargetDir
    )

    if ([string]::IsNullOrWhiteSpace($Candidate)) {
        throw "Cargo --target-dir requires a non-empty path."
    }
    try {
        $candidatePath = [System.IO.Path]::GetFullPath($Candidate)
        $lanePath = [System.IO.Path]::GetFullPath($TargetDir)
    }
    catch {
        throw "Cargo --target-dir '$Candidate' is not a valid path: $($_.Exception.Message)"
    }
    if (-not [string]::Equals($candidatePath, $lanePath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Cargo --target-dir '$Candidate' does not match reserved lane target '$TargetDir'."
    }
}

function Add-CargoWatchExecTargetDir {
    param(
        [string]$ExecCommand,
        [string]$TargetDir
    )

    if ([string]::IsNullOrWhiteSpace($ExecCommand)) {
        return $ExecCommand
    }
    $separatorIndex = $ExecCommand.IndexOf(" -- ", [StringComparison]::Ordinal)
    $cargoCommand = if ($separatorIndex -ge 0) {
        $ExecCommand.Substring(0, $separatorIndex)
    }
    else {
        $ExecCommand
    }
    $targetPattern = '(?:^|\s)--target-dir(?:=(?:"(?<double>[^\"]*)"|''(?<single>[^'']*)''|(?<bare>[^\s]+))|\s+(?:"(?<double_space>[^\"]*)"|''(?<single_space>[^'']*)''|(?<bare_space>[^\s]+)))'
    $targetMatches = [regex]::Matches($cargoCommand, $targetPattern)
    if ($cargoCommand -match '(?:^|\s)--target-dir(?:=|\s|$)' -and $targetMatches.Count -eq 0) {
        throw "Cargo watch exec command has a malformed --target-dir option."
    }
    foreach ($targetMatch in $targetMatches) {
        $candidate = @(
            "double",
            "single",
            "bare",
            "double_space",
            "single_space",
            "bare_space"
        ) | ForEach-Object { $targetMatch.Groups[$_].Value } | Where-Object { $_.Length -gt 0 } | Select-Object -First 1
        Assert-CargoTargetDirMatchesLane -Candidate $candidate -TargetDir $TargetDir
    }
    if ($targetMatches.Count -gt 0) {
        return $ExecCommand
    }

    $watchBuildCommands = @(
        "bench",
        "build",
        "check",
        "clippy",
        "doc",
        "fix",
        "llvm-cov",
        "run",
        "rustc",
        "test"
    )
    $firstToken = ($ExecCommand.Trim() -split "\s+", 2)[0]
    if ($firstToken -notin $watchBuildCommands) {
        return $ExecCommand
    }

    $targetArgument = "--target-dir $(Format-CargoWatchExecTargetDir -TargetDir $TargetDir)"
    if ($separatorIndex -ge 0) {
        return $ExecCommand.Insert($separatorIndex, " $targetArgument")
    }
    return "$ExecCommand $targetArgument"
}

function Add-CargoWatchTargetDirArgument {
    param(
        [string[]]$CommandArgs,
        [int]$SubcommandIndex,
        [string]$TargetDir
    )

    $updated = [System.Collections.Generic.List[string]]::new()
    for ($i = 0; $i -lt $CommandArgs.Count; $i++) {
        $arg = $CommandArgs[$i]
        [void]$updated.Add($arg)

        if ($i -le $SubcommandIndex) {
            continue
        }
        if ($arg -eq "--") {
            return $CommandArgs
        }
        if (
            $arg -eq "-s" -or
            $arg -eq "--shell" -or
            $arg.StartsWith("--shell=", [StringComparison]::Ordinal)
        ) {
            throw "Cargo watch --shell/-s is not allowed inside a reserved lane; use --exec/-x so --target-dir can be enforced."
        }
        if ($arg -eq "-x" -or $arg -eq "--exec") {
            $i++
            if ($i -lt $CommandArgs.Count) {
                [void]$updated.Add((Add-CargoWatchExecTargetDir -ExecCommand $CommandArgs[$i] -TargetDir $TargetDir))
            }
            continue
        }
        if ($arg.StartsWith("--exec=", [StringComparison]::Ordinal)) {
            $exec = $arg.Substring("--exec=".Length)
            [void]$updated.RemoveAt($updated.Count - 1)
            [void]$updated.Add("--exec=$(Add-CargoWatchExecTargetDir -ExecCommand $exec -TargetDir $TargetDir)")
            continue
        }
    }

    $hasExec = @($CommandArgs | Where-Object {
            $_ -eq "-x" -or $_ -eq "--exec" -or $_.StartsWith("--exec=", [StringComparison]::Ordinal)
        }).Count -gt 0
    if (-not $hasExec) {
        [void]$updated.Add("-x")
        [void]$updated.Add((Add-CargoWatchExecTargetDir -ExecCommand "check" -TargetDir $TargetDir))
    }
    return @($updated)
}

function Test-CargoTargetDirArgumentPresent {
    param(
        [string[]]$CommandArgs,
        [int]$StartIndex,
        [string]$TargetDir
    )

    $present = $false
    for ($i = $StartIndex; $i -lt $CommandArgs.Count; $i++) {
        $arg = $CommandArgs[$i]
        if ($arg -eq "--") {
            break
        }
        if ($arg -eq "--target-dir") {
            if (($i + 1) -ge $CommandArgs.Count -or $CommandArgs[$i + 1] -eq "--") {
                throw "Cargo --target-dir requires a path value."
            }
            if (-not [string]::IsNullOrWhiteSpace($TargetDir)) {
                Assert-CargoTargetDirMatchesLane -Candidate $CommandArgs[$i + 1] -TargetDir $TargetDir
            }
            $present = $true
            $i++
            continue
        }
        if ($arg.StartsWith("--target-dir=", [StringComparison]::Ordinal)) {
            if (-not [string]::IsNullOrWhiteSpace($TargetDir)) {
                Assert-CargoTargetDirMatchesLane -Candidate $arg.Substring("--target-dir=".Length) -TargetDir $TargetDir
            }
            $present = $true
        }
    }
    return $present
}

# sccache hashes CARGO_* environment variables into its rustc cache key, so
# exporting a per-lane CARGO_TARGET_DIR forces a full cache miss in every
# fresh lane. Passing the lane as a --target-dir argument right after the
# cargo subcommand keeps dependency builds shareable across lanes.
function Add-CargoTargetDirArgument {
    param(
        [string[]]$CommandArgs,
        [string]$TargetDir
    )

    if ($CommandArgs.Count -lt 2 -or -not (Test-CargoProgram -Value $CommandArgs[0])) {
        return $CommandArgs
    }

    $subcommandIndex = Get-CargoSubcommandIndex -CommandArgs $CommandArgs
    if ($subcommandIndex -lt 0) {
        return $CommandArgs
    }

    $buildCommands = @(
        "bench",
        "build",
        "check",
        "clippy",
        "doc",
        "fix",
        "llvm-cov",
        "run",
        "rustc",
        "test"
    )
    $subcommand = $CommandArgs[$subcommandIndex]
    if ($subcommand -eq "watch") {
        return Add-CargoWatchTargetDirArgument -CommandArgs $CommandArgs -SubcommandIndex $subcommandIndex -TargetDir $TargetDir
    }
    if ($subcommand -eq "nextest") {
        $nextestCommandIndex = $subcommandIndex + 1
        if ($nextestCommandIndex -ge $CommandArgs.Count) {
            return $CommandArgs
        }
        if ($CommandArgs[$nextestCommandIndex] -notin @("archive", "run")) {
            return $CommandArgs
        }
        if (Test-CargoTargetDirArgumentPresent -CommandArgs $CommandArgs -StartIndex ($nextestCommandIndex + 1) -TargetDir $TargetDir) {
            return $CommandArgs
        }
        return @(
            @($CommandArgs | Select-Object -First ($nextestCommandIndex + 1)) +
            @("--target-dir", $TargetDir) +
            @($CommandArgs | Select-Object -Skip ($nextestCommandIndex + 1))
        )
    }
    if ($subcommand -notin $buildCommands) {
        return $CommandArgs
    }
    if (Test-CargoTargetDirArgumentPresent -CommandArgs $CommandArgs -StartIndex ($subcommandIndex + 1) -TargetDir $TargetDir) {
        return $CommandArgs
    }

    return @(
        @($CommandArgs | Select-Object -First ($subcommandIndex + 1)) +
        @("--target-dir", $TargetDir) +
        @($CommandArgs | Select-Object -Skip ($subcommandIndex + 1))
    )
}
