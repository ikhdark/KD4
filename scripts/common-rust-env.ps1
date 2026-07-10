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

function Get-CodexRustSccacheExpectedStatsCacheSize {
    $cacheSize = Get-CodexRustSccacheCacheSize
    if ($cacheSize -match "^(\d+)G$") {
        return "$($matches[1]) GiB"
    }
    return $cacheSize
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
    return $actual -eq (Get-CodexRustSccacheExpectedStatsCacheSize)
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
        if ($globalOptionsWithValue -contains $optionName -and $arg -notmatch "=") {
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

function Add-CargoWatchExecTargetDir {
    param(
        [string]$ExecCommand,
        [string]$TargetDir
    )

    if ([string]::IsNullOrWhiteSpace($ExecCommand)) {
        return $ExecCommand
    }
    if ($ExecCommand -match "(?:^|\s)--target-dir(?:=|\s+)") {
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
    $separatorIndex = $ExecCommand.IndexOf(" -- ", [StringComparison]::Ordinal)
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
        if ($arg -eq "-s" -or $arg -eq "--shell") {
            $i++
            if ($i -lt $CommandArgs.Count) {
                [void]$updated.Add($CommandArgs[$i])
            }
            continue
        }
        if ($arg.StartsWith("--shell=", [StringComparison]::Ordinal)) {
            continue
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
    $hasShell = @($CommandArgs | Where-Object {
            $_ -eq "-s" -or $_ -eq "--shell" -or $_.StartsWith("--shell=", [StringComparison]::Ordinal)
        }).Count -gt 0
    if (-not $hasExec -and -not $hasShell) {
        [void]$updated.Add("-x")
        [void]$updated.Add((Add-CargoWatchExecTargetDir -ExecCommand "check" -TargetDir $TargetDir))
    }
    return @($updated)
}

function Test-CargoTargetDirArgumentPresent {
    param(
        [string[]]$CommandArgs,
        [int]$StartIndex
    )

    for ($i = $StartIndex; $i -lt $CommandArgs.Count; $i++) {
        $arg = $CommandArgs[$i]
        if ($arg -eq "--") {
            return $false
        }
        if ($arg -eq "--target-dir" -or $arg.StartsWith("--target-dir=", [StringComparison]::Ordinal)) {
            return $true
        }
    }
    return $false
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
        if (Test-CargoTargetDirArgumentPresent -CommandArgs $CommandArgs -StartIndex ($nextestCommandIndex + 1)) {
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
    if (Test-CargoTargetDirArgumentPresent -CommandArgs $CommandArgs -StartIndex ($subcommandIndex + 1)) {
        return $CommandArgs
    }

    return @(
        @($CommandArgs | Select-Object -First ($subcommandIndex + 1)) +
        @("--target-dir", $TargetDir) +
        @($CommandArgs | Select-Object -Skip ($subcommandIndex + 1))
    )
}
