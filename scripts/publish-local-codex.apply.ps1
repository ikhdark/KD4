# Atomic publish, rollback, backup, and mutex helpers.

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
