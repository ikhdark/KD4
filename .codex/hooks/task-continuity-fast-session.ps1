param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('SessionStart')]
    [string]$ExpectedEvent
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$emptyOutput = '{}'
$script:RepositoryStateObservation = $null
$stateDirectory = [System.IO.Path]::GetFullPath(
    (Join-Path $PSScriptRoot '..\harness\runs\task-continuity\v1')
)

function Get-BoundedRedactedString {
    param(
        [AllowNull()][object]$Value,
        [int]$MaximumCharacters
    )

    if ($null -eq $Value) {
        return $null
    }
    if ($Value -isnot [string] -or $Value.IndexOf([char]0) -ge 0) {
        throw 'recovery value was not a safe string'
    }
    $text = $Value.Replace("`r`n", "`n").Replace("`r", "`n")
    $text = [regex]::Replace(
        $text,
        '(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+',
        'Bearer [REDACTED]'
    )
    $text = [regex]::Replace(
        $text,
        '\bsk-(?:proj-)?[A-Za-z0-9_-]{8,}\b',
        '[REDACTED]'
    )
    $text = [regex]::Replace(
        $text,
        '(?i)(\b(?:api[_-]?key|token|password|secret)\b\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)',
        '$1[REDACTED]'
    )
    if ($text.Length -gt $MaximumCharacters) {
        return $text.Substring(0, $MaximumCharacters - 3) + '...'
    }
    return $text
}

function ConvertTo-NullableString {
    param([AllowNull()][object]$Value)

    if ($null -eq $Value -or [string]::IsNullOrWhiteSpace([string]$Value)) {
        return $null
    }
    if ($Value -isnot [string]) {
        throw 'recovery value was not a string'
    }
    return $Value
}

