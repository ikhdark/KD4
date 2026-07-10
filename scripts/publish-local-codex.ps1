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

function Get-RepoRoot {
    param(
        [AllowNull()]
        [string]$Override
    )

    if (-not [string]::IsNullOrWhiteSpace($Override)) {
        return [System.IO.Path]::GetFullPath($Override)
    }

    return [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
}

function Resolve-AbsolutePath {
    param(
        [string]$Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "Path cannot be empty."
    }

    return [System.IO.Path]::GetFullPath($Path)
}

function Get-DefaultInstallDir {
    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_LOCAL_PUBLISH_DIR)) {
        return [System.IO.Path]::GetFullPath($env:CODEX_LOCAL_PUBLISH_DIR)
    }

    if ([string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        throw "USERPROFILE is not set. Pass -InstallDir explicitly."
    }

    return [System.IO.Path]::GetFullPath(
        (Join-Path $env:USERPROFILE "Desktop\LOCAL-KD")
    )
}

function Get-DefaultLocalCodexHome {
    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_LOCAL_CODEX_HOME)) {
        return [System.IO.Path]::GetFullPath($env:CODEX_LOCAL_CODEX_HOME)
    }

    if ([string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        throw "USERPROFILE is not set. Pass -LocalCodexHome explicitly."
    }

    return [System.IO.Path]::GetFullPath(
        (Join-Path $env:USERPROFILE "Desktop\LOCAL-KD")
    )
}

function Get-DefaultLocalCodexSqliteHome {
    param(
        [string]$LocalCodexHome
    )

    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_LOCAL_CODEX_SQLITE_HOME)) {
        return [System.IO.Path]::GetFullPath($env:CODEX_LOCAL_CODEX_SQLITE_HOME)
    }

    if ([string]::IsNullOrWhiteSpace($LocalCodexHome)) {
        throw "LocalCodexHome is not set. Pass -LocalCodexSqliteHome explicitly."
    }

    return [System.IO.Path]::GetFullPath((Join-Path $LocalCodexHome "sqlite"))
}

function Get-BuiltCodexPath {
    param(
        [string]$RepoRoot,
        [string]$Profile
    )

    $profileDir = if ($Profile -eq "debug") { "debug" } else { $Profile }
    return Join-Path $RepoRoot "codex-rs\target\publish-$Profile\$profileDir\codex.exe"
}

function Get-BuiltCodeModeHostPath {
    param(
        [string]$RepoRoot,
        [string]$Profile
    )

    $profileDir = if ($Profile -eq "debug") { "debug" } else { $Profile }
    return Join-Path $RepoRoot "codex-rs\target\publish-$Profile\$profileDir\codex-code-mode-host.exe"
}

function Get-FileLastWriteUtc {
    param(
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return $null
    }

    return (Get-Item -LiteralPath $Path).LastWriteTimeUtc
}

function Format-UtcTimestamp {
    param(
        [AllowNull()]
        [object]$Value
    )

    if ($null -eq $Value) {
        return "<missing>"
    }

    return ([DateTime]$Value).ToString("o")
}

function Test-DoctorFailureAllowedForPublish {
    param(
        [object[]]$OutputLines
    )

    $doctor = ConvertFrom-DoctorOutput -OutputLines $OutputLines
    if ($null -eq $doctor) {
        return $false
    }

    # Use PSObject.Properties lookups: direct member access on missing JSON
    # properties throws under Set-StrictMode -Version Latest.
    $checksProperty = $doctor.PSObject.Properties["checks"]
    if ($null -eq $checksProperty -or $null -eq $checksProperty.Value) {
        return $false
    }

    $failedChecks = @(
        $checksProperty.Value.PSObject.Properties |
            Where-Object {
                $null -ne $_.Value -and
                $null -ne $_.Value.PSObject.Properties["status"] -and
                [string]$_.Value.PSObject.Properties["status"].Value -eq "fail"
            } |
            ForEach-Object { $_.Name }
    )

    return $failedChecks.Count -eq 1 -and $failedChecks[0] -eq "auth.credentials"
}

function ConvertFrom-DoctorOutput {
    param(
        [object[]]$OutputLines
    )

    $doctorText = ($OutputLines | ForEach-Object { [string]$_ }) -join "`n"
    if ([string]::IsNullOrWhiteSpace($doctorText)) {
        return $null
    }

    try {
        return $doctorText | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        return $null
    }
}

function Write-DoctorCheckProofLine {
    param(
        [AllowNull()]
        [object]$Doctor,
        [string]$CheckId,
        [string]$Prefix
    )

    # Use PSObject.Properties lookups: direct member access on missing JSON
    # properties throws under Set-StrictMode -Version Latest.
    if ($null -eq $Doctor) {
        return
    }
    $checksProperty = $Doctor.PSObject.Properties["checks"]
    if ($null -eq $checksProperty -or $null -eq $checksProperty.Value) {
        return
    }

    $property = $checksProperty.Value.PSObject.Properties[$CheckId]
    if ($null -eq $property -or $null -eq $property.Value) {
        return
    }

    $check = $property.Value
    $statusProperty = $check.PSObject.Properties["status"]
    if ($null -ne $statusProperty -and $null -ne $statusProperty.Value) {
        Write-ProofLine "$($Prefix)Status" ([string]$statusProperty.Value)
    }
    $summaryProperty = $check.PSObject.Properties["summary"]
    if ($null -ne $summaryProperty -and -not [string]::IsNullOrWhiteSpace([string]$summaryProperty.Value)) {
        Write-ProofLine "$($Prefix)Summary" ([string]$summaryProperty.Value)
    }
}

