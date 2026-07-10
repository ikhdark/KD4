[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$SkipBuild,
    [switch]$AutoSkipBuild,
    [switch]$NoSccache,
    [switch]$SkipPreflightCheck,
    [switch]$BuildOnly,
    [switch]$TestRun,
    [ValidateSet("debug", "release", "local-release")]
    [string]$Profile = "release",
    [switch]$PrintBuiltCodexPath,
    [string]$RepoRoot,
    [string]$SourceExe,
    [string]$SourceCodeModeHostExe,
    [string]$InstallDir = $env:CODEX_LOCAL_PUBLISH_DIR,
    [string]$BackupDir,
    [switch]$RunDoctor,
    [switch]$FastProof,
    [switch]$DoctorOnNoop,
    [switch]$FailOnStaleSourceBuild,
    [switch]$RuntimeProof,
    [switch]$AllowRustyV8Download,
    [string]$RustyV8Archive,
    [switch]$ConfigureDesktopLocalCli,
    [switch]$RestartDesktop,
    [ValidateSet("User", "Process")]
    [string]$DesktopCliEnvironmentTarget = "Process",
    [string]$LocalCodexHome = $env:CODEX_LOCAL_CODEX_HOME,
    [string]$LocalCodexSqliteHome = $env:CODEX_LOCAL_CODEX_SQLITE_HOME,
    [switch]$AllowRunningTarget,
    [ValidateRange(1, 120)]
    [int]$CloseRunningTargetTimeoutSeconds = 10
)

# Relative path parameters intentionally resolve against the caller's current directory.
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$CodexDesktopPackageName = "OpenAI.Codex"
$CodexDesktopAppId = "OpenAI.Codex_2p2nqsd0c76g0!App"
$script:CodexDesktopPackageCacheResolved = $false
$script:CodexDesktopPackageCache = $null
. (Join-Path $PSScriptRoot "common-rust-env.ps1")
. (Join-Path $PSScriptRoot "publish-local-codex.hashing.ps1")

. (Join-Path $PSScriptRoot "publish-local-codex.proof.ps1")
. (Join-Path $PSScriptRoot "publish-local-codex.desktop.ps1")
. (Join-Path $PSScriptRoot "publish-local-codex.build.ps1")
. (Join-Path $PSScriptRoot "publish-local-codex.apply.ps1")


$repoRoot = Get-RepoRoot -Override $RepoRoot
if ($PrintBuiltCodexPath) {
    Write-Output (Get-BuiltCodexPath -RepoRoot $repoRoot -Profile $Profile)
    return
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Get-DefaultInstallDir
}
else {
    $InstallDir = Resolve-AbsolutePath $InstallDir
}

if ([string]::IsNullOrWhiteSpace($LocalCodexHome)) {
    $LocalCodexHome = Get-DefaultLocalCodexHome
}
else {
    $LocalCodexHome = Resolve-AbsolutePath $LocalCodexHome
}

if ([string]::IsNullOrWhiteSpace($LocalCodexSqliteHome)) {
    $LocalCodexSqliteHome = Get-DefaultLocalCodexSqliteHome -LocalCodexHome $LocalCodexHome
}
else {
    $LocalCodexSqliteHome = Resolve-AbsolutePath $LocalCodexSqliteHome
}

$sourceExeWasExplicit = -not [string]::IsNullOrWhiteSpace($SourceExe)
if (-not $sourceExeWasExplicit) {
    $SourceExe = Get-BuiltCodexPath -RepoRoot $repoRoot -Profile $Profile
}
else {
    $SourceExe = Resolve-AbsolutePath $SourceExe
}

if ([string]::IsNullOrWhiteSpace($SourceCodeModeHostExe)) {
    if ($sourceExeWasExplicit) {
        $SourceCodeModeHostExe = Join-Path (Split-Path -Parent $SourceExe) "codex-code-mode-host.exe"
    }
    else {
        $SourceCodeModeHostExe = Get-BuiltCodeModeHostPath -RepoRoot $repoRoot -Profile $Profile
    }
}
else {
    $SourceCodeModeHostExe = Resolve-AbsolutePath $SourceCodeModeHostExe
}

if ([string]::IsNullOrWhiteSpace($BackupDir)) {
    $BackupDir = Join-Path $InstallDir "backups"
}
else {
    $BackupDir = Resolve-AbsolutePath $BackupDir
}

