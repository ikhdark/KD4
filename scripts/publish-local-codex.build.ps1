# Build environment, rusty_v8, and cargo invocation helpers.

function Set-ProcessEnvironmentVariable {
    param(
        [string]$Name,
        [AllowNull()]
        [string]$Value
    )

    [System.Environment]::SetEnvironmentVariable($Name, $Value, "Process")
}

function Get-CargoTargetRustFlagsEnvName {
    param(
        [string]$Target
    )

    return "CARGO_TARGET_" + (($Target.ToUpperInvariant()) -replace "-", "_") + "_RUSTFLAGS"
}

function Get-StaticMsvcRustFlagsForTarget {
    param(
        [string]$Target
    )

    $flags = @("-C", "link-arg=/STACK:8388608", "-C", "target-feature=+crt-static")
    if ($Target -eq "aarch64-pc-windows-msvc") {
        $flags += @("-C", "link-arg=/arm64hazardfree")
    }
    return $flags
}

function Get-StaticMsvcRustFlagsProof {
    param(
        [string]$Profile
    )

    if ($Profile -eq "debug") {
        return "<none: debug profile>"
    }
    return "enabled via CARGO_TARGET_*_RUSTFLAGS for release publish profiles"
}

function Enable-StaticMsvcRustFlagsForPublish {
    param(
        [string]$Profile,
        [hashtable]$Previous
    )

    if ($Profile -eq "debug") {
        Write-ProofLine "staticMsvcCrt" (Get-StaticMsvcRustFlagsProof -Profile $Profile)
        return
    }

    foreach ($target in @("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc")) {
        $envName = Get-CargoTargetRustFlagsEnvName -Target $target
        $Previous[$envName] = [System.Environment]::GetEnvironmentVariable($envName, "Process")
        $flags = [string]::Join(" ", (Get-StaticMsvcRustFlagsForTarget -Target $target))
        if ([string]::IsNullOrWhiteSpace($Previous[$envName])) {
            Set-ProcessEnvironmentVariable -Name $envName -Value $flags
        }
        else {
            Set-ProcessEnvironmentVariable -Name $envName -Value ($Previous[$envName] + " " + $flags)
        }
    }

    Write-ProofLine "staticMsvcCrt" (Get-StaticMsvcRustFlagsProof -Profile $Profile)
}

function Restore-StaticMsvcRustFlagsForPublish {
    param(
        [hashtable]$Previous
    )

    foreach ($entry in $Previous.GetEnumerator()) {
        Set-ProcessEnvironmentVariable -Name $entry.Key -Value $entry.Value
    }
}

function Enable-SccacheForPublish {
    param(
        [string]$RepoRoot,
        [bool]$DisableSccache = [bool]$NoSccache
    )

    if ($DisableSccache) {
        $reason = if ($NoSccache) {
            "disabled by -NoSccache"
        }
        else {
            "disabled for release profile v8 archive linker compatibility"
        }
        Set-ProcessEnvironmentVariable -Name "RUSTC_WRAPPER" -Value $null
        Set-ProcessEnvironmentVariable -Name "CARGO_BUILD_RUSTC_WRAPPER" -Value $null
        Set-ProcessEnvironmentVariable -Name "SCCACHE_BASEDIR" -Value $null
        Set-ProcessEnvironmentVariable -Name "SCCACHE_CACHE_SIZE" -Value $null
        Write-ProofLine "rustcWrapper" "<none: $reason>"
        Write-ProofLine "cargoRustcWrapperConfig" "<none: $reason>"
        return
    }

    if (-not (Get-Command sccache -ErrorAction SilentlyContinue)) {
        Write-ProofLine "rustcWrapper" "<none: sccache not found>"
        return
    }

    Set-ProcessEnvironmentVariable -Name "SCCACHE_BASEDIR" -Value (Get-CodexRustSccacheBaseDir -RepoRoot $RepoRoot)
    Set-ProcessEnvironmentVariable -Name "SCCACHE_CACHE_SIZE" -Value (Get-CodexRustSccacheCacheSize)
    if ([string]::IsNullOrWhiteSpace($env:RUSTC_WRAPPER)) {
        Set-ProcessEnvironmentVariable -Name "RUSTC_WRAPPER" -Value "sccache"
    }
    Ensure-CodexRustSccacheServer -RepoRoot $RepoRoot
    Write-ProofLine "sccacheBaseDir" $env:SCCACHE_BASEDIR
    Write-ProofLine "sccacheCacheSize" $env:SCCACHE_CACHE_SIZE
    Write-ProofLine "rustcWrapper" $env:RUSTC_WRAPPER
}