function Write-DoctorRuntimeProofLines {
    param(
        [AllowNull()]
        [object]$Doctor
    )

    Write-DoctorCheckProofLine -Doctor $Doctor -CheckId "local_publish.readiness" -Prefix "doctorLocalPublish"
    Write-DoctorCheckProofLine -Doctor $Doctor -CheckId "desktop.runtime_chain" -Prefix "doctorDesktopRuntime"
    Write-DoctorCheckProofLine -Doctor $Doctor -CheckId "app_server.status" -Prefix "doctorAppServer"
}

function Invoke-DoctorForPublish {
    param(
        [string]$TargetPath
    )

    Write-ProofLine "doctorCommand" "`"$TargetPath`" doctor --json"
    # Windows PowerShell 5.1 turns redirected native stderr into terminating
    # errors while $ErrorActionPreference is "Stop"; doctor stderr must reach
    # the allowed-failure check below instead of aborting into rollback.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $doctorOutput = @(& $TargetPath doctor --json 2>&1)
        $doctorExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    foreach ($line in $doctorOutput) {
        Write-Output $line
    }

    $doctor = ConvertFrom-DoctorOutput -OutputLines $doctorOutput
    Write-DoctorRuntimeProofLines -Doctor $doctor

    if ($doctorExitCode -eq 0) {
        Write-ProofLine "doctorStatus" "ok"
        return
    }

    if (Test-DoctorFailureAllowedForPublish -OutputLines $doctorOutput) {
        Write-ProofLine "doctorStatus" "warning: auth.credentials missing"
        return
    }

    throw "codex doctor --json failed with exit code $doctorExitCode."
}

function Test-LocalPublishBuildRelevantPath {
    param(
        [string]$Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }

    $normalized = $Path.Trim('"') -replace "\\", "/"
    return (
        $normalized -like "codex-rs/*" -or
        $normalized -eq "justfile" -or
        $normalized -like "scripts/publish-local-codex*.ps1" -or
        $normalized -eq "scripts/common-rust-env.ps1"
    ) -and
    $normalized -notlike "codex-rs/target/*"
}

function Get-TrackedSourceNewestWriteUtc {
    param(
        [string]$RepoRoot,
        [switch]$RequireStatusScan
    )

    $git = Get-Command git -ErrorAction SilentlyContinue
    if (-not $git) {
        return $null
    }

    $newest = $null

    # Windows PowerShell 5.1 turns redirected native stderr into terminating
    # errors while $ErrorActionPreference is "Stop"; git failures here must
    # fall back to "freshness unknown" instead of aborting.
    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $trackedFiles = & $git.Source -C $RepoRoot ls-files -- codex-rs scripts/publish-local-codex.ps1 scripts/publish-local-codex.hashing.ps1 scripts/common-rust-env.ps1 justfile 2>$null
        $trackedFilesExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if ($trackedFilesExitCode -eq 0) {
        foreach ($file in $trackedFiles) {
            if (-not (Test-LocalPublishBuildRelevantPath -Path $file)) {
                continue
            }
            $path = Join-Path $RepoRoot (($file -replace "/", [System.IO.Path]::DirectorySeparatorChar))
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
                continue
            }

            $lastWrite = (Get-Item -LiteralPath $path).LastWriteTimeUtc
            if ($null -eq $newest -or $lastWrite -gt $newest) {
                $newest = $lastWrite
            }
        }
    }

    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $changedFiles = & $git.Source -C $RepoRoot status --porcelain=v1 -uall -- 2>$null
        $changedFilesExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if ($changedFilesExitCode -ne 0) {
        if ($RequireStatusScan) {
            return $null
        }
        return $newest
    }

    foreach ($entry in $changedFiles) {
        if ([string]::IsNullOrWhiteSpace($entry) -or $entry.Length -lt 4) {
            continue
        }
        $statusCode = $entry.Substring(0, 2)
        $file = $entry.Substring(3).Trim()
        if ($file -match " -> ") {
            $file = ($file -split " -> ", 2)[1]
        }
        $file = $file.Trim('"')
        if (-not (Test-LocalPublishBuildRelevantPath -Path $file)) {
            continue
        }

        $path = Join-Path $RepoRoot (($file -replace "/", [System.IO.Path]::DirectorySeparatorChar))
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            if ($statusCode.Contains("D")) {
                $deletedWrite = [DateTime]::UtcNow
                if ($null -eq $newest -or $deletedWrite -gt $newest) {
                    $newest = $deletedWrite
                }
            }
            continue
        }

        $lastWrite = (Get-Item -LiteralPath $path).LastWriteTimeUtc
        if ($null -eq $newest -or $lastWrite -gt $newest) {
            $newest = $lastWrite
        }
    }

    return $newest
}

function Get-SourceNewestWriteUtcForSkipBuild {
    param(
        [string]$RepoRoot,
        [string]$StampPath
    )

    $liveNewestUtc = Get-TrackedSourceNewestWriteUtc -RepoRoot $RepoRoot -RequireStatusScan
    if ($null -ne $liveNewestUtc) {
        return $liveNewestUtc
    }

    return Get-BuildStampNewestWriteUtc -StampPath $StampPath
}

function Get-BuildStampPath {
    param(
        [string]$RepoRoot,
        [string]$Profile
    )

    return Join-Path $RepoRoot (Join-Path "codex-rs\target" "codex-local-publish-$Profile.stamp")
}

function Get-BuildStampNewestWriteUtc {
    param(
        [string]$StampPath
    )

    if (-not (Test-Path -LiteralPath $StampPath -PathType Leaf)) {
        return $null
    }

    try {
        $stamp = [IO.File]::ReadAllText($StampPath).Trim()
        if ([string]::IsNullOrWhiteSpace($stamp)) {
            return $null
        }
        return [DateTime]::ParseExact(
            $stamp,
            "o",
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind
        )
    }
    catch {
        return $null
    }
}

function Write-BuildStamp {
    param(
        [string]$StampPath,
        [AllowNull()]
        [object]$SourceNewestUtc
    )

    if ($null -eq $SourceNewestUtc) {
        return
    }

    $parent = Split-Path -Parent $StampPath
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    [IO.File]::WriteAllText($StampPath, ([DateTime]$SourceNewestUtc).ToString("o"))
}

function Test-FileStaleAgainstSource {
    param(
        [AllowNull()]
        [object]$SourceNewestUtc,
        [AllowNull()]
        [object]$FileLastWriteUtc
    )

    if ($null -eq $SourceNewestUtc -or $null -eq $FileLastWriteUtc) {
        return $false
    }

    return ([DateTime]$SourceNewestUtc) -gt ([DateTime]$FileLastWriteUtc).AddSeconds(1)
}

function Get-VersionProof {
    param(
        [string]$Path
    )

    $lines = Get-VersionProofLines -Path $Path
    return [string]$lines[0]
}

function Get-VersionProofLines {
    param(
        [string]$Path,
        [int]$TimeoutMilliseconds = 2000,
        [int]$Attempts = 1
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        return @("<missing>")
    }

    $attemptCount = [Math]::Max(1, $Attempts)
    for ($attempt = 1; $attempt -le $attemptCount; $attempt += 1) {
        $process = $null
        try {
            $process = [System.Diagnostics.Process]::new()
            $process.StartInfo.FileName = $Path
            $process.StartInfo.Arguments = "--version"
            $process.StartInfo.RedirectStandardOutput = $true
            $process.StartInfo.RedirectStandardError = $true
            $process.StartInfo.UseShellExecute = $false
            $process.StartInfo.CreateNoWindow = $true

            if (-not $process.Start()) {
                return @("<unavailable: failed to start>")
            }

            if (-not $process.WaitForExit($TimeoutMilliseconds)) {
                try {
                    $process.Kill()
                }
                catch {
                }
                if ($attempt -lt $attemptCount) {
                    Start-Sleep -Milliseconds 250
                    continue
                }
                return @("<unavailable: timed out running --version>")
            }

            $exitCode = $process.ExitCode
            $output = @(
                $process.StandardOutput.ReadToEnd()
                $process.StandardError.ReadToEnd()
            )
            $lines = @($output -split "\r?\n" | Where-Object {
                    -not [string]::IsNullOrWhiteSpace([string]$_)
                } | ForEach-Object { [string]$_ })

            if ($exitCode -eq 0 -and $lines.Count -gt 0) {
                return $lines
            }

            if ($lines.Count -gt 0) {
                return @("<unavailable: exit ${exitCode}: $($lines[0])>")
            }

            return @("<unavailable: exit $exitCode>")
        }
        catch {
            return @("<unavailable: $($_.Exception.Message)>")
        }
        finally {
            if ($null -ne $process) {
                $process.Dispose()
            }
        }
    }
}

function Get-VersionDetail {
    param(
        [string[]]$VersionLines,
        [string]$Name
    )

    foreach ($line in $VersionLines) {
        if ($line -match "^\s*$([regex]::Escape($Name))\s*:\s*(.+?)\s*$") {
            return $Matches[1]
        }
    }

    return "<missing>"
}

function Test-VersionProofAvailable {
    param(
        [string[]]$VersionLines
    )

    if ($VersionLines.Count -eq 0) {
        return $false
    }

    return -not (
        $VersionLines[0].StartsWith("<missing>") -or
        $VersionLines[0].StartsWith("<unavailable:")
    )
}

function Write-VersionProofLinesBlock {
    param(
        [string]$Prefix,
        [string[]]$VersionLines
    )

    Write-ProofLine "$($Prefix)Version" $VersionLines[0]
    Write-ProofLine "$($Prefix)Commit" (Get-VersionDetail -VersionLines $VersionLines -Name "commit")
    Write-ProofLine "$($Prefix)Dirty" (Get-VersionDetail -VersionLines $VersionLines -Name "dirty")
    Write-ProofLine "$($Prefix)Profile" (Get-VersionDetail -VersionLines $VersionLines -Name "profile")
    Write-ProofLine "$($Prefix)Built" (Get-VersionDetail -VersionLines $VersionLines -Name "built")
}

function Write-VersionProofBlock {
    param(
        [string]$Prefix,
        [string]$Path
    )

    $versionLines = @(Get-VersionProofLines -Path $Path)
    Write-VersionProofLinesBlock -Prefix $Prefix -VersionLines $versionLines
}

function Get-RunningCodexTargetProcesses {
    param(
        [string]$TargetPath
    )

    $targetFullPath = [System.IO.Path]::GetFullPath($TargetPath)
    $processName = [System.IO.Path]::GetFileNameWithoutExtension($targetFullPath)
    try {
        return @(
            Get-Process -Name $processName -ErrorAction SilentlyContinue |
                Where-Object {
                    -not [string]::IsNullOrWhiteSpace($_.Path) -and
                    [string]::Equals(
                        [System.IO.Path]::GetFullPath($_.Path),
                        $targetFullPath,
                        [System.StringComparison]::OrdinalIgnoreCase
                    )
                } |
                ForEach-Object {
                    [pscustomobject]@{
                        Id = $_.Id
                        Path = $_.Path
                    }
                }
        )
    }
    catch {
        $script:RunningTargetProcessProbeError = $_.Exception.Message
        return @()
    }
}

function Format-ProcessProof {
    param(
        [object[]]$Processes
    )

    if ($Processes.Count -eq 0) {
        return "<none>"
    }

    return (($Processes | ForEach-Object { "pid=$($_.Id)" }) -join ", ")
}

function Get-LiveProcessesById {
    param(
        [int[]]$Ids
    )

    return @(
        foreach ($id in $Ids) {
            try {
                Get-Process -Id $id -ErrorAction Stop
            }
            catch {
            }
        }
    )
}

function Stop-RunningCodexTargetProcesses {
    param(
        [object[]]$Processes,
        [int]$TimeoutSeconds
    )

    $ids = @($Processes | ForEach-Object { [int]$_.Id })
    if ($ids.Count -eq 0) {
        return
    }

    Write-ProofLine "closeRunningTarget" "requested: $(Format-ProcessProof $Processes); timeoutSeconds=$TimeoutSeconds"

    foreach ($id in $ids) {
        try {
            $process = Get-Process -Id $id -ErrorAction Stop
            try {
                if ($process.MainWindowHandle -eq [IntPtr]::Zero) {
                    continue
                }
                [void]$process.CloseMainWindow()
                [void]$process.WaitForExit($TimeoutSeconds * 1000)
            }
            finally {
                $process.Dispose()
            }
        }
        catch {
        }
    }

    $liveProcesses = @(Get-LiveProcessesById -Ids $ids)
    if ($liveProcesses.Count -gt 0) {
        Write-ProofLine "closeRunningTargetForce" (Format-ProcessProof $liveProcesses)
        foreach ($process in $liveProcesses) {
            try {
                Stop-Process -Id $process.Id -Force -ErrorAction Stop
            }
            catch {
            }
            finally {
                $process.Dispose()
            }
        }
    }

    $forceDeadline = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $forceDeadline) {
        $remainingProcesses = @(Get-LiveProcessesById -Ids $ids)
        if ($remainingProcesses.Count -eq 0) {
            Write-ProofLine "closeRunningTargetResult" "closed"
            return
        }

        foreach ($process in $remainingProcesses) {
            $process.Dispose()
        }
        Start-Sleep -Milliseconds 200
    }

    $remainingProcesses = @(Get-LiveProcessesById -Ids $ids)
    if ($remainingProcesses.Count -eq 0) {
        Write-ProofLine "closeRunningTargetResult" "closed"
        return
    }

    try {
        throw "Failed to close target Codex binary after $TimeoutSeconds seconds ($((Format-ProcessProof $remainingProcesses)))."
    }
    finally {
        foreach ($process in $remainingProcesses) {
            $process.Dispose()
        }
    }
}

function Format-ProofValue {
    param(
        [object]$Value
    )

    if ($null -eq $Value) {
        return ""
    }

    return (([string]$Value) -replace "\s+", " ").Trim()
}

function Write-ProofLine {
    param(
        [string]$Name,
        [object]$Value
    )

    Write-Output "$Name`: $(Format-ProofValue $Value)"
}

