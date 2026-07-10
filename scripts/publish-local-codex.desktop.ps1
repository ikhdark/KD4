# Desktop routing, process, and environment helpers.

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