function Restore-SccachePublishEnv {
    param(
        [hashtable]$Previous
    )

    foreach ($entry in $Previous.GetEnumerator()) {
        Set-ProcessEnvironmentVariable -Name $entry.Key -Value $entry.Value
    }
}

function Get-GitBuildCommit {
    param(
        [string]$RepoRoot
    )

    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        return "unknown"
    }

    # See Get-TrackedSourceNewestWriteUtc: keep 5.1-safe around redirected git stderr.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $commit = (& git -C $RepoRoot rev-parse --short=12 HEAD 2>$null)
        $commitExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if ($commitExitCode -ne 0 -or [string]::IsNullOrWhiteSpace($commit)) {
        return "unknown"
    }

    return ([string]$commit).Trim()
}

function Get-GitBuildDirty {
    param(
        [string]$RepoRoot
    )

    if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
        return "unknown"
    }

    # See Get-TrackedSourceNewestWriteUtc: keep 5.1-safe around redirected git stderr.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $status = (& git -C $RepoRoot status --porcelain=v1 -z -uall -- 2>$null)
        $statusExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if ($statusExitCode -ne 0) {
        return "unknown"
    }

    if ($null -ne $status -and ([string]$status).Length -gt 0) {
        return "true"
    }

    return "false"
}

function Enable-BuildMetadataForPublish {
    param(
        [string]$RepoRoot,
        [string]$Profile,
        [hashtable]$Previous
    )

    foreach ($name in @(
            "CODEX_BUILD_COMMIT",
            "CODEX_BUILD_DIRTY",
            "CODEX_BUILD_PROFILE",
            "CODEX_BUILD_TIMESTAMP"
        )) {
        $Previous[$name] = [System.Environment]::GetEnvironmentVariable($name, "Process")
    }

    Set-ProcessEnvironmentVariable -Name "CODEX_BUILD_COMMIT" -Value (Get-GitBuildCommit -RepoRoot $RepoRoot)
    Set-ProcessEnvironmentVariable -Name "CODEX_BUILD_DIRTY" -Value (Get-GitBuildDirty -RepoRoot $RepoRoot)
    Set-ProcessEnvironmentVariable -Name "CODEX_BUILD_PROFILE" -Value $Profile
    Set-ProcessEnvironmentVariable -Name "CODEX_BUILD_TIMESTAMP" -Value ((Get-Date).ToUniversalTime().ToString("o"))

    Write-ProofLine "buildMetadataCommit" $env:CODEX_BUILD_COMMIT
    Write-ProofLine "buildMetadataDirty" $env:CODEX_BUILD_DIRTY
    Write-ProofLine "buildMetadataProfile" $env:CODEX_BUILD_PROFILE
    Write-ProofLine "buildMetadataTimestamp" $env:CODEX_BUILD_TIMESTAMP
}

function Restore-BuildMetadataForPublish {
    param(
        [hashtable]$Previous
    )

    foreach ($entry in $Previous.GetEnumerator()) {
        Set-ProcessEnvironmentVariable -Name $entry.Key -Value $entry.Value
    }
}

function Get-CargoLockPackageVersion {
    param(
        [string]$RepoRoot,
        [string]$PackageName
    )

    $cargoLock = Join-Path $RepoRoot "codex-rs\Cargo.lock"
    if (-not (Test-Path -LiteralPath $cargoLock -PathType Leaf)) {
        return $null
    }

    $versions = [System.Collections.Generic.List[string]]::new()
    $currentName = $null
    $currentVersion = $null

    foreach ($line in [System.IO.File]::ReadLines($cargoLock)) {
        if ($line -eq "[[package]]") {
            if ($currentName -eq $PackageName -and -not [string]::IsNullOrWhiteSpace($currentVersion)) {
                $versions.Add($currentVersion)
            }
            $currentName = $null
            $currentVersion = $null
            continue
        }

        if ($line -match '^name = "([^"]+)"$') {
            $currentName = $matches[1]
            continue
        }

        if ($line -match '^version = "([^"]+)"$') {
            $currentVersion = $matches[1]
            continue
        }
    }

    if ($currentName -eq $PackageName -and -not [string]::IsNullOrWhiteSpace($currentVersion)) {
        $versions.Add($currentVersion)
    }

    $uniqueVersions = @($versions | Select-Object -Unique)
    if ($uniqueVersions.Count -eq 0) {
        return $null
    }

    if ($uniqueVersions.Count -ne 1) {
        throw "Expected exactly one resolved $PackageName package version in $cargoLock; found: $($uniqueVersions -join ', ')"
    }

    return $uniqueVersions[0]
}