function Get-CodexDesktopLaunchCommand {
    return "explorer.exe shell:AppsFolder\$CodexDesktopAppId"
}

function Get-CodexDesktopPackageProof {
    $package = Get-CodexDesktopPackage
    if ($package -is [string]) {
        return $package
    }

    if ($null -eq $package) {
        return "<not installed>"
    }

    return "$($package.PackageFullName); installLocation=$($package.InstallLocation)"
}

function Get-CodexDesktopPackage {
    if ($script:CodexDesktopPackageCacheResolved) {
        return $script:CodexDesktopPackageCache
    }

    $script:CodexDesktopPackageCacheResolved = $true
    $getAppxPackage = Get-Command Get-AppxPackage -ErrorAction SilentlyContinue
    if ($null -eq $getAppxPackage) {
        $script:CodexDesktopPackageCache = "<unavailable: Get-AppxPackage not found>"
        return $script:CodexDesktopPackageCache
    }

    try {
        $packages = @(Get-AppxPackage -Name $CodexDesktopPackageName -ErrorAction Stop)
    }
    catch {
        $script:CodexDesktopPackageCache = "<unavailable: $($_.Exception.Message)>"
        return $script:CodexDesktopPackageCache
    }

    if ($packages.Count -eq 0) {
        $script:CodexDesktopPackageCache = $null
        return $null
    }

    $script:CodexDesktopPackageCache = $packages |
        Sort-Object -Property Version -Descending |
        Select-Object -First 1

    return $script:CodexDesktopPackageCache
}

