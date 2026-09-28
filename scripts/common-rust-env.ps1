# One shared local default with scripts/just-shell.py and
# scripts/codex_package/cargo.py; override everywhere with
# CODEX_SCCACHE_CACHE_SIZE.
$script:CodexRustSccacheCacheSizeDefault = "80G"

function Get-CodexCargoPackageSpecs {
    param([string[]]$CommandArgs)

    for ($index = 0; $index -lt $CommandArgs.Count; $index++) {
        $token = $CommandArgs[$index]
        if ($token -ceq "--") { break }
        $spec = $null
        if ($token -cin @("-p", "--package")) {
            $index++
            if ($index -lt $CommandArgs.Count) { $spec = $CommandArgs[$index] }
            if ($spec -ceq "--") { break }
        }
        elseif ($token.StartsWith("--package=", [StringComparison]::Ordinal)) {
            $spec = $token.Substring(10)
        }
        elseif ($token.StartsWith("-p", [StringComparison]::Ordinal)) {
            $spec = $token.Substring(2)
            if ($spec.StartsWith("=")) { $spec = $spec.Substring(1) }
        }
        if (-not [string]::IsNullOrEmpty($spec) -and -not $spec.StartsWith("-")) {
            $spec
        }
    }
}

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
    $envNames = @(
        "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER",
        "CARGO_TARGET_AARCH64_PC_WINDOWS_MSVC_LINKER"
    )
    $missingEnvNames = @($envNames | Where-Object {
            [string]::IsNullOrWhiteSpace([System.Environment]::GetEnvironmentVariable($_, "Process"))
        })
    if ($missingEnvNames.Count -eq 0) {
        return
    }

    $lldLink = Get-CodexRustLldLinkPath
    if (-not [string]::IsNullOrWhiteSpace($lldLink)) {
        foreach ($envName in $missingEnvNames) {
            Set-Item -Path "Env:$envName" -Value $lldLink
        }
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
        [string[]]$CommandArgs,
        [int]$StartIndex = 1,
        [string[]]$GlobalOptionsWithValue = @("--color", "--config", "-C", "-Z")
    )

    if ($CommandArgs.Count -lt 2 -or -not (Test-CargoProgram -Value $CommandArgs[0])) {
        return -1
    }

    $index = $StartIndex
    if ($index -eq 1 -and $index -lt $CommandArgs.Count -and $CommandArgs[$index].StartsWith("+")) {
        $index += 1
    }

    while ($index -lt $CommandArgs.Count) {
        $arg = $CommandArgs[$index]
        if ($arg -eq "--") {
            return -1
        }
        if (-not $arg.StartsWith("-")) {
            return $index
        }

        $optionName = ($arg -split "=", 2)[0]
        if ($GlobalOptionsWithValue -ccontains $optionName -and $arg -notmatch "=") {
            if ($index + 1 -ge $CommandArgs.Count) {
                throw "Cargo option $arg requires a value."
            }
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
        [string]$TargetDir,
        [string]$WorkingDir = (Get-Location -PSProvider FileSystem).ProviderPath
    )

    if ([string]::IsNullOrWhiteSpace($Candidate)) {
        throw "Cargo --target-dir requires a non-empty path."
    }
    try {
        # Cargo joins a relative --target-dir onto its working directory,
        # which PowerShell takes from the current location; GetFullPath alone
        # would resolve against the unrelated process directory.
        $candidatePath = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($WorkingDir, $Candidate)).TrimEnd('\', '/')
        $lanePath = [System.IO.Path]::GetFullPath($TargetDir).TrimEnd('\', '/')
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
        [string]$TargetDir,
        [string]$WorkingDir = (Get-Location -PSProvider FileSystem).ProviderPath
    )

    if ([string]::IsNullOrWhiteSpace($ExecCommand)) {
        throw "Cargo watch exec requires a command."
    }
    $separator = [regex]::Match($ExecCommand, "\s--(?=\s|$)")
    $separatorIndex = if ($separator.Success) { $separator.Index } else { -1 }
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
        Assert-CargoTargetDirMatchesLane -Candidate $candidate -TargetDir $TargetDir -WorkingDir $WorkingDir
    }
    if ($targetMatches.Count -gt 0) {
        # Freeze relative paths before cargo-watch changes its working directory.
        $replacement = " --target-dir $(Format-CargoWatchExecTargetDir -TargetDir $TargetDir)"
        for ($i = $targetMatches.Count - 1; $i -ge 0; $i--) {
            $match = $targetMatches[$i]
            $ExecCommand = $ExecCommand.Remove($match.Index, $match.Length).Insert($match.Index, $replacement)
        }
        return $ExecCommand
    }

    $watchBuildCommands = @(
        "b", "c", "t", "r", "d", "clean", "rustdoc", "package", "install", "publish",
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
        throw "Unsupported Cargo watch exec command in a reserved lane."
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
    $execArguments = [System.Collections.Generic.List[object]]::new()
    $workingDir = (Get-Location -PSProvider FileSystem).ProviderPath
    $hasExec = $false
    $valueOptions = @("-d", "--delay", "--env-file", "-E", "--env", "--features", "-i", "--ignore", "-B", "-L", "--use-shell", "-w", "--watch", "-C", "--workdir")
    for ($i = 0; $i -lt $CommandArgs.Count; $i++) {
        $arg = $CommandArgs[$i]
        if ($i -gt $SubcommandIndex -and $arg -cmatch '^(-[cqNhV]+)([xsdiEBLwC].*)$') {
            [void]$updated.Add($matches[1])
            $arg = "-" + $matches[2]
        }
        [void]$updated.Add($arg)

        if ($i -le $SubcommandIndex) {
            continue
        }
        if ($arg -eq "--") {
            if ($i + 1 -lt $CommandArgs.Count) {
                throw "Cargo watch positional commands cannot enforce a reserved target; use --exec/-x."
            }
            [void]$updated.RemoveAt($updated.Count - 1)
            break
        }
        if (
            $arg.StartsWith("-s", [StringComparison]::Ordinal) -or
            $arg -eq "--shell" -or
            $arg.StartsWith("--shell=", [StringComparison]::Ordinal)
        ) {
            throw "Cargo watch --shell/-s is not allowed inside a reserved lane; use --exec/-x so --target-dir can be enforced."
        }
        if ($arg -eq "-x" -or $arg -eq "--exec") {
            $hasExec = $true
            $i++
            if ($i -ge $CommandArgs.Count) { throw "Cargo watch --exec/-x requires a command." }
            if ($i -lt $CommandArgs.Count) {
                [void]$execArguments.Add(@{ Index = $updated.Count; Prefix = ""; Command = $CommandArgs[$i] })
                [void]$updated.Add($CommandArgs[$i])
            }
            continue
        }
        if ($arg.StartsWith("--exec=", [StringComparison]::Ordinal)) {
            $hasExec = $true
            $exec = $arg.Substring("--exec=".Length)
            [void]$execArguments.Add(@{ Index = $updated.Count - 1; Prefix = "--exec="; Command = $exec })
            continue
        }
        if ($arg.StartsWith("-x", [StringComparison]::Ordinal)) {
            $hasExec = $true
            $exec = $arg.Substring(2)
            if ($exec.StartsWith("=")) { $exec = $exec.Substring(1) }
            [void]$execArguments.Add(@{ Index = $updated.Count - 1; Prefix = "-x"; Command = $exec })
            continue
        }
        if ($arg -cin $valueOptions) {
            $i++
            if ($i -ge $CommandArgs.Count) { throw "Cargo watch $arg requires a value." }
            [void]$updated.Add($CommandArgs[$i])
            if ($arg -cin @("-C", "--workdir")) { $workingDir = $CommandArgs[$i] }
            continue
        }
        if ($arg.StartsWith("--workdir=", [StringComparison]::Ordinal)) {
            $workingDir = $arg.Substring("--workdir=".Length)
        }
        elseif ($arg.StartsWith("-C", [StringComparison]::Ordinal)) {
            $workingDir = $arg.Substring(2).TrimStart('=')
        }
        if (-not $arg.StartsWith("-")) {
            throw "Cargo watch positional commands cannot enforce a reserved target; use --exec/-x."
        }
    }

    $workingDir = [IO.Path]::GetFullPath([IO.Path]::Combine((Get-Location -PSProvider FileSystem).ProviderPath, $workingDir))
    foreach ($exec in $execArguments) {
        $updated[$exec.Index] = $exec.Prefix + (Add-CargoWatchExecTargetDir -ExecCommand $exec.Command -TargetDir $TargetDir -WorkingDir $workingDir)
    }
    if (-not $hasExec) {
        [void]$updated.Add("-x")
        [void]$updated.Add((Add-CargoWatchExecTargetDir -ExecCommand "check" -TargetDir $TargetDir))
    }
    if ($CommandArgs[-1] -eq "--") { [void]$updated.Add("--") }
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

    if ($CommandArgs.Count -ge 4 -and [IO.Path]::GetFileNameWithoutExtension($CommandArgs[0]) -eq "rustup" -and $CommandArgs[1] -eq "run") {
        $cargoIndex = if ($CommandArgs[2] -eq "--install") { 4 } else { 3 }
        if ($cargoIndex -lt $CommandArgs.Count -and (Test-CargoProgram -Value $CommandArgs[$cargoIndex])) {
            return @(@($CommandArgs | Select-Object -First $cargoIndex) + @(Add-CargoTargetDirArgument -CommandArgs @($CommandArgs | Select-Object -Skip $cargoIndex) -TargetDir $TargetDir))
        }
    }
    if ($CommandArgs.Count -lt 2 -or -not (Test-CargoProgram -Value $CommandArgs[0])) {
        return $CommandArgs
    }

    $subcommandIndex = Get-CargoSubcommandIndex -CommandArgs $CommandArgs
    if ($subcommandIndex -lt 0) {
        return $CommandArgs
    }

    $buildCommands = @(
        "b", "c", "t", "r", "d", "clean", "rustdoc", "package", "install", "publish",
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
        $nextestCommandIndex = Get-CargoSubcommandIndex -CommandArgs $CommandArgs -StartIndex ($subcommandIndex + 1) -GlobalOptionsWithValue @("--color", "--manifest-path", "--config-file", "--user-config-file", "--tool-config-file", "-P", "--profile")
        if ($nextestCommandIndex -lt 0) {
            return $CommandArgs
        }
        if ($CommandArgs[$nextestCommandIndex] -notin @("archive", "run", "r", "list", "bench", "b")) {
            if ($CommandArgs[$nextestCommandIndex] -in @("help", "show-config", "self")) {
                return $CommandArgs
            }
            throw "Unsupported Cargo nextest command in a reserved lane."
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
        if ($subcommand -in @("add", "remove", "rm", "fetch", "fmt", "generate-lockfile", "locate-project", "login", "logout", "metadata", "new", "init", "owner", "search", "tree", "update", "vendor", "verify-project", "version", "help", "report", "read-manifest", "uninstall", "info")) {
            return $CommandArgs
        }
        throw "Unsupported Cargo command '$subcommand' in a reserved lane; use an explicit build command."
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
