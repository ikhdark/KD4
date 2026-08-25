Set-StrictMode -Version Latest

$script:CargoLanePatterns = $null

function Get-CargoLanePatterns {
    if ($null -ne $script:CargoLanePatterns) {
        return $script:CargoLanePatterns
    }

    $patternPath = Join-Path $PSScriptRoot "cargo_lane_patterns.json"
    try {
        $patterns = Get-Content -LiteralPath $patternPath -Raw -Encoding UTF8 | ConvertFrom-Json
    }
    catch {
        throw "Could not load Cargo lane patterns from '$patternPath': $($_.Exception.Message)"
    }

    foreach ($name in @("lane_path_pattern", "script_lane_pattern", "just_lane_pattern", "just_fixed_lane_pattern")) {
        if ([string]::IsNullOrWhiteSpace([string]$patterns.$name)) {
            throw "Cargo lane pattern registry '$patternPath' is missing '$name'."
        }
    }
    if ($null -eq $patterns.just_fixed_lane_names) {
        throw "Cargo lane pattern registry '$patternPath' is missing 'just_fixed_lane_names'."
    }

    $script:CargoLanePatterns = $patterns
    return $script:CargoLanePatterns
}

function Get-CargoLaneNamesFromCommandLines {
    param(
        [AllowEmptyCollection()]
        [string[]]$CommandLines
    )

    $patterns = Get-CargoLanePatterns
    $fixedLaneNames = @{}
    foreach ($property in $patterns.just_fixed_lane_names.PSObject.Properties) {
        $fixedLaneNames[$property.Name] = [string]$property.Value
    }

    $names = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
    foreach ($line in $CommandLines) {
        foreach ($match in [regex]::Matches($line, [string]$patterns.lane_path_pattern)) {
            [void]$names.Add($match.Groups[1].Value)
        }
        foreach ($match in [regex]::Matches($line, [string]$patterns.just_lane_pattern)) {
            [void]$names.Add($match.Groups[1].Value)
        }
        foreach ($match in [regex]::Matches($line, [string]$patterns.just_fixed_lane_pattern)) {
            [void]$names.Add($fixedLaneNames[$match.Groups[1].Value])
        }
        foreach ($match in [regex]::Matches($line, [string]$patterns.script_lane_pattern)) {
            [void]$names.Add($match.Groups[1].Value)
        }
    }

    return @($names)
}
