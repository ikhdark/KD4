[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($env:APP_SERVER_URL)) {
    throw 'missing required env var: APP_SERVER_URL'
}
if ([string]::IsNullOrWhiteSpace($env:APP_SERVER_TEST_CLIENT_BIN)) {
    throw 'missing required env var: APP_SERVER_TEST_CLIENT_BIN'
}

$threadId = if (-not [string]::IsNullOrWhiteSpace($env:CODEX_THREAD_ID)) {
    $env:CODEX_THREAD_ID
}
else {
    $env:THREAD_ID
}
if ([string]::IsNullOrWhiteSpace($threadId)) {
    throw 'missing required env var: CODEX_THREAD_ID'
}

$holdSeconds = if ([string]::IsNullOrWhiteSpace($env:ELICITATION_HOLD_SECONDS)) {
    '15'
}
else {
    $env:ELICITATION_HOLD_SECONDS
}

Write-Output "[elicitation-hold] increment thread=$threadId"
& $env:APP_SERVER_TEST_CLIENT_BIN --url $env:APP_SERVER_URL `
    thread-hold-elicitation $threadId --hold-seconds $holdSeconds
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Write-Output '[elicitation-hold] done'
