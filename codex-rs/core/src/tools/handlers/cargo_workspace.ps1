$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
$version = & $cargo --version
$code = $LASTEXITCODE
if ($code -ne 0) { exit $code }
$location = & $cargo locate-project --message-format=json @manifestArgs
$code = $LASTEXITCODE
if ($code -ne 0) { exit $code }
$manifest = ($location -join "`n" | ConvertFrom-Json -ErrorAction Stop).root
if ([string]::IsNullOrWhiteSpace($manifest)) { throw 'Cargo locate-project returned no manifest' }
@{
    kind = 'cargo_workspace_provenance'
    cargo_executable = $cargo
    cargo_version = $version -join "`n"
    manifest_path = $manifest
    no_deps = $true
    offline_requested = $metadataArgs -contains '--offline'
    locked_requested = $metadataArgs -contains '--locked'
} | ConvertTo-Json -Compress
& $cargo metadata @metadataArgs "--manifest-path=$manifest"
$code = $LASTEXITCODE
exit $code
