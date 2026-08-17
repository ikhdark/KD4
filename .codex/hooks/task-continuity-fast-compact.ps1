param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('PreCompact', 'PostCompact')]
    [string]$ExpectedEvent
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$emptyOutput = '{}'
$stateDirectory = [System.IO.Path]::GetFullPath(
    (Join-Path $PSScriptRoot '..\harness\runs\task-continuity\v1')
)

try {
    if ([string]::IsNullOrWhiteSpace($TaskContinuityRawInput)) {
        throw 'hook stdin was empty'
    }
    [void][Reflection.Assembly]::Load(
        'System.Web.Extensions, Version=4.0.0.0, Culture=neutral, ' +
            'PublicKeyToken=31bf3856ad364e35'
    )
    $json = [Activator]::CreateInstance(
        [type]'System.Web.Script.Serialization.JavaScriptSerializer'
    )
    $inputObject = $json.DeserializeObject($TaskContinuityRawInput)
    if ($inputObject -is [Collections.IDictionary] -and
        $inputObject.ContainsKey('agent_id')) {
        [Console]::Out.Write($emptyOutput)
        exit 0
    }
    if ($inputObject -isnot [Collections.IDictionary] -or
        [string]$inputObject.hook_event_name -ne $ExpectedEvent) {
        throw 'hook input did not match the handler event'
    }
    if ($ExpectedEvent -eq 'PostCompact' -and
        $inputObject.ContainsKey('compaction_summary') -and
        -not [string]::IsNullOrWhiteSpace([string]$inputObject.compaction_summary)) {
        throw 'validated compaction summary requires the canonical state updater'
    }

    $sessionGuid = [Guid]::Empty
    if (-not [Guid]::TryParse([string]$inputObject.session_id, [ref]$sessionGuid)) {
        throw 'session_id was not a GUID'
    }
    $sessionId = $sessionGuid.ToString('D').ToLowerInvariant()
    $capsulePath = Join-Path $stateDirectory "$sessionId.json"
    if (-not [System.IO.File]::Exists($capsulePath)) {
        throw 'capsule did not exist'
    }
    $capsule = $json.DeserializeObject(
        [System.IO.File]::ReadAllText($capsulePath, [Text.Encoding]::UTF8)
    )
    $transcriptPath = if ($inputObject.ContainsKey('transcript_path')) {
        $inputObject.transcript_path
    }
    else {
        $null
    }
    if ($capsule -isnot [Collections.IDictionary] -or
        [string]$capsule.schema_version -ne '1' -or
        [string]$capsule.session_id -ne $sessionId -or
        [string]$capsule.working_directory -ne [string]$inputObject.cwd -or
        [string]$capsule.transcript_path -ne [string]$transcriptPath -or
        [string]$capsule.last_event -ne $ExpectedEvent) {
        throw 'capsule identity did not match'
    }
    $expectedPhase = if ($ExpectedEvent -eq 'PreCompact') { 'pre' } else { 'post' }
    if ([string]$capsule.compaction.phase -ne $expectedPhase -or
        [string]$capsule.compaction.trigger -ne [string]$inputObject.trigger) {
        throw 'compaction state changed'
    }

    $storedRoot = [string]$capsule.repository.root
    $repositoryMatches = $false
    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        if ([string]::IsNullOrWhiteSpace($storedRoot)) {
            $probeLines = @(
                & git.exe -C ([string]$inputObject.cwd) rev-parse --show-toplevel 2>$null
            )
            $repositoryMatches = (
                $LASTEXITCODE -ne 0 -and
                $null -eq $capsule.repository.root -and
                $null -eq $capsule.repository.revision -and
                $null -eq $capsule.repository.dirty_summary
            )
        }
        else {
            $statusLines = @(
                & git.exe -C $storedRoot status --porcelain=v2 --branch 2>$null
            )
            if ($LASTEXITCODE -eq 0) {
                $revision = $null
                $dirtyLines = @()
                foreach ($statusLine in $statusLines) {
                    if ($statusLine.StartsWith('# branch.oid ')) {
                        $revision = $statusLine.Substring(13).Trim()
                    }
                    elseif (-not $statusLine.StartsWith('# ')) {
                        $dirtyLines += $statusLine
                    }
                }
                $dirty = 'clean'
                if ($dirtyLines.Count -gt 0) {
                    $limit = [Math]::Min(50, $dirtyLines.Count)
                    $boundedDirty = @()
                    for ($index = 0; $index -lt $limit; $index++) {
                        $boundedDirty += [string]$dirtyLines[$index]
                    }
                    $dirty = $boundedDirty -join "`n"
                    if ($dirtyLines.Count -gt 50) {
                        $dirty += "`n..."
                    }
                    $dirty = [regex]::Replace(
                        $dirty,
                        '(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+',
                        'Bearer [REDACTED]'
                    )
                    $dirty = [regex]::Replace(
                        $dirty,
                        '\bsk-(?:proj-)?[A-Za-z0-9_-]{8,}\b',
                        '[REDACTED]'
                    )
                    $dirty = [regex]::Replace(
                        $dirty,
                        '(?i)(\b(?:api[_-]?key|token|password|secret)\b\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)',
                        '$1[REDACTED]'
                    )
                    if ($dirty.Length -gt 2000) {
                        $dirty = $dirty.Substring(0, 1997) + '...'
                    }
                }
                $repositoryMatches = (
                    -not [string]::IsNullOrWhiteSpace($revision) -and
                    [string]$capsule.repository.revision -eq $revision -and
                    [string]$capsule.repository.dirty_summary -eq $dirty
                )
            }
        }
    }
    finally {
        $ErrorActionPreference = $savedErrorActionPreference
    }
    if ($repositoryMatches) {
        [Console]::Out.Write($emptyOutput)
        exit 0
    }
}
catch {
    # The validated helper owns all ambiguous and changing paths.
}

try {
    & (Join-Path $PSScriptRoot 'task-continuity.ps1') `
        -TaskContinuityRawInput $TaskContinuityRawInput
}
catch {
    try {
        [Console]::Error.WriteLine("task-continuity: $($_.Exception.Message)")
    }
    catch {
        # Diagnostics must never interfere with fail-open output.
    }
    [Console]::Out.Write($emptyOutput)
}
exit 0
