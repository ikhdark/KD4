param(
    [AllowEmptyString()]
    [string]$TaskContinuityRawInput
)

Set-StrictMode -Version 2.0

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$InformationPreference = 'SilentlyContinue'
$VerbosePreference = 'SilentlyContinue'
$WarningPreference = 'SilentlyContinue'

$script:SchemaVersion = 1
$script:MaxExcerptChars = 4000
$script:MaxContextChars = 8000
$script:StateDirectory = [System.IO.Path]::GetFullPath(
    (Join-Path $PSScriptRoot '..\harness\runs\task-continuity\v1')
)
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$script:EmptyOutput = '{}'

# PowerShell executes every function declaration it reaches. Keep proven no-change
# cases ahead of the full implementation so synchronous hooks pay only for the
# work their event requires. Any ambiguity falls through to the validated path.
$script:RawInput = if ($PSBoundParameters.ContainsKey('TaskContinuityRawInput')) {
    $TaskContinuityRawInput
}
else {
    [Console]::In.ReadToEnd()
}
$script:FastInput = $null
$script:FastJsonSerializer = $null
try {
    if ([string]::IsNullOrWhiteSpace($script:RawInput)) {
        throw 'hook stdin was empty'
    }
    [void][Reflection.Assembly]::Load(
        'System.Web.Extensions, Version=4.0.0.0, Culture=neutral, ' +
            'PublicKeyToken=31bf3856ad364e35'
    )
    $script:FastJsonSerializer = [Activator]::CreateInstance(
        [type]'System.Web.Script.Serialization.JavaScriptSerializer'
    )
    $script:FastInput = $script:FastJsonSerializer.DeserializeObject($script:RawInput)
}
catch {
    # The validated path owns malformed or unsupported input diagnostics.
}

if ($script:FastInput -is [Collections.IDictionary] -and
    $script:FastInput.ContainsKey('agent_id')) {
    [Console]::Out.Write($script:EmptyOutput)
    exit 0
}

