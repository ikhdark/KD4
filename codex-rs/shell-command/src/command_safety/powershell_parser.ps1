$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Long-lived PowerShell AST parser used by the Rust command-safety layer on Windows.
# The caller starts one child process per trusted PowerShell language flavor and then sends
# newline-delimited JSON requests over stdin:
#   { "id": <u64>, "payload": "<base64-encoded UTF-16LE script>",
#     "resolution": { "cwd": "...", "path": "...", "pathext": "..." } | null }
# We answer with one compact JSON line per request:
#   { "id": <same>, "status": "ok", "commands": [["Get-Content", "foo.txt"]],
#     "direct_argv": ["git", "status"], "native_argument_mode": "Standard",
#     "powershell_version": "7.5.2",
#     "resolved_application": "C:\\Program Files\\Git\\cmd\\git.exe" | null }
# or:
#   { "id": <same>, "status": "parse_failed" | "parse_errors" | "unsupported" }
#
# "unsupported" is intentional: it means the script parsed successfully, but the AST
# included constructs that we conservatively refuse to lower into argv-like command words.
# The Rust side treats that the same way as an unsafe command.

# Use BOM-free UTF-8 on the protocol stream so Rust sees clean JSON lines with no
# leading BOM bytes on the first response.
$utf8 = [System.Text.UTF8Encoding]::new($false)
$stdin = [System.IO.StreamReader]::new([Console]::OpenStandardInput(), $utf8, $false)
$stdout = [System.IO.StreamWriter]::new([Console]::OpenStandardOutput(), $utf8)
$stdout.AutoFlush = $true