function Get-CodexDesktopExecutableProof {
    $package = Get-CodexDesktopPackage
    if ($package -is [string]) {
        return $package
    }

    if ($null -eq $package) {
        return "<not installed>"
    }

    $path = Join-Path $package.InstallLocation "app\Codex.exe"

    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return "<missing: $path>"
    }

    return $path
}

function Write-DesktopProof {
    param(
        [bool]$BinaryChanged,
        [switch]$FastProof
    )

    if ($FastProof -and -not $BinaryChanged) {
        Write-ProofLine "desktopAppPackage" "<skipped: fast proof no-op>"
        Write-ProofLine "desktopAppExecutable" "<skipped: fast proof no-op>"
    }
    else {
        Write-ProofLine "desktopAppPackage" (Get-CodexDesktopPackageProof)
        Write-ProofLine "desktopAppExecutable" (Get-CodexDesktopExecutableProof)
    }
    Write-ProofLine "desktopAppLaunchCommand" (Get-CodexDesktopLaunchCommand)
}

function Normalize-PathForEnvironmentComparison {
    param(
        [AllowNull()]
        [string]$Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return ""
    }

    $trimmed = $Path.Trim().Trim('"')
    try {
        return [System.IO.Path]::GetFullPath($trimmed).TrimEnd(
            [System.IO.Path]::DirectorySeparatorChar,
            [System.IO.Path]::AltDirectorySeparatorChar
        )
    }
    catch {
        return $trimmed.TrimEnd("\", "/")
    }
}

function Remove-PathListEntry {
    param(
        [AllowNull()]
        [string]$PathValue,
        [string]$EntryToRemove
    )

    $entryKey = Normalize-PathForEnvironmentComparison -Path $EntryToRemove
    $parts = @()
    $removedCount = 0

    $pathList = if ($null -eq $PathValue) { "" } else { $PathValue }
    foreach ($entry in ($pathList -split ";")) {
        if ([string]::IsNullOrWhiteSpace($entry)) {
            continue
        }

        $entryPathKey = Normalize-PathForEnvironmentComparison -Path $entry
        if ([string]::Equals($entryPathKey, $entryKey, [System.StringComparison]::OrdinalIgnoreCase)) {
            $removedCount += 1
            continue
        }

        $parts += $entry
    }

    return [pscustomobject]@{
        Value = ($parts -join ";")
        RemovedCount = $removedCount
    }
}

function Set-EnvironmentVariableForTarget {
    param(
        [string]$Name,
        [AllowNull()]
        [string]$Value,
        [ValidateSet("User", "Process")]
        [string]$Target
    )

    [System.Environment]::SetEnvironmentVariable($Name, $Value, $Target)
}

function Send-EnvironmentChangedBroadcast {
    if (-not ("CodexPublish.NativeMethods" -as [type])) {
        Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

namespace CodexPublish {
    public static class NativeMethods {
        [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]
        public static extern IntPtr SendMessageTimeout(
            IntPtr hWnd,
            uint Msg,
            UIntPtr wParam,
            string lParam,
            uint fuFlags,
            uint uTimeout,
            out UIntPtr lpdwResult);
    }
}
"@
    }

    $result = [UIntPtr]::Zero
    $sendResult = [CodexPublish.NativeMethods]::SendMessageTimeout(
        [IntPtr]0xffff,
        0x001a,
        [UIntPtr]::Zero,
        "Environment",
        0x0002,
        5000,
        [ref]$result
    )
    return $sendResult -ne [IntPtr]::Zero
}

function Sync-DesktopLocalCliRouting {
    param(
        [string]$TargetPath,
        [string]$InstallDir,
        [string]$LocalCodexHome,
        [string]$LocalCodexSqliteHome,
        [switch]$DryRun,
        [ValidateSet("User", "Process")]
        [string]$EnvironmentTarget,
        [ref]$Result
    )

    Write-ProofLine "desktopLocalCliRouting" "enabled"
    Write-ProofLine "localCodexHome" $LocalCodexHome
    Write-ProofLine "localCodexHomeScope" $EnvironmentTarget
    Ensure-LocalCodexHomeDirectory -Path $LocalCodexHome -DryRun:$DryRun
    Write-ProofLine "localCodexSqliteHome" $LocalCodexSqliteHome
    Write-ProofLine "localCodexSqliteHomeScope" $EnvironmentTarget
    Ensure-LocalCodexSqliteHomeDirectory -Path $LocalCodexSqliteHome -DryRun:$DryRun
    Write-ProofLine "desktopCliPathEnvName" "CODEX_CLI_PATH"
    Write-ProofLine "desktopCliPathEnvTarget" $TargetPath
    Write-ProofLine "desktopCliPathEnvScope" $EnvironmentTarget

    $currentCliPath = [System.Environment]::GetEnvironmentVariable(
        "CODEX_CLI_PATH",
        $EnvironmentTarget
    )
    $cliPathAlreadyCurrent = [string]::Equals(
        (Normalize-PathForEnvironmentComparison -Path $currentCliPath),
        (Normalize-PathForEnvironmentComparison -Path $TargetPath),
        [System.StringComparison]::OrdinalIgnoreCase
    )
    Write-ProofLine "desktopCliPathEnvBefore" $(if ([string]::IsNullOrWhiteSpace($currentCliPath)) { "<unset>" } else { $currentCliPath })

    $currentLocalCodexHome = [System.Environment]::GetEnvironmentVariable(
        "CODEX_HOME",
        $EnvironmentTarget
    )
    $localCodexHomeAlreadyCurrent = [string]::Equals(
        (Normalize-PathForEnvironmentComparison -Path $currentLocalCodexHome),
        (Normalize-PathForEnvironmentComparison -Path $LocalCodexHome),
        [System.StringComparison]::OrdinalIgnoreCase
    )
    Write-ProofLine "localCodexHomeBefore" $(if ([string]::IsNullOrWhiteSpace($currentLocalCodexHome)) { "<unset>" } else { $currentLocalCodexHome })

    $currentLocalCodexSqliteHome = [System.Environment]::GetEnvironmentVariable(
        "CODEX_SQLITE_HOME",
        $EnvironmentTarget
    )
    $localCodexSqliteHomeAlreadyCurrent = [string]::Equals(
        (Normalize-PathForEnvironmentComparison -Path $currentLocalCodexSqliteHome),
        (Normalize-PathForEnvironmentComparison -Path $LocalCodexSqliteHome),
        [System.StringComparison]::OrdinalIgnoreCase
    )
    Write-ProofLine "localCodexSqliteHomeBefore" $(if ([string]::IsNullOrWhiteSpace($currentLocalCodexSqliteHome)) { "<unset>" } else { $currentLocalCodexSqliteHome })

    if ($cliPathAlreadyCurrent -and $localCodexHomeAlreadyCurrent -and $localCodexSqliteHomeAlreadyCurrent) {
        Write-ProofLine "desktopCliPathEnvAction" "already current"
    }
    elseif ($DryRun) {
        Write-ProofLine "desktopCliPathEnvAction" "would set"
    }
    else {
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_CLI_PATH" `
            -Value $TargetPath `
            -Target $EnvironmentTarget
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_HOME" `
            -Value $LocalCodexHome `
            -Target $EnvironmentTarget
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_SQLITE_HOME" `
            -Value $LocalCodexSqliteHome `
            -Target $EnvironmentTarget
        Write-ProofLine "desktopCliPathEnvAction" "set"
    }

    if (-not $DryRun -and $EnvironmentTarget -eq "User") {
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_CLI_PATH" `
            -Value $TargetPath `
            -Target "Process"
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_HOME" `
            -Value $LocalCodexHome `
            -Target "Process"
        Set-EnvironmentVariableForTarget `
            -Name "CODEX_SQLITE_HOME" `
            -Value $LocalCodexSqliteHome `
            -Target "Process"
    }

    $pathBefore = [System.Environment]::GetEnvironmentVariable("Path", $EnvironmentTarget)
    $pathResult = Remove-PathListEntry -PathValue $pathBefore -EntryToRemove $InstallDir
    Write-ProofLine "desktopUserPathLocalBin" $InstallDir
    Write-ProofLine "desktopUserPathLocalBinScope" $EnvironmentTarget
    if ($pathResult.RemovedCount -eq 0) {
        Write-ProofLine "desktopUserPathLocalBinAction" "already absent"
    }
    elseif ($DryRun) {
        Write-ProofLine "desktopUserPathLocalBinAction" "would remove $($pathResult.RemovedCount) entr$(if ($pathResult.RemovedCount -eq 1) { 'y' } else { 'ies' })"
    }
    else {
        Set-EnvironmentVariableForTarget `
            -Name "Path" `
            -Value $pathResult.Value `
            -Target $EnvironmentTarget
        Write-ProofLine "desktopUserPathLocalBinAction" "removed $($pathResult.RemovedCount) entr$(if ($pathResult.RemovedCount -eq 1) { 'y' } else { 'ies' })"
    }

    $changed = (-not $cliPathAlreadyCurrent) -or (-not $localCodexHomeAlreadyCurrent) -or (-not $localCodexSqliteHomeAlreadyCurrent) -or ($pathResult.RemovedCount -gt 0)
    if (-not $DryRun -and $changed -and $EnvironmentTarget -eq "User") {
        try {
            if (Send-EnvironmentChangedBroadcast) {
                Write-ProofLine "desktopEnvironmentBroadcast" "sent"
            }
            else {
                Write-ProofLine "desktopEnvironmentBroadcast" "failed"
            }
        }
        catch {
            Write-ProofLine "desktopEnvironmentBroadcast" "failed: $($_.Exception.Message)"
        }
    }
    elseif ($DryRun -and $changed -and $EnvironmentTarget -eq "User") {
        Write-ProofLine "desktopEnvironmentBroadcast" "would send"
    }
    else {
        Write-ProofLine "desktopEnvironmentBroadcast" "skipped"
    }

    $Result.Value = [pscustomobject]@{
        Changed = $changed
        RestartRequired = $changed
    }
}

function Sync-OfficialDesktopEnvironmentCleanup {
    param(
        [switch]$DryRun
    )

    Write-ProofLine "officialEnvCleanup" "CODEX_HOME unset, CODEX_CLI_PATH unset, CODEX_SQLITE_HOME unset"

    if ($DryRun) {
        Write-ProofLine "officialEnvCleanupAction" "would apply to User environment"
        return
    }

    [System.Environment]::SetEnvironmentVariable("CODEX_HOME", $null, "User")
    [System.Environment]::SetEnvironmentVariable("CODEX_CLI_PATH", $null, "User")
    [System.Environment]::SetEnvironmentVariable("CODEX_SQLITE_HOME", $null, "User")
    Write-ProofLine "officialEnvCleanupAction" "applied to User environment"
}

function Ensure-LocalCodexHomeDirectory {
    param(
        [string]$Path,
        [switch]$DryRun
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "LocalCodexHome is empty."
    }

    if (Test-Path -LiteralPath $Path -PathType Container) {
        Write-ProofLine "localCodexHomeAction" "already exists"
        return
    }

    if (Test-Path -LiteralPath $Path) {
        throw "LocalCodexHome exists but is not a directory: $Path"
    }

    if ($DryRun) {
        Write-ProofLine "localCodexHomeAction" "would create"
        return
    }

    [System.IO.Directory]::CreateDirectory($Path) | Out-Null
    Write-ProofLine "localCodexHomeAction" "created"
}

function Ensure-LocalCodexSqliteHomeDirectory {
    param(
        [string]$Path,
        [switch]$DryRun
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        throw "LocalCodexSqliteHome is empty."
    }

    if (Test-Path -LiteralPath $Path -PathType Container) {
        Write-ProofLine "localCodexSqliteHomeAction" "already exists"
        return
    }

    if (Test-Path -LiteralPath $Path) {
        throw "LocalCodexSqliteHome exists but is not a directory: $Path"
    }

    if ($DryRun) {
        Write-ProofLine "localCodexSqliteHomeAction" "would create"
        return
    }

    [System.IO.Directory]::CreateDirectory($Path) | Out-Null
    Write-ProofLine "localCodexSqliteHomeAction" "created"
}

function Restart-CodexDesktop {
    param(
        [switch]$DryRun,
        [string]$LocalCliPath,
        [string]$LocalCodexHome,
        [string]$LocalCodexSqliteHome
    )

    $desktopPath = Get-CodexDesktopExecutableProof
    if ($desktopPath.StartsWith("<")) {
        Write-ProofLine "desktopRestart" "unavailable: $desktopPath"
        return
    }

    if ($DryRun) {
        Write-ProofLine "desktopRestart" "not run"
        return
    }

    # Match on the resolved desktop executable path so codex CLI sessions
    # (same process name, different binary) are never killed.
    Get-Process Codex -ErrorAction SilentlyContinue |
        Where-Object {
            try {
                [string]::Equals($_.Path, $desktopPath, [System.StringComparison]::OrdinalIgnoreCase)
            }
            catch {
                $false
            }
        } |
        Stop-Process
    if ([string]::IsNullOrWhiteSpace($LocalCliPath)) {
        Start-Process $desktopPath
    }
    else {
        $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
        $startInfo.FileName = $desktopPath
        $startInfo.UseShellExecute = $false
        $startInfo.Environment["CODEX_CLI_PATH"] = $LocalCliPath
        $startInfo.Environment["CODEX_HOME"] = $LocalCodexHome
        $startInfo.Environment["CODEX_SQLITE_HOME"] = $LocalCodexSqliteHome
        [System.Diagnostics.Process]::Start($startInfo) | Out-Null
    }
    Write-ProofLine "desktopRestart" "restarted"
}

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

function Publish-CodexBinary {
    param(
        [string]$SourcePath,
        [string]$TargetPath,
        [string]$BackupPath
    )

    $installDir = Split-Path -Parent $TargetPath
    $tempPath = Join-Path $installDir (".codex-local-publish." + [System.Guid]::NewGuid().ToString("N") + ".tmp")

    New-Item -ItemType Directory -Path $installDir -Force | Out-Null
    [IO.File]::Copy($SourcePath, $tempPath, $true)

    try {
        if (Test-Path -LiteralPath $TargetPath -PathType Leaf) {
            $backupParent = Split-Path -Parent $BackupPath
            New-Item -ItemType Directory -Path $backupParent -Force | Out-Null
            [System.IO.File]::Replace($tempPath, $TargetPath, $BackupPath, $false)
        }
        else {
            [System.IO.File]::Move($tempPath, $TargetPath)
        }
    }
    finally {
        if (Test-Path -LiteralPath $tempPath) {
            Remove-Item -LiteralPath $tempPath -Force
        }
    }
}

function Restore-CodexBinaryPublish {
    param(
        [string]$TargetPath,
        [string]$BackupPath,
        [bool]$HadPreviousTarget,
        [string]$ProofPrefix = ""
    )

    $rollbackResultKey = if ([string]::IsNullOrWhiteSpace($ProofPrefix)) {
        "rollbackResult"
    }
    else {
        "${ProofPrefix}RollbackResult"
    }
    $targetSha256AfterRollbackKey = if ([string]::IsNullOrWhiteSpace($ProofPrefix)) {
        "targetSha256AfterRollback"
    }
    else {
        "${ProofPrefix}TargetSha256AfterRollback"
    }

    if ($HadPreviousTarget) {
        if (-not (Test-Path -LiteralPath $BackupPath -PathType Leaf)) {
            throw "Cannot roll back: backup binary is missing: $BackupPath"
        }

        [IO.File]::Copy($BackupPath, $TargetPath, $true)
        Write-ProofLine $rollbackResultKey "restored backup"
        Write-ProofLine $targetSha256AfterRollbackKey (Get-FileSha256 $TargetPath)
        return
    }

    if (Test-Path -LiteralPath $TargetPath -PathType Leaf) {
        Remove-Item -LiteralPath $TargetPath -Force
    }
    Write-ProofLine $rollbackResultKey "removed newly published target"
}

function Remove-OldCodexBackups {
    param(
        [string]$BackupDir,
        [string]$ArtifactName = "codex",
        [int]$Keep = 10,
        [AllowNull()]
        [string]$ProtectedPath
    )

    if (-not (Test-Path -LiteralPath $BackupDir -PathType Container)) {
        return
    }

    $protectedFullPath = if (
        [string]::IsNullOrWhiteSpace($ProtectedPath) -or
        -not (Test-Path -LiteralPath $ProtectedPath -PathType Leaf)
    ) {
        $null
    }
    else {
        [System.IO.Path]::GetFullPath($ProtectedPath)
    }
    $unprotectedKeep = if ($null -eq $protectedFullPath) {
        $Keep
    }
    else {
        [Math]::Max(0, $Keep - 1)
    }

    $backupNamePattern = "^$([Regex]::Escape($ArtifactName))-[0-9]{8}T[0-9]{9}Z\.exe$"
    $backups = @(Get-ChildItem -LiteralPath $BackupDir -Filter "$ArtifactName-*.exe" -File |
        Where-Object {
            $_.Name -match $backupNamePattern -and
            (
                $null -eq $protectedFullPath -or
                -not [string]::Equals(
                    [System.IO.Path]::GetFullPath($_.FullName),
                    $protectedFullPath,
                    [System.StringComparison]::OrdinalIgnoreCase
                )
            )
        } |
        Sort-Object -Property Name -Descending)
    $old = @($backups | Select-Object -Skip $unprotectedKeep)
    foreach ($backup in $old) {
        Remove-Item -LiteralPath $backup.FullName -Force
    }
    if ($old.Count -gt 0) {
        Write-ProofLine "backupPruned" $old.Count
    }
}

function Enter-CodexLocalPublishMutex {
    $mutex = [System.Threading.Mutex]::new($false, "Global\CodexLocalPublish")
    try {
        $hasMutex = $false
        try {
            $hasMutex = $mutex.WaitOne([TimeSpan]::FromSeconds(30))
        }
        catch [System.Threading.AbandonedMutexException] {
            $hasMutex = $true
        }

        if (-not $hasMutex) {
            throw "Another publish-local-codex run is already in progress."
        }

        return [pscustomobject]@{
            Mutex = $mutex
            Held = $true
        }
    }
    catch {
        $mutex.Dispose()
        throw
    }
}

function Exit-CodexLocalPublishMutex {
    param(
        [AllowNull()]
        [object]$Lock
    )

    if ($null -eq $Lock) {
        return
    }

    try {
        if ($Lock.Held) {
            $Lock.Mutex.ReleaseMutex()
        }
    }
    finally {
        $Lock.Mutex.Dispose()
    }
}

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