function Get-WindowsRustyV8Target {
    $architecture = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITEW6432")
    if ([string]::IsNullOrWhiteSpace($architecture)) {
        $architecture = [System.Environment]::GetEnvironmentVariable("PROCESSOR_ARCHITECTURE")
    }

    switch ($architecture) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "ARM64" { return "aarch64-pc-windows-msvc" }
        default { return $null }
    }
}

function ConvertTo-RustyV8CacheFileName {
    param(
        [string]$Url
    )

    return $Url -replace "[^A-Za-z0-9]", "_"
}

function Test-TruthyEnvValue {
    param(
        [AllowNull()]
        [string]$Value
    )

    return $Value -in @("1", "true", "TRUE", "True", "yes", "YES", "Yes")
}

function Get-RustyV8ExpectedChecksum {
    param(
        [string]$RepoRoot,
        [string]$Version,
        [string]$ArchiveName
    )

    $checksumFileName = "rusty_v8_$($Version -replace '\.', '_').sha256"
    $checksumPath = Join-Path $RepoRoot (Join-Path "third_party\v8" $checksumFileName)
    if (-not (Test-Path -LiteralPath $checksumPath -PathType Leaf)) {
        return $null
    }

    $archiveNamePattern = [regex]::Escape($ArchiveName)
    foreach ($line in [System.IO.File]::ReadLines($checksumPath)) {
        if ($line -match "^\s*([a-fA-F0-9]{64})\s+$archiveNamePattern\s*$") {
            return $matches[1].ToLowerInvariant()
        }
    }

    return $null
}

function Assert-RustyV8ArchiveChecksum {
    param(
        [string]$Path,
        [AllowNull()]
        [string]$ExpectedChecksum
    )

    if ([string]::IsNullOrWhiteSpace($ExpectedChecksum)) {
        Write-ProofLine "v8ArchiveChecksumStatus" "<skipped: checksum metadata missing>"
        return
    }

    $actualChecksum = Get-FileSha256 -Path $Path
    Write-ProofLine "v8ArchiveChecksum" $actualChecksum
    if ($actualChecksum -ne $ExpectedChecksum) {
        Write-ProofLine "v8ArchiveChecksumStatus" "mismatch"
        throw "Rusty V8 archive checksum mismatch for $Path. Expected $ExpectedChecksum but found $actualChecksum."
    }

    Write-ProofLine "v8ArchiveChecksumStatus" "ok"
}