function Invoke-ParseRequest {
    param($RequestId, $Source, $Resolution)

    $tokens = $null
    $errors = $null

    $ast = $null
    try {
        $ast = [System.Management.Automation.Language.Parser]::ParseInput(
            $Source,
            [ref]$tokens,
            [ref]$errors
        )
    } catch {
        return @{ id = $RequestId; status = 'parse_failed' }
    }

    if ($errors.Count -gt 0) {
        return @{ id = $RequestId; status = 'parse_errors' }
    }

    # Top-level AST regions and collections outside the end-block statement list
    # can execute code that the command lowering below does not inspect.
    $cleanBlock = $ast.PSObject.Properties['CleanBlock']
    if (
        $ast.ParamBlock -ne $null -or
        $ast.DynamicParamBlock -ne $null -or
        $ast.BeginBlock -ne $null -or
        $ast.ProcessBlock -ne $null -or
        ($cleanBlock -ne $null -and $cleanBlock.Value -ne $null) -or
        $ast.UsingStatements.Count -gt 0 -or
        $ast.EndBlock.Traps.Count -gt 0
    ) {
        return @{ id = $RequestId; status = 'unsupported' }
    }

    # PowerShell's stop-parsing marker hands the remaining source text to native
    # commands with runtime argument handling that does not match the AST shape we
    # flatten below. Keep that form out of the argv-like lowering path entirely.
    foreach ($token in $tokens) {
        if ($token.Text -eq '--%') {
            return @{ id = $RequestId; status = 'unsupported' }
        }
    }

    # Only accept AST shapes we can flatten into a list of argv-like command words.
    # Anything more dynamic than that becomes "unsupported" instead of being guessed at.
    $commands = [System.Collections.ArrayList]::new()
    $localConstants = @{}
    $commandOutputVariables = @{}

    foreach ($statement in $ast.EndBlock.Statements) {
        if ($statement -is [System.Management.Automation.Language.AssignmentStatementAst]) {
            if (Add-LocalScalarConstant $statement $localConstants) {
                $name = $statement.Left.VariablePath.UserPath.ToLowerInvariant()
                $null = $commandOutputVariables.Remove($name)
                continue
            }

            if (Add-CommandOutputAssignment $statement $commands $localConstants $commandOutputVariables) {
                continue
            }

            $commands = $null
            break
        }

        if (Test-IsBareCommandOutputRead $statement $commandOutputVariables) {
            continue
        }

        if (-not (Add-CommandsFromPipelineBase $statement $commands $localConstants)) {
            $commands = $null
            break
        }
    }

    if ($commands -ne $null) {
        $normalized = [System.Collections.ArrayList]::new()
        foreach ($cmd in $commands) {
            # Convert every successful parse result to an array-of-arrays shape so the Rust
            # side can deserialize one uniform representation.
            if ($cmd -is [string]) {
                $null = $normalized.Add(@($cmd))
                continue
            }

            if ($cmd -is [System.Array] -or $cmd -is [System.Collections.IEnumerable]) {
                $null = $normalized.Add(@($cmd))
                continue
            }

            $normalized = $null
            break
        }

        $commands = $normalized
    }

    if ($commands -eq $null) {
        return @{ id = $RequestId; status = 'unsupported' }
    }

    $directArgv = Get-DirectArgvCandidate $ast
    $nativeArgumentMode = $null
    $powershellVersion = $null
    if ($directArgv -ne $null) {
        # Windows PowerShell and PowerShell's Legacy mode reconstruct one native command line.
        # That is not unambiguously equivalent to passing an argv vector, especially for empty
        # and quoted arguments. Standard and Windows mode both use the modern argv contract for
        # .exe applications; the Rust side separately proves that the resolved target is an .exe.
        $modeVariable = Get-Variable -Name PSNativeCommandArgumentPassing -ErrorAction SilentlyContinue
        if ($modeVariable -ne $null -and $PSVersionTable.PSVersion.Major -eq 7) {
            $mode = $modeVariable.Value.ToString()
            if ($mode -eq 'Standard' -or $mode -eq 'Windows') {
                $nativeArgumentMode = $mode
                $powershellVersion = $PSVersionTable.PSVersion.ToString()
            }
        }

        if ($nativeArgumentMode -eq $null) {
            $directArgv = $null
        }
    }

    $resolvedApplication = $null
    if ($directArgv -ne $null -and $Resolution -ne $null) {
        $resolvedApplication = Resolve-ApplicationAgainstState ($directArgv[0]) $Resolution
    }

    return @{
        id = $RequestId
        status = 'ok'
        commands = $commands
        direct_argv = $directArgv
        native_argument_mode = $nativeArgumentMode
        powershell_version = $powershellVersion
        resolved_application = $resolvedApplication
    }
}

function Write-Response {
    param($Response)

    $stdout.WriteLine(($Response | ConvertTo-Json -Compress -Depth 3))
}

function Resolve-ApplicationAgainstState {
    param([string]$CommandName, $Resolution)

    if (
        [string]::IsNullOrEmpty($CommandName) -or
        $Resolution -eq $null -or
        [string]::IsNullOrEmpty($Resolution.cwd) -or
        $Resolution.path -eq $null -or
        $Resolution.pathext -eq $null
    ) {
        return $null
    }

    $previousLocation = Get-Location
    $previousPath = $env:PATH
    $previousPathExt = $env:PATHEXT
    try {
        $env:PATH = [string]$Resolution.path
        $env:PATHEXT = [string]$Resolution.pathext
        Set-Location -LiteralPath ([string]$Resolution.cwd)
        if ((Get-Location).Provider.Name -ne 'FileSystem') {
            return $null
        }

        # Do not filter by CommandType while resolving: the first PowerShell-visible command must
        # itself be an application, otherwise an alias, function, cmdlet, or builtin shadows it.
        $matches = @(Microsoft.PowerShell.Core\Get-Command -Name $CommandName -All -ErrorAction Stop)
        if ($matches.Count -eq 0 -or $matches[0].CommandType -ne 'Application') {
            return $null
        }

        $applicationPath = $matches[0].Path
        if ([string]::IsNullOrEmpty($applicationPath)) {
            $applicationPath = $matches[0].Source
        }
        return $applicationPath
    } catch {
        return $null
    } finally {
        if ($previousPath -eq $null) {
            Remove-Item Env:PATH -ErrorAction SilentlyContinue
        } else {
            $env:PATH = $previousPath
        }
        if ($previousPathExt -eq $null) {
            Remove-Item Env:PATHEXT -ErrorAction SilentlyContinue
        } else {
            $env:PATHEXT = $previousPathExt
        }
        try {
            Set-Location -LiteralPath $previousLocation.Path
        } catch {
            # A failed restoration invalidates the long-lived parser host. Surface no proof and
            # let the Rust caller's next request fail closed if the provider state is unusable.
        }
    }
}

