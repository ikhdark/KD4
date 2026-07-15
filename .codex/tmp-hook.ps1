param(
    [string]$Capture,
    [string]$Gate,
    [string]$Mode
)
$payload = [Console]::In.ReadToEnd()
[IO.File]::WriteAllText($Capture, $payload, [Text.UTF8Encoding]::new($false))
if ($Mode -eq "fast") {
    [IO.File]::WriteAllText($Gate, "ready")
} else {
    for ($i = 0; $i -lt 500 -and -not (Test-Path -LiteralPath $Gate); $i++) {
        Start-Sleep -Milliseconds 10
    }
    if (-not (Test-Path -LiteralPath $Gate)) {
        throw "fast hook did not create gate"
    }
    Start-Sleep -Milliseconds 200
}
if ($Mode -eq "slow") {
    [Console]::Out.WriteLine('{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"winner":"slow"}}}')
} else {
    [Console]::Out.WriteLine('{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"winner":"fast"}}}')
}