try {
    if ($script:FastInput -is [Collections.IDictionary]) {
        $fastEvent = [string]$script:FastInput.hook_event_name
        $fastSessionGuid = [Guid]::Empty
        $fastSessionValue = [string]$script:FastInput.session_id
        if ([Guid]::TryParse($fastSessionValue, [ref]$fastSessionGuid)) {
            $fastSessionId = $fastSessionGuid.ToString('D').ToLowerInvariant()
            $fastCapsulePath = Join-Path $script:StateDirectory "$fastSessionId.json"
            if ([System.IO.File]::Exists($fastCapsulePath)) {
                $fastCapsule = $script:FastJsonSerializer.DeserializeObject(
                    [System.IO.File]::ReadAllText(
                        $fastCapsulePath,
                        [Text.Encoding]::UTF8
                    )
                )
                $fastInputTranscript = if (
                    -not $script:FastInput.ContainsKey('transcript_path')
                ) {
                    $null
                }
                else {
                    $script:FastInput.transcript_path
                }
                $fastCommonMatches = (
                    [string]$fastCapsule.schema_version -eq [string]$script:SchemaVersion -and
                    [string]$fastCapsule.session_id -eq $fastSessionId -and
                    [string]$fastCapsule.working_directory -eq [string]$script:FastInput.cwd -and
                    [string]$fastCapsule.transcript_path -eq [string]$fastInputTranscript
                )

                if ($fastCommonMatches -and $fastEvent -eq 'UserPromptSubmit' -and
                    [string]$fastCapsule.last_event -eq 'UserPromptSubmit' -and
                    $script:FastInput.prompt -is [string] -and
                    [string]$script:FastInput.prompt -eq [string]$fastCapsule.last_user_request) {
                    [Console]::Out.Write($script:EmptyOutput)
                    exit 0
                }

                if ($fastCommonMatches -and $fastEvent -eq 'Stop' -and
                    [string]$fastCapsule.last_event -eq 'Stop') {
                    $fastAssistant = if (
                        -not $script:FastInput.ContainsKey('last_assistant_message')
                    ) {
                        $null
                    }
                    else {
                        $script:FastInput.last_assistant_message
                    }
                    $fastAssistantMatches = (
                        ($null -eq $fastAssistant -and
                            $null -eq $fastCapsule.last_assistant_result) -or
                        ($fastAssistant -is [string] -and
                            [string]$fastAssistant -eq [string]$fastCapsule.last_assistant_result)
                    )
                    if ($fastAssistantMatches) {
                        [Console]::Out.Write($script:EmptyOutput)
                        exit 0
                    }
                }

                $fastNeedsRepository = $fastCommonMatches -and (
                    $fastEvent -in @('PreCompact', 'PostCompact') -or
                    ($fastEvent -eq 'SessionStart' -and
                        [string]$script:FastInput.source -eq 'resume' -and
                        [string]$fastCapsule.last_event -eq 'SessionStart')
                )
                if ($fastNeedsRepository) {
                    $savedErrorActionPreference = $ErrorActionPreference
                    $ErrorActionPreference = 'SilentlyContinue'
                    try {
                        $fastRootLines = @(
                            & git.exe -C ([string]$script:FastInput.cwd) `
                                rev-parse --show-toplevel 2>$null
                        )
                        $fastRootExit = $LASTEXITCODE
                        $fastRevisionLines = @()
                        $fastRevisionExit = 1
                        $fastDirtyLines = @()
                        $fastDirtyExit = 1
                        if ($fastRootExit -eq 0 -and $fastRootLines.Count -gt 0) {
                            $fastRevisionLines = @(
                                & git.exe -C ([string]$script:FastInput.cwd) `
                                    rev-parse HEAD 2>$null
                            )
                            $fastRevisionExit = $LASTEXITCODE
                            $fastDirtyLines = @(
                                & git.exe -C ([string]$script:FastInput.cwd) `
                                    status --short 2>$null
                            )
                            $fastDirtyExit = $LASTEXITCODE
                        }
                    }
                    finally {
                        $ErrorActionPreference = $savedErrorActionPreference
                    }

                    $fastRoot = $null
                    $fastRevision = $null
                    $fastDirty = $null
                    if ($fastRootExit -eq 0 -and $fastRevisionExit -eq 0 -and
                        $fastDirtyExit -eq 0) {
                        $fastRoot = ($fastRootLines -join "`n").Trim()
                        $fastRevision = ($fastRevisionLines -join "`n").Trim()
                        $fastBoundedDirty = @($fastDirtyLines | Select-Object -First 50)
                        if ($fastBoundedDirty.Count -eq 0) {
                            $fastDirty = 'clean'
                        }
                        else {
                            $fastDirty = $fastBoundedDirty -join "`n"
                            if ($fastDirtyLines.Count -gt 50) {
                                $fastDirty += "`n..."
                            }
                            $fastDirty = [regex]::Replace(
                                $fastDirty,
                                '(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+',
                                'Bearer [REDACTED]'
                            )
                            $fastDirty = [regex]::Replace(
                                $fastDirty,
                                '\bsk-(?:proj-)?[A-Za-z0-9_-]{8,}\b',
                                '[REDACTED]'
                            )
                            $fastDirty = [regex]::Replace(
                                $fastDirty,
                                '(?i)(\b(?:api[_-]?key|token|password|secret)\b\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)',
                                '$1[REDACTED]'
                            )
                            if ($fastDirty.Length -gt 2000) {
                                $fastDirty = $fastDirty.Substring(0, 1997) + '...'
                            }
                        }
                    }
                    $fastRepositoryMatches = (
                        [string]$fastCapsule.repository.root -eq [string]$fastRoot -and
                        [string]$fastCapsule.repository.revision -eq [string]$fastRevision -and
                        [string]$fastCapsule.repository.dirty_summary -eq [string]$fastDirty
                    )

                    if ($fastRepositoryMatches -and
                        $fastEvent -in @('PreCompact', 'PostCompact')) {
                        $fastPhase = if ($fastEvent -eq 'PreCompact') { 'pre' } else { 'post' }
                        if ([string]$fastCapsule.last_event -eq $fastEvent -and
                            [string]$fastCapsule.compaction.phase -eq $fastPhase -and
                            [string]$fastCapsule.compaction.trigger -eq
                                [string]$script:FastInput.trigger) {
                            [Console]::Out.Write($script:EmptyOutput)
                            exit 0
                        }
                    }

                    if ($fastRepositoryMatches -and $fastEvent -eq 'SessionStart') {
                        [void][System.IO.Directory]::CreateDirectory($script:StateDirectory)
                        $fastCutoff = [DateTime]::UtcNow.AddDays(-30)
                        $fastInactive = @(
                            Get-ChildItem -LiteralPath $script:StateDirectory `
                                -Filter '*.json' -File |
                                Where-Object { $_.Name -ne "$fastSessionId.json" }
                        )
                        foreach ($fastFile in @(
                            $fastInactive |
                                Where-Object { $_.LastWriteTimeUtc -lt $fastCutoff }
                        )) {
                            Remove-Item -LiteralPath $fastFile.FullName -Force -ErrorAction Stop
                        }
                        $fastRemaining = @(
                            Get-ChildItem -LiteralPath $script:StateDirectory `
                                -Filter '*.json' -File |
                                Where-Object { $_.Name -ne "$fastSessionId.json" } |
                                Sort-Object LastWriteTimeUtc -Descending
                        )
                        foreach ($fastFile in @($fastRemaining | Select-Object -Skip 100)) {
                            Remove-Item -LiteralPath $fastFile.FullName -Force -ErrorAction Stop
                        }

                        $fastHasRecovery = (
                            -not [string]::IsNullOrWhiteSpace(
                                [string]$fastCapsule.last_user_request
                            ) -or
                            -not [string]::IsNullOrWhiteSpace(
                                [string]$fastCapsule.last_assistant_result
                            ) -or
                            -not [string]::IsNullOrWhiteSpace(
                                [string]$fastCapsule.predecessor_thread_id
                            )
                        )
                        if (-not $fastHasRecovery) {
                            [Console]::Out.Write($script:EmptyOutput)
                            exit 0
                        }

                        $fastLines = New-Object 'System.Collections.Generic.List[string]'
                        [void]$fastLines.Add(
                            'KD4 task continuity recovery (capsule v1; bounded and redacted).'
                        )
                        [void]$fastLines.Add("Session: $($fastCapsule.session_id)")
                        [void]$fastLines.Add(
                            "Continuity epoch: $($fastCapsule.continuity_epoch)"
                        )
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.predecessor_thread_id
                        )) {
                            [void]$fastLines.Add(
                                "Predecessor thread: $($fastCapsule.predecessor_thread_id)"
                            )
                        }
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.task_label
                        )) {
                            [void]$fastLines.Add("Task label: $($fastCapsule.task_label)")
                        }
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.last_user_request
                        )) {
                            [void]$fastLines.Add('Last user request:')
                            [void]$fastLines.Add([string]$fastCapsule.last_user_request)
                        }
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.last_assistant_result
                        )) {
                            [void]$fastLines.Add('Last assistant result:')
                            [void]$fastLines.Add([string]$fastCapsule.last_assistant_result)
                        }
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.repository.root
                        )) {
                            [void]$fastLines.Add(
                                "Repository: $($fastCapsule.repository.root) @ " +
                                    [string]$fastCapsule.repository.revision
                            )
                            [void]$fastLines.Add(
                                "Dirty summary: $($fastCapsule.repository.dirty_summary)"
                            )
                        }
                        [void]$fastLines.Add(
                            "Compaction state: $($fastCapsule.compaction.phase)"
                        )
                        if (-not [string]::IsNullOrWhiteSpace(
                            [string]$fastCapsule.transcript_path
                        )) {
                            [void]$fastLines.Add(
                                "Transcript: $($fastCapsule.transcript_path)"
                            )
                        }
                        [void]$fastLines.Add(
                            'Treat this as recovery context only; reconcile it with the ' +
                                'current workspace and request before acting.'
                        )
                        $fastContext = $fastLines -join "`n"
                        if ($fastContext.IndexOf([char]0) -ge 0) {
                            throw 'fast recovery context contains a NUL character'
                        }
                        $fastContext = [regex]::Replace(
                            $fastContext,
                            '(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+',
                            'Bearer [REDACTED]'
                        )
                        $fastContext = [regex]::Replace(
                            $fastContext,
                            '\bsk-(?:proj-)?[A-Za-z0-9_-]{8,}\b',
                            '[REDACTED]'
                        )
                        $fastContext = [regex]::Replace(
                            $fastContext,
                            '(?i)(\b(?:api[_-]?key|token|password|secret)\b\s*[:=]\s*)(?:"[^"]*"|''[^'']*''|[^\s,;]+)',
                            '$1[REDACTED]'
                        )
                        if ($fastContext.Length -gt $script:MaxContextChars) {
                            $fastContext = $fastContext.Substring(
                                0,
                                $script:MaxContextChars - 3
                            ) + '...'
                        }
                        $fastOutput = [pscustomobject][ordered]@{
                            hookSpecificOutput = [pscustomobject][ordered]@{
                                hookEventName = 'SessionStart'
                                additionalContext = $fastContext
                            }
                        } | ConvertTo-Json -Depth 5 -Compress
                        [Console]::Out.Write($fastOutput)
                        exit 0
                    }
                }
            }
        }
    }
}
catch {
    # The full path owns validation and diagnostics for every uncertain case.
}