function Convert-ScalarConstantExpression {
    param($element)

    if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.ExpandableStringExpressionAst]) {
        if ($element.NestedExpressions.Count -gt 0) {
            return $null
        }
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.ConstantExpressionAst]) {
        if ($element.Value -eq $null -or $element.Value -isnot [System.ValueType]) {
            return $null
        }
        return @($element.Value.ToString())
    }

    return $null
}

function Add-LocalScalarConstant {
    param($assignment, $localConstants)

    if (
        -not ($assignment.Left -is [System.Management.Automation.Language.VariableExpressionAst]) -or
        $assignment.Left.Splatted -or
        -not $assignment.Left.VariablePath.IsUnscopedVariable
    ) {
        return $false
    }

    $name = $assignment.Left.VariablePath.UserPath
    if ([string]::IsNullOrEmpty($name) -or $name.Contains(':')) {
        return $false
    }

    $commandExpression = $assignment.Right
    if (-not ($commandExpression -is [System.Management.Automation.Language.CommandExpressionAst])) {
        return $false
    }
    if ($commandExpression.Redirections.Count -gt 0) {
        return $false
    }
    $value = Convert-ScalarConstantExpression $commandExpression.Expression
    if ($value -eq $null -or $value.Count -ne 1) {
        return $false
    }

    $localConstants[$name.ToLowerInvariant()] = $value[0]
    return $true
}

function Add-CommandOutputAssignment {
    param($assignment, $commands, $localConstants, $commandOutputVariables)

    if (
        -not ($assignment.Left -is [System.Management.Automation.Language.VariableExpressionAst]) -or
        $assignment.Left.Splatted -or
        -not $assignment.Left.VariablePath.IsUnscopedVariable
    ) {
        return $false
    }

    $name = $assignment.Left.VariablePath.UserPath
    if ([string]::IsNullOrEmpty($name) -or $name.Contains(':')) {
        return $false
    }

    if (-not (Add-CommandsFromPipelineBase $assignment.Right $commands $localConstants)) {
        return $false
    }

    $key = $name.ToLowerInvariant()
    $null = $localConstants.Remove($key)
    $commandOutputVariables[$key] = $true
    return $true
}

function Test-IsBareCommandOutputRead {
    param($statement, $commandOutputVariables)

    if (
        -not ($statement -is [System.Management.Automation.Language.PipelineAst]) -or
        $statement.PipelineElements.Count -ne 1
    ) {
        return $false
    }

    $element = $statement.PipelineElements[0]
    if (
        -not ($element -is [System.Management.Automation.Language.CommandExpressionAst]) -or
        $element.Redirections.Count -gt 0 -or
        -not ($element.Expression -is [System.Management.Automation.Language.VariableExpressionAst]) -or
        $element.Expression.Splatted -or
        -not $element.Expression.VariablePath.IsUnscopedVariable
    ) {
        return $false
    }

    $name = $element.Expression.VariablePath.UserPath
    if ([string]::IsNullOrEmpty($name) -or $name.Contains(':')) {
        return $false
    }

    return $commandOutputVariables.ContainsKey($name.ToLowerInvariant())
}

