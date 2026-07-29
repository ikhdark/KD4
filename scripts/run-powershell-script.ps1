<#
.SYNOPSIS
Runs a PowerShell script body or script file through PowerShell -EncodedCommand.

.DESCRIPTION
This is an opt-in helper for local commands that are awkward to express as a
nested PowerShell string. It does not lint, block, or rewrite other commands.
#>
[CmdletBinding(DefaultParameterSetName = 'Body')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Body')]
    [string]$ScriptBody,

    [Parameter(Mandatory = $true, ParameterSetName = 'File')]
    [string]$ScriptFile,

    [string]$PowerShellPath,

    [switch]$UseProfile,

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ArgumentList = @()
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($PowerShellPath)) {
    $command = Get-Command pwsh -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $command) {
        $command = Get-Command powershell.exe -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
    }
    if ($null -eq $command) {
        throw 'PowerShell executable not found. Install pwsh or pass -PowerShellPath.'
    }
    $PowerShellPath = $command.Source
}

if ($PSCmdlet.ParameterSetName -eq 'File') {
    $resolvedScript = Resolve-Path -LiteralPath $ScriptFile
    $encodedScriptPath = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($resolvedScript.Path))
    $ScriptBody = ''
}
else {
    $encodedScriptPath = ''
}

$encodedScriptBody = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($ScriptBody))
$encodedArgumentList = [Convert]::ToBase64String(
    [Text.Encoding]::Unicode.GetBytes((ConvertTo-Json -Compress -InputObject @($ArgumentList)))
)
$runner = @"
`$argumentJson = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('$encodedArgumentList'))
`[string[]]`$argumentList = ConvertFrom-Json -InputObject `$argumentJson
if ('$encodedScriptPath') {
    `$scriptPath = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('$encodedScriptPath'))
    & `$scriptPath @argumentList
}
else {
    `$scriptBody = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String('$encodedScriptBody'))
    . ([scriptblock]::Create(`$scriptBody)) @argumentList
}
if (`$LASTEXITCODE -is [int]) {
    exit `$LASTEXITCODE
}
"@
$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($runner))
$powerShellArgs = @('-NoLogo')
if (-not $UseProfile) {
    $powerShellArgs += '-NoProfile'
}
$powerShellArgs += @('-EncodedCommand', $encoded)

if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
    $commandLineLength = $PowerShellPath.Length
    foreach ($argument in $powerShellArgs) {
        # Include a separating space and worst-case surrounding quotes.
        $commandLineLength += $argument.Length + 3
    }
    if ($commandLineLength -ge 32767) {
        throw "Encoded command is too large for the Windows command-line limit ($commandLineLength characters). Use -ScriptFile for large script bodies."
    }
}

& $PowerShellPath @powerShellArgs
exit $LASTEXITCODE
