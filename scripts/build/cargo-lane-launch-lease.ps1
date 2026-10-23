# Shared launch-generation protocol. The worker lock remains cleanup authority.
function Invoke-CargoMaintenanceLaunchLease {
    param([string]$LanesRoot, [string]$ReleaseToken = "")
    $guard = $null
    try {
        $guard = [IO.File]::Open((Join-Path $LanesRoot ".maintenance-launch.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $path = Join-Path $LanesRoot ".maintenance-launch.json"
        $current = $null
        if (Test-Path -LiteralPath $path) {
            try {
                $current = [IO.File]::ReadAllText($path) | ConvertFrom-Json
                if ($null -eq $current -or $current.PSObject.Properties.Name -notcontains "generation" -or $current.PSObject.Properties.Name -notcontains "expires_at") { throw "invalid startup lease" }
                if ($current.generation -isnot [string] -or [string]::IsNullOrEmpty($current.generation) -or
                    ($current.expires_at -isnot [double] -and $current.expires_at -isnot [int] -and $current.expires_at -isnot [long] -and $current.expires_at -isnot [decimal]) -or
                    [double]::IsNaN([double]$current.expires_at) -or [double]::IsInfinity([double]$current.expires_at)) { throw "invalid startup lease fields" }
            } catch {
                if (([DateTime]::UtcNow - (Get-Item -LiteralPath $path).LastWriteTimeUtc).TotalSeconds -lt 60) { return $null }
                $current = $null
            }
        }
        if ($ReleaseToken) {
            if ($null -ne $current -and $current.generation -ceq $ReleaseToken) {
                Remove-Item -LiteralPath $path -Force
                return $true
            }
            return $false
        }
        $worker = $null
        try {
            $worker = [IO.File]::Open((Join-Path $LanesRoot ".cargo-lane-trash-cleanup.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        } catch { return $null } finally { if ($null -ne $worker) { $worker.Dispose() } }
        $now = ([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() / 1000.0)
        if ($null -ne $current -and [double]$current.expires_at -gt $now) { return $null }
        $token = [Guid]::NewGuid().ToString()
        $record = @{ generation = $token; launcher_pid = $PID; expires_at = $now + 60 }
        [IO.File]::WriteAllText($path, ($record | ConvertTo-Json -Compress), [Text.UTF8Encoding]::new($false))
        return $token
    } catch { return $null } finally { if ($null -ne $guard) { $guard.Dispose() } }
}
