param(
    [Parameter(Mandatory = $true)]
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput,
    [Parameter(Mandatory = $true)]
    [ValidateSet('UserPromptSubmit', 'Stop')]
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
    $turnMatches = if (-not $inputObject.ContainsKey('turn_id')) {
        $true
    }
    elseif ($null -eq $inputObject.turn_id) {
        $null -eq $capsule.last_turn_id
    }
    else {
        $inputObject.turn_id -is [string] -and
            [string]$inputObject.turn_id -eq [string]$capsule.last_turn_id
    }
    $commonMatches = (
        $capsule -is [Collections.IDictionary] -and
        [string]$capsule.schema_version -eq '1' -and
        [string]$capsule.session_id -eq $sessionId -and
        [string]$capsule.working_directory -eq [string]$inputObject.cwd -and
        [string]$capsule.transcript_path -eq [string]$transcriptPath
    )
    if (-not $commonMatches) {
        throw 'capsule identity did not match'
    }

    if ($ExpectedEvent -eq 'UserPromptSubmit' -and
        [string]$capsule.last_event -eq $ExpectedEvent -and
        $inputObject.prompt -is [string] -and
        [string]$inputObject.prompt -eq [string]$capsule.last_user_request -and
        $turnMatches) {
        [Console]::Out.Write($emptyOutput)
        exit 0
    }
    if ($ExpectedEvent -eq 'Stop' -and
        [string]$capsule.last_event -eq $ExpectedEvent -and
        $turnMatches) {
        $assistantResult = if ($inputObject.ContainsKey('last_assistant_message')) {
            $inputObject.last_assistant_message
        }
        else {
            $null
        }
        if (($null -eq $assistantResult -and
                $null -eq $capsule.last_assistant_result) -or
            ($assistantResult -is [string] -and
                [string]$assistantResult -eq [string]$capsule.last_assistant_result)) {
            [Console]::Out.Write($emptyOutput)
            exit 0
        }
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
