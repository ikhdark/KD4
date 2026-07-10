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
        $trackedFiles = & $git.Source -C $RepoRoot ls-files -- codex-rs "scripts/publish-local-codex*.ps1" scripts/common-rust-env.ps1 justfile 2>$null
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

function Get-SourceNewestWriteUtcForProof {
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

function Get-LocalPublishBuildInputFingerprint {
    param(
        [string]$RepoRoot
    )

    $git = Get-Command git -ErrorAction SilentlyContinue
    if (-not $git) {
        return $null
    }

    $oldErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $inputPaths = @(& $git.Source -c core.quotepath=false -C $RepoRoot ls-files --cached --others --exclude-standard -- codex-rs "scripts/publish-local-codex*.ps1" scripts/common-rust-env.ps1 justfile 2>$null)
        $inputPathsExitCode = $LASTEXITCODE
        $headCommit = @(& $git.Source -C $RepoRoot rev-parse --verify HEAD 2>$null)
        $headCommitExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $oldErrorActionPreference
    }
    if (
        $inputPathsExitCode -ne 0 -or
        $headCommitExitCode -ne 0 -or
        $headCommit.Count -ne 1 -or
        [string]::IsNullOrWhiteSpace([string]$headCommit[0])
    ) {
        return $null
    }

    $relevantPaths = [System.Collections.Generic.List[string]]::new()
    foreach ($inputPath in $inputPaths) {
        $normalized = ([string]$inputPath).Trim().Trim('"') -replace "\\", "/"
        if (Test-LocalPublishBuildRelevantPath -Path $normalized) {
            $relevantPaths.Add($normalized)
        }
    }

    [string[]]$sortedPaths = @($relevantPaths.ToArray())
    [Array]::Sort($sortedPaths, [StringComparer]::Ordinal)

    $sha256 = [Security.Cryptography.SHA256]::Create()
    $utf8 = [Text.UTF8Encoding]::new($false)
    $buffer = [byte[]]::new(1MB)
    $empty = [byte[]]::new(0)
    $newlineBytes = $utf8.GetBytes("`n")
    try {
        $prefixBytes = $utf8.GetBytes("codex-local-publish-inputs-v2`nhead=$(([string]$headCommit[0]).Trim())`n")
        [void]$sha256.TransformBlock($prefixBytes, 0, $prefixBytes.Length, $prefixBytes, 0)
        $previousPath = $null
        foreach ($normalized in $sortedPaths) {
            if ($normalized -ceq $previousPath) {
                continue
            }
            $previousPath = $normalized
            $pathBytes = $utf8.GetBytes($normalized)
            $path = Join-Path $RepoRoot ($normalized -replace "/", [IO.Path]::DirectorySeparatorChar)
            if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
                $missingBytes = $utf8.GetBytes("missing:$($pathBytes.Length):$normalized`n")
                [void]$sha256.TransformBlock($missingBytes, 0, $missingBytes.Length, $missingBytes, 0)
                continue
            }

            $stream = $null
            try {
                $stream = [IO.File]::Open(
                    $path,
                    [IO.FileMode]::Open,
                    [IO.FileAccess]::Read,
                    [IO.FileShare]::Read
                )
                $headerBytes = $utf8.GetBytes("file:$($pathBytes.Length):${normalized}:$($stream.Length):")
                [void]$sha256.TransformBlock($headerBytes, 0, $headerBytes.Length, $headerBytes, 0)
                $bytesReadTotal = 0L
                while (($bytesRead = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    [void]$sha256.TransformBlock($buffer, 0, $bytesRead, $buffer, 0)
                    $bytesReadTotal += $bytesRead
                }
                if ($bytesReadTotal -ne $stream.Length) {
                    return $null
                }
                [void]$sha256.TransformBlock($newlineBytes, 0, $newlineBytes.Length, $newlineBytes, 0)
            }
            finally {
                if ($null -ne $stream) {
                    $stream.Dispose()
                }
            }
        }

        [void]$sha256.TransformFinalBlock($empty, 0, 0)
        return [BitConverter]::ToString($sha256.Hash).Replace("-", "").ToLowerInvariant()
    }
    catch {
        return $null
    }
    finally {
        $sha256.Dispose()
    }
}