function Assert-RustyV8ArchiveReadyForPublish {
    param(
        [string]$RepoRoot,
        [string]$Profile,
        [switch]$DryRun,
        [switch]$AllowDownload,
        [AllowNull()]
        [string]$ArchivePath
    )

    if ($Profile -ne "release") {
        return
    }

    $v8Version = Get-CargoLockPackageVersion -RepoRoot $RepoRoot -PackageName "v8"
    if ([string]::IsNullOrWhiteSpace($v8Version)) {
        return
    }

    $target = Get-WindowsRustyV8Target
    if ([string]::IsNullOrWhiteSpace($target)) {
        Write-ProofLine "v8ArchiveStatus" "<skipped: unsupported Windows architecture>"
        return
    }

    if (Test-TruthyEnvValue -Value $env:V8_FROM_SOURCE) {
        Write-ProofLine "v8ArchiveStatus" "<skipped: V8_FROM_SOURCE set>"
        return
    }

    $archiveName = "rusty_v8_release_$target.lib.gz"
    $archiveUrl = "https://github.com/denoland/rusty_v8/releases/download/v$v8Version/$archiveName"
    $cacheFileName = ConvertTo-RustyV8CacheFileName -Url $archiveUrl

    if ([string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        Write-ProofLine "v8ArchiveStatus" "<skipped: USERPROFILE not set>"
        return
    }

    $cachePath = Join-Path (Join-Path $env:USERPROFILE ".cargo\.rusty_v8") $cacheFileName
    Write-ProofLine "v8ArchiveVersion" $v8Version
    Write-ProofLine "v8ArchiveUrl" $archiveUrl
    Write-ProofLine "v8ArchiveCachePath" $cachePath
    $expectedChecksum = Get-RustyV8ExpectedChecksum `
        -RepoRoot $RepoRoot `
        -Version $v8Version `
        -ArchiveName $archiveName

    if (-not [string]::IsNullOrWhiteSpace($ArchivePath)) {
        $resolvedArchivePath = [System.IO.Path]::GetFullPath($ArchivePath)
        Write-ProofLine "v8ArchiveInput" $resolvedArchivePath
        if (-not (Test-Path -LiteralPath $resolvedArchivePath -PathType Leaf)) {
            Write-ProofLine "v8ArchiveStatus" "input-missing"
            if (-not $DryRun) {
                throw "Rusty V8 archive input does not exist: $resolvedArchivePath"
            }
            return
        }

        Assert-RustyV8ArchiveChecksum -Path $resolvedArchivePath -ExpectedChecksum $expectedChecksum
        if ($DryRun) {
            if (Test-Path -LiteralPath $cachePath -PathType Leaf) {
                Write-ProofLine "v8ArchiveStatus" "cached"
            }
            else {
                Write-ProofLine "v8ArchiveStatus" "input-present"
                Write-ProofLine "v8ArchiveCacheAction" "would seed from $resolvedArchivePath"
            }
            return
        }

        if (-not (Test-Path -LiteralPath (Split-Path -Parent $cachePath) -PathType Container)) {
            New-Item -ItemType Directory -Path (Split-Path -Parent $cachePath) -Force | Out-Null
        }

        Copy-Item -LiteralPath $resolvedArchivePath -Destination $cachePath -Force
        Write-ProofLine "v8ArchiveCacheAction" "seeded from $resolvedArchivePath"
        Write-ProofLine "v8ArchiveStatus" "cached"
        return
    }

    if (-not [string]::IsNullOrWhiteSpace($env:RUSTY_V8_ARCHIVE)) {
        $archiveOverride = [System.IO.Path]::GetFullPath($env:RUSTY_V8_ARCHIVE)
        Write-ProofLine "v8ArchiveOverride" $archiveOverride
        if (Test-Path -LiteralPath $archiveOverride -PathType Leaf) {
            Assert-RustyV8ArchiveChecksum -Path $archiveOverride -ExpectedChecksum $expectedChecksum
            Write-ProofLine "v8ArchiveStatus" "override-present"
            return
        }

        Write-ProofLine "v8ArchiveStatus" "override-missing"
        if (-not $DryRun) {
            throw "RUSTY_V8_ARCHIVE is set, but the file does not exist: $archiveOverride"
        }
        return
    }

    if (Test-Path -LiteralPath $cachePath -PathType Leaf) {
        Assert-RustyV8ArchiveChecksum -Path $cachePath -ExpectedChecksum $expectedChecksum
        Write-ProofLine "v8ArchiveStatus" "cached"
        return
    }

    if (-not [string]::IsNullOrWhiteSpace($env:RUSTY_V8_MIRROR)) {
        Write-ProofLine "v8ArchiveStatus" "missing: delegated to RUSTY_V8_MIRROR"
        Write-ProofLine "v8ArchiveMirror" $env:RUSTY_V8_MIRROR
        return
    }

    if ($AllowDownload) {
        Write-ProofLine "v8ArchiveStatus" "missing: download allowed"
        return
    }

    Write-ProofLine "v8ArchiveStatus" "missing"
    Write-ProofLine "v8ArchiveRemedy" "Place $archiveName at $cachePath, pass -RustyV8Archive with a local copy, set RUSTY_V8_ARCHIVE, or rerun with -AllowRustyV8Download when GitHub is reachable."

    if (-not $DryRun) {
        throw "Rusty V8 archive is missing for v$v8Version ($target). This publish path will not try GitHub by default; cache $archiveName locally or rerun with -AllowRustyV8Download."
    }
}

function Format-CargoCommandForProof {
    param(
        [string[]]$CommandArgs
    )

    return ($CommandArgs | ForEach-Object {
            if ($_ -match '\s') {
                "`"$_`""
            }
            else {
                $_
            }
        }) -join " "
}