function Test-RepositoryStateMatchesCapsule {
    param(
        [Collections.IDictionary]$Capsule,
        [string]$WorkingDirectory
    )

    $storedRoot = [string]$Capsule.repository.root
    $matches = $false
    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'SilentlyContinue'
    try {
        if ([string]::IsNullOrWhiteSpace($storedRoot)) {
            [void]@(& git.exe -C $WorkingDirectory rev-parse --show-toplevel 2>$null)
            $matches = (
                $LASTEXITCODE -ne 0 -and
                $null -eq $Capsule.repository.root -and
                $null -eq $Capsule.repository.revision -and
                $null -eq $Capsule.repository.dirty_summary
            )
        }
        else {
            $rootLines = @(
                & git.exe -C $WorkingDirectory rev-parse --show-toplevel 2>$null
            )
            $rootExitCode = $LASTEXITCODE
            $currentRoot = ($rootLines -join "`n").Trim()
            $statusExitCode = 1
            if ($rootExitCode -eq 0 -and
                [string]::Equals(
                    $currentRoot,
                    $storedRoot,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                $statusLines = @(
                    & git.exe -C $WorkingDirectory status --porcelain=v2 --branch 2>$null
                )
                $statusExitCode = $LASTEXITCODE
            }
            if ($rootExitCode -eq 0 -and $statusExitCode -eq 0) {
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
                    $dirty = @($dirtyLines | Select-Object -First 50) -join "`n"
                    if ($dirtyLines.Count -gt 50) {
                        $dirty += "`n..."
                    }
                    $dirty = Get-BoundedRedactedString $dirty 2000
                }
                $script:RepositoryStateObservation = [pscustomobject][ordered]@{
                    working_directory = $WorkingDirectory
                    root = $currentRoot
                    revision = $revision
                    dirty_summary = $dirty
                }
                $matches = (
                    -not [string]::IsNullOrWhiteSpace($revision) -and
                    [string]$Capsule.repository.revision -eq $revision -and
                    [string]$Capsule.repository.dirty_summary -eq $dirty
                )
            }
        }
    }
    finally {
        $ErrorActionPreference = $savedErrorActionPreference
    }
    return $matches
}

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
        [string]$inputObject.hook_event_name -ne $ExpectedEvent -or
        [string]$inputObject.source -ne 'resume') {
        throw 'hook input was not a repeated resume event'
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
        [string]$capsule.last_event -ne 'SessionStart') {
        throw 'capsule identity did not match'
    }
    if (-not (Test-RepositoryStateMatchesCapsule `
        -Capsule $capsule `
        -WorkingDirectory ([string]$inputObject.cwd))) {
        throw 'repository state changed'
    }

    [void][System.IO.Directory]::CreateDirectory($stateDirectory)
    $cutoff = [DateTime]::UtcNow.AddDays(-30)
    $inactive = @()
    foreach ($path in [System.IO.Directory]::GetFiles(
        $stateDirectory,
        '*.json',
        [System.IO.SearchOption]::TopDirectoryOnly
    )) {
        if (-not [string]::Equals(
            $path,
            $capsulePath,
            [StringComparison]::OrdinalIgnoreCase
        )) {
            $file = [System.IO.FileInfo]$path
            if ($file.LastWriteTimeUtc -lt $cutoff) {
                [System.IO.File]::Delete($file.FullName)
            }
            else {
                $inactive += $file
            }
        }
    }
    if ($inactive.Count -gt 100) {
        $inactive = @($inactive | Sort-Object LastWriteTimeUtc -Descending)
        for ($index = 100; $index -lt $inactive.Count; $index++) {
            [System.IO.File]::Delete($inactive[$index].FullName)
        }
    }

    $taskState = $capsule.task_state
    $hasTaskState = $taskState -is [Collections.IDictionary] -and @(@(
        'goal', 'current_state', 'completed_work', 'unresolved_work',
        'evidence', 'next_action'
    ) | Where-Object {
            -not [string]::IsNullOrWhiteSpace([string]$taskState[$_])
        }
    ).Count -gt 0
    $hasRecovery = (
        $hasTaskState -or
        -not [string]::IsNullOrWhiteSpace([string]$capsule.last_user_request) -or
        -not [string]::IsNullOrWhiteSpace([string]$capsule.last_assistant_result) -or
        -not [string]::IsNullOrWhiteSpace([string]$capsule.predecessor_thread_id)
    )
    if (-not $hasRecovery) {
        [Console]::Out.Write($emptyOutput)
        exit 0
    }

    $semantic = [ordered]@{
        schema_version = [int]$capsule.schema_version
        session_id = [string]$capsule.session_id
        continuity_epoch = [int]$capsule.continuity_epoch
        predecessor_thread_id = ConvertTo-NullableString $capsule.predecessor_thread_id
        working_directory = [string]$capsule.working_directory
        task_label = Get-BoundedRedactedString $capsule.task_label 512
        last_user_request = Get-BoundedRedactedString $capsule.last_user_request 900
        last_assistant_result = Get-BoundedRedactedString $capsule.last_assistant_result 900
        task_state = [ordered]@{
            goal = Get-BoundedRedactedString $taskState.goal 600
            current_state = Get-BoundedRedactedString $taskState.current_state 600
            completed_work = Get-BoundedRedactedString $taskState.completed_work 600
            unresolved_work = Get-BoundedRedactedString $taskState.unresolved_work 600
            evidence = Get-BoundedRedactedString $taskState.evidence 600
            next_action = Get-BoundedRedactedString $taskState.next_action 600
        }
        repository = [ordered]@{
            root = ConvertTo-NullableString $capsule.repository.root
            revision = ConvertTo-NullableString $capsule.repository.revision
            dirty_summary = Get-BoundedRedactedString $capsule.repository.dirty_summary 600
        }
        compaction = [ordered]@{
            phase = [string]$capsule.compaction.phase
            trigger = ConvertTo-NullableString $capsule.compaction.trigger
        }
    }
    $context = '<kd4_continuity_capsule_v1>' + $json.Serialize($semantic) +
        '</kd4_continuity_capsule_v1>'
    if ($context.Length -gt 8000) {
        throw 'canonical continuity capsule exceeded its hard context bound'
    }
    $wire = [ordered]@{
        hookSpecificOutput = [ordered]@{
            hookEventName = 'SessionStart'
            additionalContext = $context
        }
    }
    [Console]::Out.Write($json.Serialize($wire))
    exit 0
}
catch {
    # The validated helper owns all ambiguous and changing paths.
}

try {
    $slowArguments = @{
        TaskContinuityRawInput = $TaskContinuityRawInput
        TaskContinuitySkipFastPath = $true
    }
    if ($null -ne $script:RepositoryStateObservation) {
        $slowArguments.TaskContinuityRepositoryState = $script:RepositoryStateObservation
    }
    & (Join-Path $PSScriptRoot 'task-continuity.ps1') @slowArguments
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
