# Fast repeated local publishes; keep installation, freshness checks, rollback,
# and Desktop routing in the existing publisher. Forward its command-line options
# through a child PowerShell so named switches retain their -File semantics.
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "publish-local-codex.ps1") `
    -Concise -AutoSkipBuild -Profile local-fast `
    -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli `
    -DesktopCliEnvironmentTarget User -RestartDesktopIfNeeded @args
exit $LASTEXITCODE