$script:SlowImplementation = @'
$script:ParsedInput = $null
$script:InputParseError = $null
try {
    if ([string]::IsNullOrWhiteSpace($script:RawInput)) {
        throw 'hook stdin was empty'
    }
    $script:ParsedInput = $script:RawInput | ConvertFrom-Json -ErrorAction Stop
}
catch {
    $script:InputParseError = $_.Exception
}

function Write-Diagnostic {
    param([string]$Message)

    try {
        [Console]::Error.WriteLine("task-continuity: $Message")
    }
    catch {
        # Diagnostics must never interfere with the hook result.
    }
}

function Get-UtcTimestamp {
    return [DateTime]::UtcNow.ToString('o', [Globalization.CultureInfo]::InvariantCulture)
}

function Get-OptionalProperty {
    param(
        [object]$Object,
        [string]$Name
    )

    if ($null -eq $Object) {
        return $null
    }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        return $null
    }
    return $property.Value
}

function Test-HasProperty {
    param(
        [object]$Object,
        [string]$Name
    )

    return $null -ne $Object -and $null -ne $Object.PSObject.Properties[$Name]
}

function Get-RequiredString {
    param(
        [object]$Object,
        [string]$Name,
        [switch]$AllowEmpty
    )

    $value = Get-OptionalProperty -Object $Object -Name $Name
    if ($value -isnot [string]) {
        throw "input property '$Name' must be a string"
    }
    if (-not $AllowEmpty -and [string]::IsNullOrWhiteSpace($value)) {
        throw "input property '$Name' must not be empty"
    }
    return $value
}

function Get-OptionalString {
    param(
        [object]$Object,
        [string]$Name
    )

    $value = Get-OptionalProperty -Object $Object -Name $Name
    if ($null -eq $value) {
        return $null
    }
    if ($value -isnot [string]) {
        throw "input property '$Name' must be a string or null"
    }
    return $value
}

function ConvertTo-SessionId {
    param([string]$Value)

    $parsed = [Guid]::Empty
    if (-not [Guid]::TryParse($Value, [ref]$parsed)) {
        throw "session identity is not a UUID"
    }
    return $parsed.ToString('D').ToLowerInvariant()
}