$targetPath = Join-Path $InstallDir "codex.exe"
$codeModeHostTargetPath = Join-Path $InstallDir "codex-code-mode-host.exe"
$backupStamp = (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssfffZ")
$backupPath = Join-Path $BackupDir "codex-$backupStamp.exe"
$codeModeHostBackupPath = Join-Path $BackupDir "codex-code-mode-host-$backupStamp.exe"
$buildStampPath = Get-BuildStampPath -RepoRoot $repoRoot -Profile $Profile

Write-ProofLine "action" $(if ($DryRun) { "DRY-RUN" } elseif ($BuildOnly) { "build-only" } elseif ($TestRun) { "test-run" } else { "publish" })
Write-ProofLine "profile" $Profile
Write-ProofLine "sourceBuildStalePolicy" $(if ($FailOnStaleSourceBuild) { "fail" } else { "report" })
Write-ProofLine "runtimeProof" $(if ($RuntimeProof) { "requested" } else { "not requested" })

if ($BuildOnly -and $SkipBuild) {
    throw "-BuildOnly cannot be combined with -SkipBuild."
}
if ($TestRun -and $DryRun) {
    throw "-TestRun cannot be combined with -DryRun."
}
if ($TestRun -and $BuildOnly) {
    throw "-TestRun cannot be combined with -BuildOnly."
}
if ($TestRun -and $SkipBuild) {
    throw "-TestRun cannot be combined with -SkipBuild."
}

if ($AutoSkipBuild -and -not $SkipBuild -and -not $BuildOnly) {
    $sourceTreeNewestForAutoSkip = Get-SourceNewestWriteUtcForSkipBuild `
        -RepoRoot $repoRoot `
        -StampPath $buildStampPath
    $sourceLastWriteForAutoSkip = Get-FileLastWriteUtc $SourceExe
    $sourceCodeModeHostLastWriteForAutoSkip = Get-FileLastWriteUtc $SourceCodeModeHostExe
    if (
        (Test-Path -LiteralPath $SourceExe -PathType Leaf) -and
        (Test-Path -LiteralPath $SourceCodeModeHostExe -PathType Leaf) -and
        $null -ne $sourceTreeNewestForAutoSkip -and
        $null -ne $sourceLastWriteForAutoSkip -and
        $null -ne $sourceCodeModeHostLastWriteForAutoSkip -and
        -not (Test-FileStaleAgainstSource -SourceNewestUtc $sourceTreeNewestForAutoSkip -FileLastWriteUtc $sourceLastWriteForAutoSkip) -and
        -not (Test-FileStaleAgainstSource -SourceNewestUtc $sourceTreeNewestForAutoSkip -FileLastWriteUtc $sourceCodeModeHostLastWriteForAutoSkip)
    ) {
        $SkipBuild = $true
        Write-ProofLine "autoSkipBuild" "true"
        Write-ProofLine "autoSkipBuildReason" "source build is current for tracked publish inputs"
    }
    else {
        Write-ProofLine "autoSkipBuild" "false"
        if (
            -not (Test-Path -LiteralPath $SourceExe -PathType Leaf) -or
            -not (Test-Path -LiteralPath $SourceCodeModeHostExe -PathType Leaf)
        ) {
            Write-ProofLine "autoSkipBuildReason" "source artifact missing"
        }
        elseif ($null -eq $sourceTreeNewestForAutoSkip) {
            Write-ProofLine "autoSkipBuildReason" "tracked source freshness unknown"
        }
        elseif ($null -eq $sourceLastWriteForAutoSkip) {
            Write-ProofLine "autoSkipBuildReason" "source binary timestamp unavailable"
        }
        else {
            Write-ProofLine "autoSkipBuildReason" "tracked source is newer than source build"
        }
    }
}

if (-not $SkipBuild) {
    Invoke-CodexBuild -RepoRoot $repoRoot -Profile $Profile -DryRun:$DryRun
    if (-not $DryRun) {
        $builtSourceNewestUtc = Get-TrackedSourceNewestWriteUtc -RepoRoot $repoRoot
        Write-BuildStamp `
            -StampPath (Get-BuildStampPath -RepoRoot $repoRoot -Profile $Profile) `
            -SourceNewestUtc $builtSourceNewestUtc
    }
}
else {
    Write-ProofLine "buildCommand" "<skipped>"
}

if ($BuildOnly) {
    Write-ProofLine "buildOnly" "true"
    Write-ProofLine "builtCodexPath" (Get-BuiltCodexPath -RepoRoot $repoRoot -Profile $Profile)
    Write-ProofLine "builtCodeModeHostPath" (Get-BuiltCodeModeHostPath -RepoRoot $repoRoot -Profile $Profile)
    Write-ProofLine "buildStampPath" $buildStampPath
    exit 0
}

if ($TestRun) {
    Write-ProofLine "testRun" "true"
    Write-ProofLine "sourcePath" $SourceExe
    Write-ProofLine "sourceCodeModeHostPath" $SourceCodeModeHostExe
    if (-not (Test-Path -LiteralPath $SourceExe -PathType Leaf)) {
        throw "Test run built source Codex binary does not exist: $SourceExe"
    }
    if (-not (Test-Path -LiteralPath $SourceCodeModeHostExe -PathType Leaf)) {
        throw "Test run built source code-mode host does not exist: $SourceCodeModeHostExe"
    }
    Write-ProofLine "sourceMissing" "false"
    Write-ProofLine "sourceCodeModeHostMissing" "false"
    Write-ProofLine "sourceCodeModeHostSha256" (Get-FileSha256 $SourceCodeModeHostExe)
    Write-VersionProofBlock -Prefix "source" -Path $SourceExe
    if ($RunDoctor) {
        Invoke-DoctorForPublish -TargetPath $SourceExe
    }
    else {
        Write-ProofLine "doctorCommand" "<skipped: -RunDoctor not set>"
    }
    Write-ProofLine "replace" "not run: test run"
    Write-ProofLine "restartRequired" "false"
    exit 0
}

Write-ProofLine "sourcePath" $SourceExe
if (-not (Test-Path -LiteralPath $SourceExe -PathType Leaf)) {
    if (-not $DryRun) {
        throw "Source Codex binary does not exist: $SourceExe"
    }
    Write-ProofLine "sourceMissing" "true"
}
else {
    Write-ProofLine "sourceMissing" "false"
}
Write-ProofLine "sourceCodeModeHostPath" $SourceCodeModeHostExe
if (-not (Test-Path -LiteralPath $SourceCodeModeHostExe -PathType Leaf)) {
    if (-not $DryRun) {
        throw "Source code-mode host does not exist: $SourceCodeModeHostExe"
    }
    Write-ProofLine "sourceCodeModeHostMissing" "true"
}
else {
    Write-ProofLine "sourceCodeModeHostMissing" "false"
}
Write-VersionProofBlock -Prefix "source" -Path $SourceExe
Write-ProofLine "targetPath" $targetPath
Write-ProofLine "codeModeHostTargetPath" $codeModeHostTargetPath
Write-ProofLine "targetKind" "local CLI/TUI payload used by Codex Desktop; launching it directly opens a terminal."
$publishLock = $null
if (-not $DryRun) {
    $publishLock = Enter-CodexLocalPublishMutex
    Write-ProofLine "publishLock" "acquired"
}

try {
$targetBeforeSha256 = Get-FileSha256 $targetPath
$codeModeHostTargetBeforeSha256 = Get-FileSha256 $codeModeHostTargetPath
Write-ProofLine "targetBeforeSha256" $targetBeforeSha256
Write-ProofLine "codeModeHostTargetBeforeSha256" $codeModeHostTargetBeforeSha256
$sourceSha256Mode = "hashed"
$sourceSha256 = Get-FileSha256 $SourceExe
$sourceCodeModeHostSha256 = Get-FileSha256 $SourceCodeModeHostExe
Write-ProofLine "sourceSha256Mode" $sourceSha256Mode
Write-ProofLine "sourceSha256" $sourceSha256
Write-ProofLine "sourceCodeModeHostSha256" $sourceCodeModeHostSha256
$sourceTreeNewestUtc = if ($SkipBuild) {
    Get-SourceNewestWriteUtcForSkipBuild `
        -RepoRoot $repoRoot `
        -StampPath $buildStampPath
}
else {
    Get-BuildStampNewestWriteUtc -StampPath $buildStampPath
}
$sourceLastWriteUtc = Get-FileLastWriteUtc $SourceExe
$sourceCodeModeHostLastWriteUtc = Get-FileLastWriteUtc $SourceCodeModeHostExe
$targetBeforeLastWriteUtc = Get-FileLastWriteUtc $targetPath
$codeModeHostTargetBeforeLastWriteUtc = Get-FileLastWriteUtc $codeModeHostTargetPath
Write-ProofLine "sourceTreeNewestWriteUtc" (Format-UtcTimestamp $sourceTreeNewestUtc)
Write-ProofLine "sourceLastWriteUtc" (Format-UtcTimestamp $sourceLastWriteUtc)
Write-ProofLine "sourceCodeModeHostLastWriteUtc" (Format-UtcTimestamp $sourceCodeModeHostLastWriteUtc)
Write-ProofLine "targetBeforeLastWriteUtc" (Format-UtcTimestamp $targetBeforeLastWriteUtc)
Write-ProofLine "codeModeHostTargetBeforeLastWriteUtc" (Format-UtcTimestamp $codeModeHostTargetBeforeLastWriteUtc)
$codexSourceBuildStale = Test-FileStaleAgainstSource `
    -SourceNewestUtc $sourceTreeNewestUtc `
    -FileLastWriteUtc $sourceLastWriteUtc
$codeModeHostSourceBuildStale = Test-FileStaleAgainstSource `
    -SourceNewestUtc $sourceTreeNewestUtc `
    -FileLastWriteUtc $sourceCodeModeHostLastWriteUtc
$sourceBuildStale = $codexSourceBuildStale -or $codeModeHostSourceBuildStale
Write-ProofLine "codexSourceBuildStale" $codexSourceBuildStale
Write-ProofLine "codeModeHostSourceBuildStale" $codeModeHostSourceBuildStale
Write-ProofLine "sourceBuildStale" $sourceBuildStale
if ($sourceBuildStale) {
    Write-ProofLine "sourceBuildStaleRemedy" "Run just publish-local-codex -Profile $Profile -RunDoctor without -SkipBuild, then restart Codex Desktop."
}
$codexTargetBeforeStale = Test-FileStaleAgainstSource `
    -SourceNewestUtc $sourceLastWriteUtc `
    -FileLastWriteUtc $targetBeforeLastWriteUtc
$codeModeHostTargetBeforeStale = Test-FileStaleAgainstSource `
    -SourceNewestUtc $sourceCodeModeHostLastWriteUtc `
    -FileLastWriteUtc $codeModeHostTargetBeforeLastWriteUtc
$targetBeforeStale = $codexTargetBeforeStale -or $codeModeHostTargetBeforeStale
Write-ProofLine "codexTargetBeforeStale" $codexTargetBeforeStale
Write-ProofLine "codeModeHostTargetBeforeStale" $codeModeHostTargetBeforeStale
Write-ProofLine "targetBeforeStale" $targetBeforeStale
if ($targetBeforeStale) {
    Write-ProofLine "targetBeforeStaleRemedy" "Run just publish-local-codex and restart Codex Desktop."
}
$script:RunningTargetProcessProbeError = $null
$runningCodexProcesses = @(Get-RunningCodexTargetProcesses -TargetPath $targetPath)
$runningCodeModeHostProcesses = @(Get-RunningCodexTargetProcesses -TargetPath $codeModeHostTargetPath)
$allRunningTargetProcesses = @($runningCodexProcesses) + @($runningCodeModeHostProcesses)
$runningTargetProcesses = @($allRunningTargetProcesses | Sort-Object -Property Id -Unique)
if (-not [string]::IsNullOrWhiteSpace($script:RunningTargetProcessProbeError)) {
    Write-ProofLine "runningTargetProcesses" "<unavailable: $script:RunningTargetProcessProbeError>"
}
elseif ($runningTargetProcesses.Count -gt 0) {
    Write-ProofLine "runningTargetProcesses" (Format-ProcessProof $runningTargetProcesses)
}
else {
    Write-ProofLine "runningTargetProcesses" "<none>"
}

$targetExists = Test-Path -LiteralPath $targetPath -PathType Leaf
$codeModeHostTargetExists = Test-Path -LiteralPath $codeModeHostTargetPath -PathType Leaf
$codexBinaryChanged = -not (
    $targetExists -and
    $sourceSha256 -ne "<missing>" -and
    [string]::Equals($sourceSha256, $targetBeforeSha256, [System.StringComparison]::OrdinalIgnoreCase)
)
$codeModeHostBinaryChanged = -not (
    $codeModeHostTargetExists -and
    $sourceCodeModeHostSha256 -ne "<missing>" -and
    [string]::Equals(
        $sourceCodeModeHostSha256,
        $codeModeHostTargetBeforeSha256,
        [System.StringComparison]::OrdinalIgnoreCase
    )
)
$binaryChanged = $codexBinaryChanged -or $codeModeHostBinaryChanged
Write-ProofLine "codexBinaryChanged" $(if ($codexBinaryChanged) { "true" } else { "false" })
Write-ProofLine "codeModeHostBinaryChanged" $(if ($codeModeHostBinaryChanged) { "true" } else { "false" })
Write-ProofLine "binaryChanged" $(if ($binaryChanged) { "true" } else { "false" })
if (
    $binaryChanged -and
    -not $DryRun -and
    -not $AllowRunningTarget -and
    -not [string]::IsNullOrWhiteSpace($script:RunningTargetProcessProbeError)
) {
    throw "Cannot safely publish because running-target process detection failed: $script:RunningTargetProcessProbeError. Rerun after fixing process access, or explicitly use -AllowRunningTarget."
}
Write-DesktopProof -BinaryChanged $binaryChanged -FastProof:$FastProof
if ($sourceBuildStale) {
    Write-ProofLine "publishReadiness" "blocked: source build stale"
}
elseif ($targetBeforeStale) {
    Write-ProofLine "publishReadiness" "needs publish: target older than source build"
}
elseif ($binaryChanged) {
    Write-ProofLine "publishReadiness" "needs publish: target differs from source build"
}
else {
    Write-ProofLine "publishReadiness" "ready: target already current"
}
if ($binaryChanged) {
    Write-VersionProofBlock -Prefix "targetBefore" -Path $targetPath
}
$skipBuildBlockedByStaleSource = $SkipBuild -and $sourceBuildStale
if ($targetExists) {
    Write-ProofLine "backupPath" $backupPath
}
else {
    Write-ProofLine "backupPath" "<none: target missing>"
}
if ($codeModeHostTargetExists) {
    Write-ProofLine "codeModeHostBackupPath" $codeModeHostBackupPath
}
else {
    Write-ProofLine "codeModeHostBackupPath" "<none: target missing>"
}

$desktopRoutingResult = [pscustomobject]@{
    Changed = $false
    RestartRequired = $false
}
if ($ConfigureDesktopLocalCli) {
    if ($skipBuildBlockedByStaleSource -and -not $DryRun) {
        Write-ProofLine "desktopLocalCliRouting" "skipped: source build stale"
    }
    else {
        Sync-OfficialDesktopEnvironmentCleanup -DryRun:$DryRun
        Sync-DesktopLocalCliRouting `
            -TargetPath $targetPath `
            -InstallDir $InstallDir `
            -LocalCodexHome $LocalCodexHome `
            -LocalCodexSqliteHome $LocalCodexSqliteHome `
            -DryRun:$DryRun `
            -EnvironmentTarget $DesktopCliEnvironmentTarget `
            -Result ([ref]$desktopRoutingResult)
    }
}

if ($DryRun) {
    $staleSourceBuildFailureMessage = $null
    if ($skipBuildBlockedByStaleSource) {
        Write-ProofLine "replace" "not run: source build stale"
        Write-ProofLine "restartRequired" "unknown until rebuild"
        if ($FailOnStaleSourceBuild) {
            $staleSourceBuildFailureMessage = "Dry-run source build is stale. Run just publish-local-codex -Profile $Profile -RunDoctor without -SkipBuild, then restart Codex Desktop."
        }
    }
    else {
        Write-ProofLine "replace" $(if ($binaryChanged) { "not run" } else { "not run: target already current" })
        Write-ProofLine "restartRequired" $(if ($binaryChanged -or $desktopRoutingResult.RestartRequired) { "true" } else { "false" })
    }
    if ($RunDoctor) {
        if ($RuntimeProof -and (Test-Path -LiteralPath $targetPath -PathType Leaf)) {
            Invoke-DoctorForPublish -TargetPath $targetPath
        }
        elseif ($RuntimeProof) {
            Write-ProofLine "doctorCommand" "`"$targetPath`" doctor --json (not run: target missing)"
            Write-ProofLine "doctorStatus" "skipped: target missing"
        }
        else {
            Write-ProofLine "doctorCommand" "`"$targetPath`" doctor --json (not run)"
        }
    }
    if ($RestartDesktop) {
        if ($ConfigureDesktopLocalCli) {
            Restart-CodexDesktop -DryRun -LocalCliPath $targetPath -LocalCodexHome $LocalCodexHome -LocalCodexSqliteHome $LocalCodexSqliteHome
        }
        else {
            Restart-CodexDesktop -DryRun
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($staleSourceBuildFailureMessage)) {
        throw $staleSourceBuildFailureMessage
    }
    exit 0
}

if ($skipBuildBlockedByStaleSource) {
    Write-ProofLine "replace" "blocked: source build stale"
    Write-ProofLine "restartRequired" "unknown until rebuild"
    throw "SkipBuild cannot publish the newest Codex bundle because tracked source files are newer than one or more source artifacts. Run just publish-local-codex -Profile $Profile -RunDoctor without -SkipBuild."
}

if ($runningTargetProcesses.Count -gt 0 -and $binaryChanged -and -not $AllowRunningTarget) {
    Stop-RunningCodexTargetProcesses `
        -Processes $runningTargetProcesses `
        -TimeoutSeconds $CloseRunningTargetTimeoutSeconds
    $runningCodexProcesses = @(Get-RunningCodexTargetProcesses -TargetPath $targetPath)
    $runningCodeModeHostProcesses = @(Get-RunningCodexTargetProcesses -TargetPath $codeModeHostTargetPath)
    $allRunningTargetProcesses = @($runningCodexProcesses) + @($runningCodeModeHostProcesses)
    $runningTargetProcesses = @($allRunningTargetProcesses | Sort-Object -Property Id -Unique)
    if ($runningTargetProcesses.Count -gt 0) {
        throw "Target Codex publish bundle is still running ($((Format-ProcessProof $runningTargetProcesses))). Close Codex Desktop or rerun with -AllowRunningTarget."
    }
    Write-ProofLine "runningTargetProcessesAfterClose" "<none>"
}
elseif ($runningTargetProcesses.Count -gt 0 -and -not $binaryChanged) {
    Write-ProofLine "closeRunningTarget" "skipped: target already current"
}
elseif ($runningTargetProcesses.Count -gt 0 -and $AllowRunningTarget) {
    Write-ProofLine "closeRunningTarget" "skipped: -AllowRunningTarget"
}

if (-not $binaryChanged) {
    Write-ProofLine "replace" "skipped: target already current"
    Write-ProofLine "targetSha256" $targetBeforeSha256
    Write-ProofLine "codeModeHostTargetSha256" $codeModeHostTargetBeforeSha256
    Write-ProofLine "backupSha256" "<none: target already current>"
    Write-ProofLine "codeModeHostBackupSha256" "<none: target already current>"
    Write-ProofLine "restartRequired" $(if ($desktopRoutingResult.RestartRequired) { "true" } else { "false" })
    if ($RunDoctor) {
        if ($DoctorOnNoop) {
            Invoke-DoctorForPublish -TargetPath $targetPath
        }
        else {
            Write-ProofLine "doctorCommand" "<skipped: target already current>"
        }
    }
    if ($RestartDesktop) {
        if ($ConfigureDesktopLocalCli) {
            Restart-CodexDesktop -LocalCliPath $targetPath -LocalCodexHome $LocalCodexHome -LocalCodexSqliteHome $LocalCodexSqliteHome
        }
        else {
            Restart-CodexDesktop
        }
    }
    exit 0
}

$publishedCodeModeHost = $false
$publishedCodex = $false
try {
    if ($codeModeHostBinaryChanged) {
        Publish-CodexBinary `
            -SourcePath $SourceCodeModeHostExe `
            -TargetPath $codeModeHostTargetPath `
            -BackupPath $codeModeHostBackupPath
        $publishedCodeModeHost = $true
    }
    if ($codexBinaryChanged) {
        Publish-CodexBinary -SourcePath $SourceExe -TargetPath $targetPath -BackupPath $backupPath
        $publishedCodex = $true
    }

    $targetVersionLines = @(Get-VersionProofLines -Path $targetPath -TimeoutMilliseconds 10000 -Attempts 2)
    Write-VersionProofLinesBlock -Prefix "target" -VersionLines $targetVersionLines
    if (-not (Test-VersionProofAvailable -VersionLines $targetVersionLines)) {
        throw "Published Codex binary failed --version verification: $($targetVersionLines[0])"
    }
    Write-ProofLine "postPublishVerify" "version ok"
    $targetSha256 = Get-FileSha256 $targetPath
    $codeModeHostTargetSha256 = Get-FileSha256 $codeModeHostTargetPath
    if (-not [string]::Equals(
            $sourceSha256,
            $targetSha256,
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
        throw "Published Codex binary failed SHA-256 verification."
    }
    Write-ProofLine "codexPostPublishVerify" "sha256 ok"
    if (-not [string]::Equals(
            $sourceCodeModeHostSha256,
            $codeModeHostTargetSha256,
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
        throw "Published code-mode host failed SHA-256 verification."
    }
    Write-ProofLine "codeModeHostPostPublishVerify" "sha256 ok"
    Write-ProofLine "targetSha256" $targetSha256
    Write-ProofLine "codeModeHostTargetSha256" $codeModeHostTargetSha256
    if (-not $codexBinaryChanged) {
        Write-ProofLine "backupSha256" "<none: target already current>"
    }
    elseif (-not $targetExists) {
        Write-ProofLine "backupSha256" "<none: target missing>"
    }
    else {
        Write-ProofLine "backupSha256" $targetBeforeSha256
        Write-ProofLine "rollbackCommand" "Copy-Item -LiteralPath `"$backupPath`" -Destination `"$targetPath`" -Force"
    }
    if (-not $codeModeHostBinaryChanged) {
        Write-ProofLine "codeModeHostBackupSha256" "<none: target already current>"
    }
    elseif (-not $codeModeHostTargetExists) {
        Write-ProofLine "codeModeHostBackupSha256" "<none: target missing>"
    }
    else {
        Write-ProofLine "codeModeHostBackupSha256" $codeModeHostTargetBeforeSha256
        Write-ProofLine "codeModeHostRollbackCommand" "Copy-Item -LiteralPath `"$codeModeHostBackupPath`" -Destination `"$codeModeHostTargetPath`" -Force"
    }
    Write-ProofLine "restartRequired" "true"
    Write-ProofLine "restart" "Restart Codex Desktop from the Start menu or desktopAppLaunchCommand; do not launch targetPath directly."

    if ($RunDoctor) {
        Invoke-DoctorForPublish -TargetPath $targetPath
    }
    if ($RestartDesktop) {
        if ($ConfigureDesktopLocalCli) {
            Restart-CodexDesktop -LocalCliPath $targetPath -LocalCodexHome $LocalCodexHome -LocalCodexSqliteHome $LocalCodexSqliteHome
        }
        else {
            Restart-CodexDesktop
        }
    }
}
catch {
    $publishError = $_.Exception
    Write-ProofLine "rollback" "requested: $($publishError.Message)"
    $rollbackFailures = [System.Collections.Generic.List[string]]::new()
    if ($publishedCodex) {
        try {
            Restore-CodexBinaryPublish `
                -TargetPath $targetPath `
                -BackupPath $backupPath `
                -HadPreviousTarget $targetExists
        }
        catch {
            $rollbackFailures.Add("codex.exe: $($_.Exception.Message)")
        }
    }
    if ($publishedCodeModeHost) {
        try {
            Restore-CodexBinaryPublish `
                -TargetPath $codeModeHostTargetPath `
                -BackupPath $codeModeHostBackupPath `
                -HadPreviousTarget $codeModeHostTargetExists `
                -ProofPrefix "codeModeHost"
        }
        catch {
            $rollbackFailures.Add("codex-code-mode-host.exe: $($_.Exception.Message)")
        }
    }
    if ($rollbackFailures.Count -gt 0) {
        throw "Publish failed: $($publishError.Message) Rollback also failed: $($rollbackFailures -join '; ')"
    }
    throw $publishError
}

$protectedBackupPath = if ($targetExists) { $backupPath } else { $null }
$protectedCodeModeHostBackupPath = if ($codeModeHostTargetExists) { $codeModeHostBackupPath } else { $null }
Remove-OldCodexBackups `
    -BackupDir $BackupDir `
    -ArtifactName "codex" `
    -ProtectedPath $protectedBackupPath
Remove-OldCodexBackups `
    -BackupDir $BackupDir `
    -ArtifactName "codex-code-mode-host" `
    -ProtectedPath $protectedCodeModeHostBackupPath

Write-Output "Restart Codex Desktop from the Start menu or run: $(Get-CodexDesktopLaunchCommand)"
Write-Output "Published codex.exe and codex-code-mode-host.exe as one local runtime bundle."
Write-Output "Do not launch targetPath directly; it is the CLI/TUI payload and opens a terminal."
}
finally {
    Exit-CodexLocalPublishMutex -Lock $publishLock
}