function Invoke-CodexBuild {
    param(
        [string]$RepoRoot,
        [string]$Profile,
        [switch]$DryRun
    )

    $cargoProfile = if ($Profile -eq "debug") { "dev" } else { $Profile }
    $cargoConfigArgs = @()
    $noSccacheCargoConfigPath = $null
    $releaseUsesUpstreamProfile = $Profile -eq "release"
    $effectiveNoSccache = [bool]$NoSccache -or $releaseUsesUpstreamProfile
    if ($releaseUsesUpstreamProfile) {
        Write-ProofLine "releaseProfile" "upstream-compatible: thin LTO, line-table debug, strip=false, codegen-units=4"
    }
    if ($effectiveNoSccache) {
        if ($DryRun) {
            $cargoConfigArgs = @("--config", "<temporary publish cargo config>")
        }
        else {
            $noSccacheCargoConfigPath = [System.IO.Path]::GetTempFileName()
            $cargoConfigLines = [System.Collections.Generic.List[string]]::new()
            $cargoConfigLines.Add("[build]")
            $cargoConfigLines.Add("rustc-wrapper = `"`"")
            [System.IO.File]::WriteAllText($noSccacheCargoConfigPath, ([string]::Join("`n", $cargoConfigLines) + "`n"))
            $cargoConfigArgs = @("--config", $noSccacheCargoConfigPath)
        }
    }
    $publishPackages = @("-p", "codex-cli", "-p", "codex-code-mode-host")
    $buildArgs = $cargoConfigArgs + @("build") + $publishPackages + @("--profile", $cargoProfile)
    $checkArgs = $cargoConfigArgs + @("check") + $publishPackages + @("--profile", $cargoProfile)
    $runPreflightCheck = $DryRun -and -not $SkipPreflightCheck -and $Profile -ne "debug"

    $codexRs = Join-Path $RepoRoot "codex-rs"
    $publishTargetDir = Join-Path $codexRs "target\publish-$Profile"
    $checkCommandArgs = @(Add-CargoTargetDirArgument -CommandArgs (@("cargo") + $checkArgs) -TargetDir $publishTargetDir)
    $buildCommandArgs = @(Add-CargoTargetDirArgument -CommandArgs (@("cargo") + $buildArgs) -TargetDir $publishTargetDir)
    $checkCommand = Format-CargoCommandForProof -CommandArgs $checkCommandArgs
    $buildCommand = Format-CargoCommandForProof -CommandArgs $buildCommandArgs
    Assert-RustyV8ArchiveReadyForPublish `
        -RepoRoot $RepoRoot `
        -Profile $Profile `
        -DryRun:$DryRun `
        -AllowDownload:$AllowRustyV8Download `
        -ArchivePath $RustyV8Archive
    if ($DryRun) {
        if ($runPreflightCheck) {
            Write-ProofLine "preflightCheckCommand" "$checkCommand (not run)"
        }
        Write-ProofLine "buildCommand" "$buildCommand (not run)"
        return
    }

    $previousSccacheEnv = @{
        SCCACHE_BASEDIR = $env:SCCACHE_BASEDIR
        SCCACHE_CACHE_SIZE = $env:SCCACHE_CACHE_SIZE
        RUSTC_WRAPPER = $env:RUSTC_WRAPPER
        CARGO_BUILD_RUSTC_WRAPPER = $env:CARGO_BUILD_RUSTC_WRAPPER
    }
    $previousTargetDir = $env:CARGO_TARGET_DIR
    Set-ProcessEnvironmentVariable -Name "CARGO_TARGET_DIR" -Value $null
    $previousStaticMsvcRustFlags = @{}
    $previousBuildMetadataEnv = @{}
    Enable-StaticMsvcRustFlagsForPublish -Profile $Profile -Previous $previousStaticMsvcRustFlags
    Enable-BuildMetadataForPublish -RepoRoot $RepoRoot -Profile $Profile -Previous $previousBuildMetadataEnv
    Enable-SccacheForPublish -RepoRoot $RepoRoot -DisableSccache $effectiveNoSccache
    Set-CodexRustMsvcLinkerEnvironment
    Write-ProofLine "cargoTargetDir" $publishTargetDir
    Push-Location $codexRs
    try {
        if ($runPreflightCheck) {
            $checkProgram = $checkCommandArgs[0]
            $checkProgramArgs = @($checkCommandArgs | Select-Object -Skip 1)
            & $checkProgram @checkProgramArgs
            if ($LASTEXITCODE -ne 0) {
                throw "cargo check failed with exit code $LASTEXITCODE."
            }
        }
        $buildProgram = $buildCommandArgs[0]
        $buildProgramArgs = @($buildCommandArgs | Select-Object -Skip 1)
        & $buildProgram @buildProgramArgs
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE."
        }
    }
    finally {
        Pop-Location
        Set-ProcessEnvironmentVariable -Name "CARGO_TARGET_DIR" -Value $previousTargetDir
        Restore-SccachePublishEnv -Previous $previousSccacheEnv
        Restore-BuildMetadataForPublish -Previous $previousBuildMetadataEnv
        Restore-StaticMsvcRustFlagsForPublish -Previous $previousStaticMsvcRustFlags
        if ($null -ne $noSccacheCargoConfigPath) {
            Remove-Item -LiteralPath $noSccacheCargoConfigPath -ErrorAction SilentlyContinue
        }
    }
}
