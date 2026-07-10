# Proof, freshness, and local-build inspection helpers.

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
