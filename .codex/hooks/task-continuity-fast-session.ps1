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

    $hasRecovery = (
        -not [string]::IsNullOrWhiteSpace([string]$capsule.last_user_request) -or
        -not [string]::IsNullOrWhiteSpace([string]$capsule.last_assistant_result) -or
        -not [string]::IsNullOrWhiteSpace([string]$capsule.predecessor_thread_id)
    )
    if (-not $hasRecovery) {
        [Console]::Out.Write($emptyOutput)
        exit 0
    }

    # Recovery output is delegated to the validated implementation. It owns
    # canonical full-snapshot construction, so slow/fast entrypoints emit the
    # identical capsule representation.
    throw 'canonical recovery capsule requires validated path'
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