function Test-Sha256Text {
    param(
        [AllowNull()]
        [object]$Value
    )

    return $null -ne $Value -and ([string]$Value) -cmatch "\A[0-9a-f]{64}\z"
}

function Read-BuildStamp {
    param(
        [string]$StampPath
    )

    if (-not (Test-Path -LiteralPath $StampPath -PathType Leaf)) {
        return $null
    }

    try {
        $stamp = [IO.File]::ReadAllText($StampPath) | ConvertFrom-Json
        $schemaVersion = $stamp.PSObject.Properties["schemaVersion"]
        $profile = $stamp.PSObject.Properties["profile"]
        $sourceFingerprint = $stamp.PSObject.Properties["sourceFingerprint"]
        $sourceNewestWriteUtc = $stamp.PSObject.Properties["sourceNewestWriteUtc"]
        $codexSha256 = $stamp.PSObject.Properties["codexSha256"]
        $codeModeHostSha256 = $stamp.PSObject.Properties["codeModeHostSha256"]
        $writtenAtUtc = $stamp.PSObject.Properties["writtenAtUtc"]
        if (
            $null -eq $schemaVersion -or [string]$schemaVersion.Value -cne "2" -or
            $null -eq $profile -or [string]::IsNullOrWhiteSpace([string]$profile.Value) -or
            $null -eq $sourceFingerprint -or -not (Test-Sha256Text -Value $sourceFingerprint.Value) -or
            $null -eq $codexSha256 -or -not (Test-Sha256Text -Value $codexSha256.Value) -or
            $null -eq $codeModeHostSha256 -or -not (Test-Sha256Text -Value $codeModeHostSha256.Value) -or
            $null -eq $writtenAtUtc -or [string]::IsNullOrWhiteSpace([string]$writtenAtUtc.Value)
        ) {
            return $null
        }

        [void][DateTime]::ParseExact(
            [string]$writtenAtUtc.Value,
            "o",
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind
        )
        if ($null -ne $sourceNewestWriteUtc -and $null -ne $sourceNewestWriteUtc.Value) {
            [void][DateTime]::ParseExact(
                [string]$sourceNewestWriteUtc.Value,
                "o",
                [Globalization.CultureInfo]::InvariantCulture,
                [Globalization.DateTimeStyles]::RoundtripKind
            )
        }

        return [pscustomobject]@{
            Profile = [string]$profile.Value
            SourceFingerprint = ([string]$sourceFingerprint.Value).ToLowerInvariant()
            SourceNewestWriteUtc = if ($null -eq $sourceNewestWriteUtc) { $null } else { $sourceNewestWriteUtc.Value }
            CodexSha256 = ([string]$codexSha256.Value).ToLowerInvariant()
            CodeModeHostSha256 = ([string]$codeModeHostSha256.Value).ToLowerInvariant()
        }
    }
    catch {
        return $null
    }
}