function Convert-CommandElement {
    param($element, $localConstants)

    # Accept only literal-ish command elements. Variable expansion, subexpressions, splats,
    # and other dynamic forms return $null so the whole request becomes unsupported.
    if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.ExpandableStringExpressionAst]) {
        if ($element.NestedExpressions.Count -gt 0) {
            return $null
        }
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.ConstantExpressionAst]) {
        return @($element.Value.ToString())
    }

    if ($element -is [System.Management.Automation.Language.CommandParameterAst]) {
        if ($element.Argument -eq $null) {
            return @('-' + $element.ParameterName)
        }

        if ($element.Argument -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
            return @('-' + $element.ParameterName, $element.Argument.Value)
        }

        if ($element.Argument -is [System.Management.Automation.Language.ConstantExpressionAst]) {
            return @('-' + $element.ParameterName, $element.Argument.Value.ToString())
        }

        if ($element.Argument -is [System.Management.Automation.Language.VariableExpressionAst]) {
            $value = Convert-CommandElement $element.Argument $localConstants
            if ($value -eq $null -or $value.Count -ne 1) {
                return $null
            }
            return @('-' + $element.ParameterName, $value[0])
        }

        return $null
    }

    if ($element -is [System.Management.Automation.Language.VariableExpressionAst]) {
        if ($element.Splatted -or -not $element.VariablePath.IsUnscopedVariable) {
            return $null
        }
        $name = $element.VariablePath.UserPath
        if ([string]::IsNullOrEmpty($name) -or $name.Contains(':')) {
            return $null
        }
        $key = $name.ToLowerInvariant()
        if (-not $localConstants.ContainsKey($key)) {
            return $null
        }
        return @($localConstants[$key])
    }

    return $null
}

function Convert-DirectCommandElement {
    param($element)

    if ($element -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.ExpandableStringExpressionAst]) {
        if ($element.NestedExpressions.Count -gt 0) {
            return $null
        }
        return @($element.Value)
    }

    if ($element -is [System.Management.Automation.Language.CommandParameterAst]) {
        # A parameter with an attached argument can have native tokenization semantics that are
        # not represented by two argv values (for example -Name:value). Fail closed.
        if ($element.Argument -ne $null) {
            return $null
        }
        return @('-' + $element.ParameterName)
    }

    # ConstantExpressionAst and every other expression form are intentionally excluded from the
    # direct candidate even though the broader safety parser can still inspect some of them.
    return $null
}

function Get-DirectArgvCandidate {
    param($ast)

    if ($ast.EndBlock.Statements.Count -ne 1) {
        return $null
    }

    $pipeline = $ast.EndBlock.Statements[0]
    if (-not ($pipeline -is [System.Management.Automation.Language.PipelineAst])) {
        return $null
    }
    $backgroundProperty = $pipeline.PSObject.Properties['Background']
    if ($backgroundProperty -ne $null -and $backgroundProperty.Value) {
        return $null
    }
    if ($pipeline.PipelineElements.Count -ne 1) {
        return $null
    }

    $command = $pipeline.PipelineElements[0]
    if (-not ($command -is [System.Management.Automation.Language.CommandAst])) {
        return $null
    }
    if ($command.Redirections.Count -gt 0) {
        return $null
    }
    if (
        $command.InvocationOperator -ne $null -and
        $command.InvocationOperator -ne [System.Management.Automation.Language.TokenKind]::Unknown
    ) {
        return $null
    }

    $argv = @()
    foreach ($element in $command.CommandElements) {
        $converted = Convert-DirectCommandElement $element
        if ($converted -eq $null) {
            return $null
        }
        $argv += $converted
    }
    if ($argv.Count -eq 0 -or [string]::IsNullOrEmpty($argv[0])) {
        return $null
    }
    # Prevent PowerShell's function-output unrolling from turning a one-token argv into a scalar.
    return ,$argv
}