function Get-CapsulePath {
    param([string]$SessionId)

    $normalized = ConvertTo-SessionId -Value $SessionId
    return Join-Path $script:StateDirectory "$normalized.json"
}

function Get-RedactedExcerpt {
    param(
        [AllowNull()][object]$Value,
        [int]$MaximumCharacters = $script:MaxExcerptChars
    )

    if ($null -eq $Value) {
        return $null
    }
    if ($Value -isnot [string]) {
        throw 'redaction input must be a string or null'
    }
    if ($Value.IndexOf([char]0) -ge 0) {
        throw 'redaction rejected a NUL character'
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

function Get-TaskLabel {
    param([string]$Prompt)

    $candidate = $null
    foreach ($line in ($Prompt -split "`n")) {
        if (-not [string]::IsNullOrWhiteSpace($line)) {
            $candidate = $line.Trim()
            break
        }
    }
    if ([string]::IsNullOrWhiteSpace($candidate)) {
        return 'Untitled task'
    }
    $candidate = [regex]::Replace($candidate, '\s+', ' ')
    if ($candidate.Length -gt 80) {
        return $candidate.Substring(0, 77) + '...'
    }
    return $candidate
}

function New-RepositoryState {
    return [pscustomobject][ordered]@{
        root = $null
        revision = $null
        dirty_summary = $null
    }
}

function Get-RepositoryState {
    param([string]$WorkingDirectory)

    $empty = New-RepositoryState
    try {
        if (-not (Test-Path -LiteralPath $WorkingDirectory -PathType Container)) {
            throw 'working directory does not exist'
        }

        $rootLines = @(& git.exe -C $WorkingDirectory rev-parse --show-toplevel 2>$null)
        if ($LASTEXITCODE -ne 0 -or $rootLines.Count -eq 0) {
            throw 'working directory is not a Git repository'
        }
        $statusLines = @(
            & git.exe -C $WorkingDirectory status --porcelain=v2 --branch 2>$null
        )
        if ($LASTEXITCODE -ne 0) {
            throw 'Git status lookup failed'
        }

        $root = ($rootLines -join "`n").Trim()
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
        if ([string]::IsNullOrWhiteSpace($revision) -or $revision -eq '(initial)') {
            throw 'Git revision lookup failed'
        }
        $boundedDirtyLines = @($dirtyLines | Select-Object -First 50)
        $dirty = if ($boundedDirtyLines.Count -eq 0) {
            'clean'
        }
        else {
            $summary = $boundedDirtyLines -join "`n"
            if ($dirtyLines.Count -gt 50) {
                $summary += "`n..."
            }
            Get-RedactedExcerpt -Value $summary -MaximumCharacters 2000
        }

        return [pscustomobject][ordered]@{
            root = Get-RedactedExcerpt -Value $root -MaximumCharacters 1000
            revision = Get-RedactedExcerpt -Value $revision -MaximumCharacters 200
            dirty_summary = $dirty
        }
    }
    catch {
        Write-Diagnostic "Git state unavailable: $($_.Exception.Message)"
        return $empty
    }
}

function New-CompactionState {
    return [pscustomobject][ordered]@{
        phase = 'none'
        trigger = $null
    }
}

function New-Capsule {
    param(
        [string]$SessionId,
        [int]$ContinuityEpoch,
        [string]$WorkingDirectory,
        [AllowNull()][object]$TranscriptPath,
        [AllowNull()][object]$PredecessorThreadId,
        [AllowNull()][object]$Seed
    )

    $now = Get-UtcTimestamp
    $storedTranscriptPath = if ($null -eq $TranscriptPath) { $null } else { [string]$TranscriptPath }
    $storedPredecessorId = if ($null -eq $PredecessorThreadId) {
        $null
    }
    else {
        [string]$PredecessorThreadId
    }
    $capsule = [pscustomobject][ordered]@{
        schema_version = $script:SchemaVersion
        session_id = $SessionId
        continuity_epoch = $ContinuityEpoch
        predecessor_thread_id = $storedPredecessorId
        created_at = $now
        updated_at = $now
        last_event = $null
        last_turn_id = $null
        working_directory = $WorkingDirectory
        transcript_path = $storedTranscriptPath
        task_label = $null
        last_user_request = $null
        last_assistant_result = $null
        repository = New-RepositoryState
        compaction = New-CompactionState
        material_digest = $null
    }

    if ($null -ne $Seed) {
        $capsule.continuity_epoch = [int]$Seed.continuity_epoch
        $capsule.task_label = Get-RedactedExcerpt -Value $Seed.task_label -MaximumCharacters 80
        $capsule.last_user_request = Get-RedactedExcerpt -Value $Seed.last_user_request
        $capsule.last_assistant_result = Get-RedactedExcerpt -Value $Seed.last_assistant_result
        if ([string]$Seed.compaction.phase -eq 'post') {
            $capsule.compaction = [pscustomobject][ordered]@{
                phase = 'post'
                trigger = Get-RedactedExcerpt -Value $Seed.compaction.trigger -MaximumCharacters 100
            }
        }
    }
    return $capsule
}

function Assert-Capsule {
    param(
        [object]$Capsule,
        [string]$ExpectedSessionId
    )

    if ($Capsule -isnot [pscustomobject]) {
        throw 'capsule root must be a JSON object'
    }
    $schemaVersion = Get-OptionalProperty -Object $Capsule -Name 'schema_version'
    if ([string]$schemaVersion -ne [string]$script:SchemaVersion) {
        throw "unsupported capsule schema version '$schemaVersion'"
    }
    $capsuleSessionId = ConvertTo-SessionId -Value (
        Get-RequiredString -Object $Capsule -Name 'session_id'
    )
    if ($capsuleSessionId -ne $ExpectedSessionId) {
        throw 'capsule session identity does not match its filename'
    }

    $epoch = 0
    $epochValue = Get-OptionalProperty -Object $Capsule -Name 'continuity_epoch'
    if (-not [int]::TryParse([string]$epochValue, [ref]$epoch) -or $epoch -lt 0) {
        throw 'capsule continuity_epoch must be a non-negative integer'
    }
    foreach ($name in @('created_at', 'updated_at', 'working_directory')) {
        [void](Get-RequiredString -Object $Capsule -Name $name)
    }
    foreach ($name in @(
        'predecessor_thread_id',
        'last_event',
        'last_turn_id',
        'transcript_path',
        'task_label',
        'last_user_request',
        'last_assistant_result',
        'material_digest'
    )) {
        [void](Get-OptionalString -Object $Capsule -Name $name)
    }
    if (-not (Test-HasProperty -Object $Capsule -Name 'repository') -or
        $Capsule.repository -isnot [pscustomobject]) {
        throw 'capsule repository state is missing or invalid'
    }
    foreach ($name in @('root', 'revision', 'dirty_summary')) {
        [void](Get-OptionalString -Object $Capsule.repository -Name $name)
    }
    if (-not (Test-HasProperty -Object $Capsule -Name 'compaction') -or
        $Capsule.compaction -isnot [pscustomobject]) {
        throw 'capsule compaction state is missing or invalid'
    }
    $phase = Get-RequiredString -Object $Capsule.compaction -Name 'phase'
    if ($phase -notin @('none', 'pre', 'post')) {
        throw "capsule compaction phase '$phase' is invalid"
    }
    [void](Get-OptionalString -Object $Capsule.compaction -Name 'trigger')
    return $Capsule
}

function Read-Capsule {
    param(
        [string]$Path,
        [string]$ExpectedSessionId
    )

    if (-not [System.IO.File]::Exists($Path)) {
        return $null
    }
    $json = [System.IO.File]::ReadAllText($Path, [Text.Encoding]::UTF8)
    if ([string]::IsNullOrWhiteSpace($json)) {
        throw 'capsule file is empty'
    }
    $capsule = $json | ConvertFrom-Json -ErrorAction Stop
    return Assert-Capsule -Capsule $capsule -ExpectedSessionId $ExpectedSessionId
}

function Get-MaterialDigest {
    param([object]$Capsule)

    $material = [pscustomobject][ordered]@{
        schema_version = $Capsule.schema_version
        session_id = $Capsule.session_id
        continuity_epoch = $Capsule.continuity_epoch
        predecessor_thread_id = $Capsule.predecessor_thread_id
        last_event = $Capsule.last_event
        working_directory = $Capsule.working_directory
        transcript_path = $Capsule.transcript_path
        task_label = $Capsule.task_label
        last_user_request = $Capsule.last_user_request
        last_assistant_result = $Capsule.last_assistant_result
        repository = $Capsule.repository
        compaction = $Capsule.compaction
    }
    $json = $material | ConvertTo-Json -Depth 8 -Compress
    $bytes = $script:Utf8NoBom.GetBytes($json)
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha256.ComputeHash($bytes)
    }
    finally {
        $sha256.Dispose()
    }
    return ([BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

function Write-CapsuleAtomic {
    param(
        [object]$Capsule,
        [string]$Path
    )

    [void][System.IO.Directory]::CreateDirectory($script:StateDirectory)
    $json = $Capsule | ConvertTo-Json -Depth 8 -Compress
    $temporaryPath = Join-Path $script:StateDirectory (
        '.{0}.{1}.tmp' -f [System.IO.Path]::GetFileName($Path), [Guid]::NewGuid().ToString('N')
    )
    $backupPath = Join-Path $script:StateDirectory (
        '.{0}.{1}.bak' -f [System.IO.Path]::GetFileName($Path), [Guid]::NewGuid().ToString('N')
    )
    try {
        [System.IO.File]::WriteAllText($temporaryPath, $json, $script:Utf8NoBom)
        if ([System.IO.File]::Exists($Path)) {
            [System.IO.File]::Replace($temporaryPath, $Path, $backupPath, $true)
        }
        else {
            [System.IO.File]::Move($temporaryPath, $Path)
        }
    }
    finally {
        if ([System.IO.File]::Exists($temporaryPath)) {
            [System.IO.File]::Delete($temporaryPath)
        }
        if ([System.IO.File]::Exists($backupPath)) {
            [System.IO.File]::Delete($backupPath)
        }
    }
}

function Save-Capsule {
    param(
        [object]$Capsule,
        [string]$Path,
        [AllowNull()][string]$PreviousDigest,
        [switch]$PreserveIfUnchanged
    )

    $newDigest = Get-MaterialDigest -Capsule $Capsule
    if ($PreserveIfUnchanged -and $null -ne $PreviousDigest -and
        $PreviousDigest -eq $newDigest) {
        return $false
    }
    $Capsule.updated_at = Get-UtcTimestamp
    $Capsule.material_digest = $newDigest
    Write-CapsuleAtomic -Capsule $Capsule -Path $Path
    return $true
}

function Set-CommonEventFields {
    param(
        [object]$Capsule,
        [object]$InputObject,
        [string]$EventName,
        [string]$WorkingDirectory,
        [AllowNull()][string]$TranscriptPath
    )

    $Capsule.last_event = $EventName
    $Capsule.working_directory = $WorkingDirectory
    $Capsule.transcript_path = $TranscriptPath
    if (Test-HasProperty -Object $InputObject -Name 'turn_id') {
        $Capsule.last_turn_id = Get-OptionalString -Object $InputObject -Name 'turn_id'
    }
}

function Get-ForkedFromId {
    param([AllowNull()][string]$TranscriptPath)

    if ([string]::IsNullOrWhiteSpace($TranscriptPath)) {
        Write-Diagnostic 'transcript path is unavailable; fork lineage was not restored'
        return $null
    }
    if (-not [System.IO.File]::Exists($TranscriptPath)) {
        Write-Diagnostic "transcript '$TranscriptPath' is missing; fork lineage was not restored"
        return $null
    }

    try {
        $lineNumber = 0
        foreach ($line in (Get-Content -LiteralPath $TranscriptPath -TotalCount 64 -ErrorAction Stop)) {
            $lineNumber += 1
            if ([string]::IsNullOrWhiteSpace($line)) {
                continue
            }
            try {
                $entry = $line | ConvertFrom-Json -ErrorAction Stop
            }
            catch {
                Write-Diagnostic "ignored malformed transcript JSON on line $lineNumber"
                continue
            }
            if ([string](Get-OptionalProperty -Object $entry -Name 'type') -ne 'session_meta') {
                continue
            }
            $payload = Get-OptionalProperty -Object $entry -Name 'payload'
            if ($null -eq $payload) {
                continue
            }
            $forkedFrom = Get-OptionalProperty -Object $payload -Name 'forked_from_id'
            if ($null -eq $forkedFrom -or [string]::IsNullOrWhiteSpace([string]$forkedFrom)) {
                return $null
            }
            try {
                return ConvertTo-SessionId -Value ([string]$forkedFrom)
            }
            catch {
                Write-Diagnostic 'transcript forked_from_id is not a UUID'
                return $null
            }
        }
        return $null
    }
    catch {
        Write-Diagnostic "transcript could not be read: $($_.Exception.Message)"
        return $null
    }
}

function Invoke-Retention {
    param([string]$CurrentSessionId)

    [void][System.IO.Directory]::CreateDirectory($script:StateDirectory)
    $currentName = "$CurrentSessionId.json"
    $cutoff = [DateTime]::UtcNow.AddDays(-30)
    $inactive = @(
        Get-ChildItem -LiteralPath $script:StateDirectory -Filter '*.json' -File |
            Where-Object { $_.Name -ne $currentName }
    )

    foreach ($file in @($inactive | Where-Object { $_.LastWriteTimeUtc -lt $cutoff })) {
        try {
            Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
        }
        catch {
            Write-Diagnostic "retention could not remove '$($file.Name)': $($_.Exception.Message)"
        }
    }

    $remaining = @(
        Get-ChildItem -LiteralPath $script:StateDirectory -Filter '*.json' -File |
            Where-Object { $_.Name -ne $currentName } |
            Sort-Object LastWriteTimeUtc -Descending
    )
    foreach ($file in @($remaining | Select-Object -Skip 100)) {
        try {
            Remove-Item -LiteralPath $file.FullName -Force -ErrorAction Stop
        }
        catch {
            Write-Diagnostic "retention could not remove '$($file.Name)': $($_.Exception.Message)"
        }
    }
}

function Build-RecoveryContext {
    param([object]$Capsule)

    $hasRecovery = -not [string]::IsNullOrWhiteSpace([string]$Capsule.last_user_request) -or
        -not [string]::IsNullOrWhiteSpace([string]$Capsule.last_assistant_result) -or
        -not [string]::IsNullOrWhiteSpace([string]$Capsule.predecessor_thread_id)
    if (-not $hasRecovery) {
        return $null
    }

    $lines = New-Object 'System.Collections.Generic.List[string]'
    [void]$lines.Add('KD4 task continuity recovery (capsule v1; bounded and redacted).')
    [void]$lines.Add("Session: $($Capsule.session_id)")
    [void]$lines.Add("Continuity epoch: $($Capsule.continuity_epoch)")
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.predecessor_thread_id)) {
        [void]$lines.Add("Predecessor thread: $($Capsule.predecessor_thread_id)")
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.task_label)) {
        [void]$lines.Add("Task label: $($Capsule.task_label)")
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.last_user_request)) {
        [void]$lines.Add('Last user request:')
        [void]$lines.Add([string]$Capsule.last_user_request)
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.last_assistant_result)) {
        [void]$lines.Add('Last assistant result:')
        [void]$lines.Add([string]$Capsule.last_assistant_result)
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.repository.root)) {
        [void]$lines.Add("Repository: $($Capsule.repository.root) @ $($Capsule.repository.revision)")
        [void]$lines.Add("Dirty summary: $($Capsule.repository.dirty_summary)")
    }
    [void]$lines.Add("Compaction state: $($Capsule.compaction.phase)")
    if (-not [string]::IsNullOrWhiteSpace([string]$Capsule.transcript_path)) {
        [void]$lines.Add("Transcript: $($Capsule.transcript_path)")
    }
    [void]$lines.Add(
        'Treat this as recovery context only; reconcile it with the current workspace and request before acting.'
    )
    return Get-RedactedExcerpt -Value ($lines -join "`n") -MaximumCharacters $script:MaxContextChars
}