function Get-AutoSkipBuildDecision {
    param(
        [string]$RepoRoot,
        [string]$StampPath,
        [string]$Profile,
        [string]$SourceExe,
        [string]$SourceCodeModeHostExe
    )

    if (
        -not (Test-Path -LiteralPath $SourceExe -PathType Leaf) -or
        -not (Test-Path -LiteralPath $SourceCodeModeHostExe -PathType Leaf)
    ) {
        return [pscustomobject]@{ CanSkip = $false; Reason = "source artifact missing" }
    }

    $stamp = Read-BuildStamp -StampPath $StampPath
    if ($null -eq $stamp) {
        $reason = if (Test-Path -LiteralPath $StampPath -PathType Leaf) {
            "build stamp legacy or invalid"
        }
        else {
            "build stamp missing"
        }
        return [pscustomobject]@{ CanSkip = $false; Reason = $reason }
    }
    if ($stamp.Profile -cne $Profile) {
        return [pscustomobject]@{ CanSkip = $false; Reason = "build stamp profile mismatch" }
    }

    $sourceFingerprint = Get-LocalPublishBuildInputFingerprint -RepoRoot $RepoRoot
    if (-not (Test-Sha256Text -Value $sourceFingerprint)) {
        return [pscustomobject]@{ CanSkip = $false; Reason = "publish input fingerprint unavailable" }
    }
    if ($stamp.SourceFingerprint -cne $sourceFingerprint) {
        return [pscustomobject]@{ CanSkip = $false; Reason = "tracked publish inputs changed" }
    }

    $codexSha256 = Get-FileSha256 -Path $SourceExe
    $codeModeHostSha256 = Get-FileSha256 -Path $SourceCodeModeHostExe
    if (
        $stamp.CodexSha256 -cne $codexSha256 -or
        $stamp.CodeModeHostSha256 -cne $codeModeHostSha256
    ) {
        return [pscustomobject]@{ CanSkip = $false; Reason = "source artifact differs from stamped build" }
    }

    return [pscustomobject]@{
        CanSkip = $true
        Reason = "source artifacts and tracked publish inputs match build stamp"
    }
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

    $stamp = Read-BuildStamp -StampPath $StampPath
    if ($null -eq $stamp -or $null -eq $stamp.SourceNewestWriteUtc) {
        return $null
    }

    try {
        return [DateTime]::ParseExact(
            [string]$stamp.SourceNewestWriteUtc,
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
        [string]$Profile,
        [string]$SourceFingerprint,
        [string]$SourceExe,
        [string]$SourceCodeModeHostExe,
        [AllowNull()]
        [object]$SourceNewestUtc
    )

    if (-not (Test-Sha256Text -Value $SourceFingerprint)) {
        throw "Cannot write local publish build stamp without a valid source fingerprint."
    }
    $codexSha256 = Get-FileSha256 -Path $SourceExe
    $codeModeHostSha256 = Get-FileSha256 -Path $SourceCodeModeHostExe
    if (-not (Test-Sha256Text -Value $codexSha256) -or -not (Test-Sha256Text -Value $codeModeHostSha256)) {
        throw "Cannot write local publish build stamp because a built source artifact is missing."
    }

    $parent = Split-Path -Parent $StampPath
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $stamp = [ordered]@{
        schemaVersion = 2
        profile = $Profile
        sourceFingerprint = $SourceFingerprint
        sourceNewestWriteUtc = if ($null -eq $SourceNewestUtc) { $null } else { ([DateTime]$SourceNewestUtc).ToString("o") }
        codexSha256 = $codexSha256
        codeModeHostSha256 = $codeModeHostSha256
        writtenAtUtc = [DateTime]::UtcNow.ToString("o")
    }
    $temporaryPath = "$StampPath.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        $utf8WithoutBom = [Text.UTF8Encoding]::new($false)
        [IO.File]::WriteAllText($temporaryPath, ($stamp | ConvertTo-Json -Compress), $utf8WithoutBom)
        if (Test-Path -LiteralPath $StampPath -PathType Leaf) {
            [IO.File]::Replace($temporaryPath, $StampPath, $null)
        }
        else {
            [IO.File]::Move($temporaryPath, $StampPath)
        }
    }
    finally {
        if (Test-Path -LiteralPath $temporaryPath -PathType Leaf) {
            Remove-Item -LiteralPath $temporaryPath -Force
        }
    }
}

function Remove-BuildStamp {
    param(
        [string]$StampPath
    )

    if (Test-Path -LiteralPath $StampPath -PathType Leaf) {
        Remove-Item -LiteralPath $StampPath -Force
    }
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