function Convert-PipelineElement {
    param($element, $localConstants)

    if ($element -is [System.Management.Automation.Language.CommandAst]) {
        # Redirections and invocation operators make the command harder to classify safely,
        # so reject them rather than trying to normalize them.
        if ($element.Redirections.Count -gt 0) {
            return $null
        }

        if (
            $element.InvocationOperator -ne $null -and
            $element.InvocationOperator -ne [System.Management.Automation.Language.TokenKind]::Unknown
        ) {
            return $null
        }

        $parts = @()
        foreach ($commandElement in $element.CommandElements) {
            $converted = Convert-CommandElement $commandElement $localConstants
            if ($converted -eq $null) {
                return $null
            }
            $parts += $converted
        }
        return $parts
    }

    if ($element -is [System.Management.Automation.Language.CommandExpressionAst]) {
        if ($element.Redirections.Count -gt 0) {
            return $null
        }

        # Allow a parenthesized single pipeline element like "(Get-Content foo.rs -Raw)" so
        # the caller still sees the inner command words. More complex expressions stay unsupported.
        if ($element.Expression -is [System.Management.Automation.Language.ParenExpressionAst]) {
            $innerPipeline = $element.Expression.Pipeline
            if ($innerPipeline -and $innerPipeline.PipelineElements.Count -eq 1) {
                return Convert-PipelineElement $innerPipeline.PipelineElements[0] $localConstants
            }
        }

        return $null
    }

    return $null
}

function Add-CommandsFromPipelineAst {
    param($pipeline, $commands, $localConstants)

    if ($pipeline.PipelineElements.Count -eq 0) {
        return $false
    }

    foreach ($element in $pipeline.PipelineElements) {
        $words = Convert-PipelineElement $element $localConstants
        if ($words -eq $null -or $words.Count -eq 0) {
            return $false
        }
        $null = $commands.Add($words)
    }

    return $true
}

function Add-CommandsFromPipelineChain {
    param($chain, $commands, $localConstants)

    if (-not (Add-CommandsFromPipelineBase $chain.LhsPipelineChain $commands $localConstants)) {
        return $false
    }

    if (-not (Add-CommandsFromPipelineAst $chain.RhsPipeline $commands $localConstants)) {
        return $false
    }

    return $true
}

function Add-CommandsFromPipelineBase {
    param($pipeline, $commands, $localConstants)

    if ($pipeline -is [System.Management.Automation.Language.PipelineAst]) {
        return Add-CommandsFromPipelineAst $pipeline $commands $localConstants
    }

    # Windows PowerShell 5.1 does not define PipelineChainAst, so avoid a direct type
    # reference here and instead check the runtime type name.
    if ($pipeline.GetType().FullName -eq 'System.Management.Automation.Language.PipelineChainAst') {
        return Add-CommandsFromPipelineChain $pipeline $commands $localConstants
    }

    return $false
}

# This script stays alive so the Rust caller can amortize PowerShell startup across
# many parse requests. Each request and response is one compact JSON line.
while (($requestLine = $stdin.ReadLine()) -ne $null) {
    $request = $null
    try {
        $request = $requestLine | ConvertFrom-Json
    } catch {
        Write-Response @{ id = $null; status = 'parse_failed' }
        continue
    }

    # We process requests serially, but still echo the id back so the Rust side can
    # detect protocol desyncs instead of silently trusting mixed stdout.
    $requestId = $request.id
    $payload = $request.payload
    if ([string]::IsNullOrEmpty($payload)) {
        Write-Response @{ id = $requestId; status = 'parse_failed' }
        continue
    }

    try {
        $source =
            [System.Text.Encoding]::Unicode.GetString(
                [System.Convert]::FromBase64String($payload)
            )
    } catch {
        Write-Response @{ id = $requestId; status = 'parse_failed' }
        continue
    }

    Write-Response (
        Invoke-ParseRequest -RequestId $requestId -Source $source -Resolution $request.resolution
    )
}