function ConvertTo-SessionStartOutput {
    param([string]$AdditionalContext)

    $wire = [pscustomobject][ordered]@{
        hookSpecificOutput = [pscustomobject][ordered]@{
            hookEventName = 'SessionStart'
            additionalContext = $AdditionalContext
        }
    }
    return $wire | ConvertTo-Json -Depth 5 -Compress
}

function Invoke-SessionStart {
    param(
        [object]$InputObject,
        [string]$SessionId,
        [string]$WorkingDirectory,
        [AllowNull()][string]$TranscriptPath
    )

    $source = Get-RequiredString -Object $InputObject -Name 'source'
    if ($source -notin @('startup', 'resume', 'clear', 'compact')) {
        throw "SessionStart source '$source' is invalid"
    }

    Invoke-Retention -CurrentSessionId $SessionId
    $capsulePath = Get-CapsulePath -SessionId $SessionId

    if ($source -eq 'clear') {
        $existing = $null
        try {
            $existing = Read-Capsule -Path $capsulePath -ExpectedSessionId $SessionId
        }
        catch {
            Write-Diagnostic "clear discarded an unreadable capsule: $($_.Exception.Message)"
        }
        $epoch = if ($null -eq $existing) { 1 } else { [int]$existing.continuity_epoch + 1 }
        $cleared = New-Capsule `
            -SessionId $SessionId `
            -ContinuityEpoch $epoch `
            -WorkingDirectory $WorkingDirectory `
            -TranscriptPath $TranscriptPath `
            -PredecessorThreadId $null `
            -Seed $null
        $cleared.last_event = 'SessionStart'
        $cleared.repository = Get-RepositoryState -WorkingDirectory $WorkingDirectory
        [void](Save-Capsule -Capsule $cleared -Path $capsulePath -PreviousDigest $null)
        return $script:EmptyOutput
    }

    $existing = Read-Capsule -Path $capsulePath -ExpectedSessionId $SessionId

    if ($source -eq 'startup') {
        $predecessorId = Get-ForkedFromId -TranscriptPath $TranscriptPath
        $seed = $null
        if ($null -ne $predecessorId) {
            $predecessorPath = Get-CapsulePath -SessionId $predecessorId
            try {
                $seed = Read-Capsule -Path $predecessorPath -ExpectedSessionId $predecessorId
            }
            catch {
                Write-Diagnostic "predecessor capsule could not be restored: $($_.Exception.Message)"
            }
            if ($null -eq $seed) {
                Write-Diagnostic "predecessor capsule '$predecessorId' is unavailable"
            }
        }

        $epoch = if ($null -eq $seed) { 0 } else { [int]$seed.continuity_epoch }
        $startupCapsule = New-Capsule `
            -SessionId $SessionId `
            -ContinuityEpoch $epoch `
            -WorkingDirectory $WorkingDirectory `
            -TranscriptPath $TranscriptPath `
            -PredecessorThreadId $predecessorId `
            -Seed $seed
        $startupCapsule.last_event = 'SessionStart'
        $startupCapsule.repository = Get-RepositoryState -WorkingDirectory $WorkingDirectory
        [void](Save-Capsule -Capsule $startupCapsule -Path $capsulePath -PreviousDigest $null)

        if ($null -ne $seed) {
            $context = Build-RecoveryContext -Capsule $startupCapsule
            if ($null -ne $context) {
                return ConvertTo-SessionStartOutput -AdditionalContext $context
            }
        }
        return $script:EmptyOutput
    }

    if ($null -eq $existing) {
        if ($source -eq 'compact') {
            Write-Diagnostic 'compact SessionStart has no capsule to restore'
            return $script:EmptyOutput
        }
        Write-Diagnostic 'resume capsule is missing; starting with an empty continuity epoch'
        $existing = New-Capsule `
            -SessionId $SessionId `
            -ContinuityEpoch 0 `
            -WorkingDirectory $WorkingDirectory `
            -TranscriptPath $TranscriptPath `
            -PredecessorThreadId $null `
            -Seed $null
    }

    if ($source -eq 'compact' -and [string]$existing.compaction.phase -ne 'post') {
        Write-Diagnostic 'compact SessionStart ignored a capsule that is not post-compaction'
        return $script:EmptyOutput
    }

    $previousDigest = Get-OptionalString -Object $existing -Name 'material_digest'
    Set-CommonEventFields `
        -Capsule $existing `
        -InputObject $InputObject `
        -EventName 'SessionStart' `
        -WorkingDirectory $WorkingDirectory `
        -TranscriptPath $TranscriptPath
    $existing.repository = Get-RepositoryState -WorkingDirectory $WorkingDirectory
    [void](Save-Capsule `
        -Capsule $existing `
        -Path $capsulePath `
        -PreviousDigest $previousDigest `
        -PreserveIfUnchanged)

    $context = Build-RecoveryContext -Capsule $existing
    if ($null -eq $context) {
        return $script:EmptyOutput
    }
    return ConvertTo-SessionStartOutput -AdditionalContext $context
}

function Invoke-TaskContinuity {
    param([object]$InputObject)

    if ($InputObject -isnot [pscustomobject]) {
        throw 'hook input must be a JSON object'
    }
    if (Test-HasProperty -Object $InputObject -Name 'agent_id') {
        return $script:EmptyOutput
    }

    $eventName = Get-RequiredString -Object $InputObject -Name 'hook_event_name'
    if ($eventName -notin @(
        'UserPromptSubmit',
        'PreCompact',
        'PostCompact',
        'SessionStart',
        'Stop'
    )) {
        throw "hook event '$eventName' is unsupported"
    }
    $sessionId = ConvertTo-SessionId -Value (
        Get-RequiredString -Object $InputObject -Name 'session_id'
    )
    $workingDirectory = Get-RequiredString -Object $InputObject -Name 'cwd'
    $transcriptPath = Get-OptionalString -Object $InputObject -Name 'transcript_path'

    if ($eventName -eq 'SessionStart') {
        return Invoke-SessionStart `
            -InputObject $InputObject `
            -SessionId $sessionId `
            -WorkingDirectory $workingDirectory `
            -TranscriptPath $transcriptPath
    }

    $capsulePath = Get-CapsulePath -SessionId $sessionId
    $capsule = Read-Capsule -Path $capsulePath -ExpectedSessionId $sessionId
    if ($null -eq $capsule) {
        $capsule = New-Capsule `
            -SessionId $sessionId `
            -ContinuityEpoch 0 `
            -WorkingDirectory $workingDirectory `
            -TranscriptPath $transcriptPath `
            -PredecessorThreadId $null `
            -Seed $null
    }
    $previousDigest = Get-OptionalString -Object $capsule -Name 'material_digest'
    Set-CommonEventFields `
        -Capsule $capsule `
        -InputObject $InputObject `
        -EventName $eventName `
        -WorkingDirectory $workingDirectory `
        -TranscriptPath $transcriptPath

    switch ($eventName) {
        'UserPromptSubmit' {
            $prompt = Get-RequiredString -Object $InputObject -Name 'prompt' -AllowEmpty
            $capsule.last_user_request = Get-RedactedExcerpt -Value $prompt
            $capsule.task_label = Get-TaskLabel -Prompt $capsule.last_user_request
            [void](Save-Capsule `
                -Capsule $capsule `
                -Path $capsulePath `
                -PreviousDigest $previousDigest `
                -PreserveIfUnchanged)
        }
        'PreCompact' {
            $trigger = Get-RequiredString -Object $InputObject -Name 'trigger'
            $capsule.compaction = [pscustomobject][ordered]@{
                phase = 'pre'
                trigger = Get-RedactedExcerpt -Value $trigger -MaximumCharacters 100
            }
            $capsule.repository = Get-RepositoryState -WorkingDirectory $workingDirectory
            [void](Save-Capsule `
                -Capsule $capsule `
                -Path $capsulePath `
                -PreviousDigest $previousDigest `
                -PreserveIfUnchanged)
        }
        'PostCompact' {
            $trigger = Get-RequiredString -Object $InputObject -Name 'trigger'
            $capsule.compaction = [pscustomobject][ordered]@{
                phase = 'post'
                trigger = Get-RedactedExcerpt -Value $trigger -MaximumCharacters 100
            }
            $capsule.repository = Get-RepositoryState -WorkingDirectory $workingDirectory
            [void](Save-Capsule `
                -Capsule $capsule `
                -Path $capsulePath `
                -PreviousDigest $previousDigest `
                -PreserveIfUnchanged)
        }
        'Stop' {
            $assistantResult = Get-OptionalString -Object $InputObject -Name 'last_assistant_message'
            $capsule.last_assistant_result = Get-RedactedExcerpt -Value $assistantResult

            $fastDigest = Get-MaterialDigest -Capsule $capsule
            if ($null -ne $previousDigest -and $fastDigest -eq $previousDigest) {
                return $script:EmptyOutput
            }
            $capsule.repository = Get-RepositoryState -WorkingDirectory $workingDirectory
            [void](Save-Capsule `
                -Capsule $capsule `
                -Path $capsulePath `
                -PreviousDigest $previousDigest `
                -PreserveIfUnchanged)
        }
    }
    return $script:EmptyOutput
}

$hookOutput = $script:EmptyOutput
try {
    if ($null -ne $script:InputParseError) {
        throw $script:InputParseError
    }
    $hookOutput = Invoke-TaskContinuity -InputObject $script:ParsedInput
    if ([string]::IsNullOrWhiteSpace($hookOutput)) {
        $hookOutput = $script:EmptyOutput
    }
}
catch {
    Write-Diagnostic $_.Exception.Message
    $hookOutput = $script:EmptyOutput
}

[Console]::Out.Write($hookOutput)
exit 0
'@

$slowScriptBlock = [ScriptBlock]::Create($script:SlowImplementation)
& $slowScriptBlock
exit 0
