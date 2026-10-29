# Profile-guided local publishing. Prepare/Train/Optimize never install or restart.
[CmdletBinding()]
param(
    [ValidateSet("Prepare", "Train", "Optimize", "Publish")]
    [string]$Stage = "Prepare",
    [string]$WorkDir,
    [string]$TrainingScript,
    [string]$RustyV8Archive,
    [switch]$AllowRustyV8Download,
    [string]$InstallDir = $env:CODEX_LOCAL_PUBLISH_DIR,
    [string]$LocalCodexHome = $env:CODEX_LOCAL_CODEX_HOME,
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$script:PgoPublisher = Join-Path $PSScriptRoot "publish-local-codex.ps1"
$script:PgoRepoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
$script:PgoArchive = $RustyV8Archive

function Invoke-PgoOwner {
    param([hashtable]$Options = @{}, [scriptblock]$Action)

    # Import into this function's scope: publisher parameters must not overwrite
    # the wrapper's Stage, paths, or switches, and its entrypoint must never run.
    . $script:PgoPublisher -ImportOnly -NoSccache -RepoRoot $script:PgoRepoRoot `
        -RustyV8Archive $script:PgoArchive @Options
    & $Action
}

function Get-PgoFileHash {
    param([string]$Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
}

function Get-PgoSourceFingerprint {
    $snapshot = Invoke-PgoOwner -Action { Get-LocalPublishBuildInputSnapshot -RepoRoot $RepoRoot }
    if ($null -eq $snapshot) { throw "Could not fingerprint the PGO source/toolchain inputs." }
    return $snapshot.Fingerprint
}

function Assert-PgoSource {
    param([object]$State)
    if ($State.sourceFingerprint -cne (Get-PgoSourceFingerprint)) {
        throw "PGO source, toolchain, or build settings changed. Rerun Prepare and Train."
    }
}

function Get-PgoBuildProof {
    param([hashtable]$Options)
    return Invoke-PgoOwner -Options $Options -Action {
        $paths = @(
            (Get-BuiltCodexPath -RepoRoot $RepoRoot -Profile $Profile),
            (Get-BuiltCodeModeHostPath -RepoRoot $RepoRoot -Profile $Profile),
            (Get-BuiltWindowsSandboxSetupPath -RepoRoot $RepoRoot -Profile $Profile),
            (Get-BuiltCommandRunnerPath -RepoRoot $RepoRoot -Profile $Profile)
        )
        $stampPath = Get-BuildStampPath -RepoRoot $RepoRoot -Profile $Profile
        $stamp = Read-BuildStamp -StampPath $stampPath
        $decision = Get-AutoSkipBuildDecision -RepoRoot $RepoRoot -StampPath $stampPath `
            -Profile $Profile -SourceExe $paths[0] -SourceCodeModeHostExe $paths[1] `
            -SourceWindowsSandboxSetupExe $paths[2] -SourceCommandRunnerExe $paths[3]
        if (-not $decision.CanSkip) { throw "PGO build is not current: $($decision.Reason). Rerun its build stage." }
        if ($null -eq $stamp -or $stamp.SourceFingerprint -cne $decision.SourceSnapshot.Fingerprint) {
            throw "PGO build stamp changed while collecting proof. Rerun its build stage."
        }
        $names = @("codex.exe", "codex-code-mode-host.exe", "codex-resources/codex-windows-sandbox-setup.exe", "codex-resources/codex-command-runner.exe")
        $hashes = @($stamp.CodexSha256, $stamp.CodeModeHostSha256, $stamp.WindowsSandboxSetupSha256, $stamp.CommandRunnerSha256)
        $entries = for ($index = 0; $index -lt $paths.Count; $index++) {
            [pscustomobject]@{ relativePath = $names[$index]; sourcePath = $paths[$index]; sha256 = $hashes[$index] }
        }
        [pscustomobject]@{ fingerprint = $decision.SourceSnapshot.Fingerprint; entries = @($entries) }
    }
}

function Invoke-PgoNative {
    param([string]$Program, [string[]]$CommandArgs, [string]$Description)
    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $Program @CommandArgs
        $code = $LASTEXITCODE
    }
    finally { $ErrorActionPreference = $previousPreference }
    if ($code -ne 0) { throw "$Description failed with exit code $code." }
}

function Invoke-PgoPublisher {
    param([hashtable]$Options, [switch]$Publish)
    $arguments = @("-NoProfile", "-ExecutionPolicy", "Bypass", "-File", $script:PgoPublisher,
        "-Concise", "-NoSccache", "-RepoRoot", $script:PgoRepoRoot)
    foreach ($name in @($Options.Keys | Sort-Object)) { $arguments += @("-$name", [string]$Options[$name]) }
    if ($script:PgoArchive) { $arguments += @("-RustyV8Archive", $script:PgoArchive) }
    if ($AllowRustyV8Download) { $arguments += "-AllowRustyV8Download" }
    if ($Publish) {
        # Publish only the already verified optimized bundle; never silently
        # rebuild with old training data or bypass the existing live-turn checks.
        $arguments += @("-SkipBuild", "-ConfigureDesktopLocalCli", "-RestartDesktopIfNeeded",
            "-CloseRunningTargetTimeoutSeconds", "30", "-LocalCodexHome", $LocalCodexHome)
        if ($InstallDir) { $arguments += @("-InstallDir", $InstallDir) }
        if ($Force) { $arguments += "-Force" }
    }
    else { $arguments += @("-BuildOnly", "-AutoSkipBuild") }
    Invoke-PgoNative -Program "powershell.exe" -CommandArgs $arguments -Description "PGO publisher"
}

function Resolve-PgoProfdataTool {
    return Invoke-PgoOwner -Action {
        Push-Location -LiteralPath (Join-Path $RepoRoot "codex-rs")
        try {
            $target = Get-LocalPublishBuildTarget -RepoRoot $RepoRoot -Profile "pgo"
            $sysroot = @(& rustc --print sysroot)
            if ($LASTEXITCODE -ne 0 -or $sysroot.Count -ne 1) { throw "Could not resolve the pinned Rust sysroot." }
        }
        finally { Pop-Location }
        $tool = Join-Path ([string]$sysroot[0]) "lib/rustlib/$target/bin/llvm-profdata.exe"
        if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) {
            throw "Matching llvm-profdata is missing. From codex-rs run: rustup component add llvm-tools-preview"
        }
        $tool
    }
}

function Assert-PgoChildPath {
    param([string]$Parent, [string]$Path)
    $prefix = [IO.Path]::GetFullPath($Parent).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not [IO.Path]::GetFullPath($Path).StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "PGO state references a path outside its owned directory: $Path"
    }
}

function Write-PgoState {
    param([object]$State, [string]$Path)
    $temporary = "$Path.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($temporary, ($State | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path) {
            [IO.File]::Replace($temporary, $Path, [System.Management.Automation.Language.NullString]::Value)
        }
        else { [IO.File]::Move($temporary, $Path) }
    }
    finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary } }
}

function Read-PgoState {
    param([string]$Path, [string]$Root)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "PGO state is missing. Run Prepare first." }
    $state = [IO.File]::ReadAllText($Path) | ConvertFrom-Json -ErrorAction Stop
    if ($state.schemaVersion -ne 1 -or -not [string]::Equals($state.repoRoot, $script:PgoRepoRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw "PGO state belongs to another checkout or schema. Use a new WorkDir."
    }
    Assert-PgoChildPath -Parent (Join-Path $Root "runs") -Path $state.runDirectory
    Assert-PgoChildPath -Parent $state.runDirectory -Path $state.bundleDirectory
    if ($state.rawDirectory) { Assert-PgoChildPath -Parent $state.runDirectory -Path $state.rawDirectory }
    if ($state.profileData) { Assert-PgoChildPath -Parent $state.runDirectory -Path $state.profileData }
    return $state
}

function Assert-PgoTrainingBundle {
    param([object]$State)
    $expected = @("codex.exe", "codex-code-mode-host.exe", "codex-resources/codex-windows-sandbox-setup.exe", "codex-resources/codex-command-runner.exe")
    if (-not $State.prepared -or @($State.entries).Count -ne $expected.Count) { throw "PGO preparation is incomplete. Rerun Prepare." }
    foreach ($name in $expected) {
        $entry = @($State.entries | Where-Object { $_.relativePath -ceq $name })
        if ($entry.Count -ne 1 -or $entry[0].sha256 -cne (Get-PgoFileHash (Join-Path $State.bundleDirectory $name))) {
            throw "Instrumented training bundle changed. Rerun Prepare."
        }
    }
}

function Assert-PgoTrainingStopped {
    param([object]$State)
    Invoke-PgoOwner -Action {
        $script:RunningTargetProcessProbeError = $null
        $script:RunningTargetProcessProbeWarnings = [Collections.Generic.List[string]]::new()
        foreach ($entry in $State.entries) {
            if (@(Get-RunningCodexTargetProcesses -TargetPath (Join-Path $State.bundleDirectory $entry.relativePath)).Count -gt 0) {
                throw "An instrumented training process is still running. Wait for all training children to exit."
            }
        }
        if ($script:RunningTargetProcessProbeError -or $script:RunningTargetProcessProbeWarnings.Count -gt 0) {
            throw "Could not verify that instrumented training processes have exited."
        }
    }
}

function Get-PgoRawSnapshot {
    param([string]$Directory)
    $files = @(Get-ChildItem -LiteralPath $Directory -Recurse -File -Filter "*.profraw" | Sort-Object FullName)
    if ($files.Count -eq 0) { throw "Training produced no .profraw files. Ensure it runs the instrumented fork and exits cleanly." }
    $records = foreach ($file in $files) {
        if ($file.Length -eq 0) { throw "Training produced an empty profile: $($file.FullName)" }
        "$($file.FullName):$($file.Length):$(Get-PgoFileHash $file.FullName)"
    }
    $fingerprint = Invoke-PgoOwner -Action { Get-TextSha256 -Value ($records -join "`n") }
    return [pscustomobject]@{ files = $files; fingerprint = $fingerprint }
}

if ($TrainingScript -and $Stage -ne "Train") { throw "-TrainingScript is only valid for Train." }
if ($Force -and $Stage -ne "Publish") { throw "-Force is only valid for Publish." }
if ($Stage -eq "Train") {
    if ([string]::IsNullOrWhiteSpace($TrainingScript) -or -not (Test-Path -LiteralPath $TrainingScript -PathType Leaf)) {
        throw "Train requires an existing -TrainingScript (.ps1) containing representative workloads."
    }
    $TrainingScript = (Resolve-Path -LiteralPath $TrainingScript).ProviderPath
    if ([IO.Path]::GetExtension($TrainingScript) -ine ".ps1") { throw "TrainingScript must be a PowerShell .ps1 file." }
}
if (-not $WorkDir) { $WorkDir = Join-Path $script:PgoRepoRoot "codex-rs/target/pgo" }
$WorkDir = [IO.Path]::GetFullPath($WorkDir)
$repoPrefix = $script:PgoRepoRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
if ($WorkDir -ieq $script:PgoRepoRoot -or $WorkDir.StartsWith($repoPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    Assert-PgoChildPath -Parent (Join-Path $script:PgoRepoRoot "codex-rs/target") -Path $WorkDir
}
if (-not $LocalCodexHome) { $LocalCodexHome = Invoke-PgoOwner -Action { Get-DefaultLocalCodexHome } }
$LocalCodexHome = [IO.Path]::GetFullPath($LocalCodexHome)
# Refuse flag overrides before doing expensive builds or making state directories.
foreach ($item in Get-ChildItem Env:) {
    if ($item.Name -like "CARGO_PROFILE_*" -or $item.Name -in @("CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS")) {
        throw "Unset $($item.Name) before running the PGO workflow."
    }
}
$tool = Resolve-PgoProfdataTool
[void][IO.Directory]::CreateDirectory($WorkDir)
$lock = $null
try {
    try { $lock = [IO.File]::Open((Join-Path $WorkDir "workflow.lock"), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
    catch { throw "Another PGO stage owns this WorkDir, or its lock is unavailable: $WorkDir" }
    $statePath = Join-Path $WorkDir "state.json"
    if ($Stage -eq "Prepare") {
        if (Test-Path -LiteralPath $statePath) { [void](Read-PgoState -Path $statePath -Root $WorkDir) }
        elseif (@(Get-ChildItem -LiteralPath $WorkDir -Force | Where-Object { $_.Name -ne "workflow.lock" }).Count -gt 0) {
            throw "WorkDir contains unowned files. Choose a new empty WorkDir. Nothing was removed."
        }
        $run = Join-Path $WorkDir "runs/$([Guid]::NewGuid().ToString('N'))"
        $generation = Join-Path $run "raw"
        $bundle = Join-Path $run "bundle"
        [void][IO.Directory]::CreateDirectory($generation)
        [void][IO.Directory]::CreateDirectory($bundle)
        $state = [pscustomobject][ordered]@{
            schemaVersion = 1; repoRoot = $script:PgoRepoRoot; sourceFingerprint = Get-PgoSourceFingerprint
            runDirectory = $run; bundleDirectory = $bundle; prepared = $false; entries = @()
            trained = $false; rawDirectory = $null; trainingScriptSha256 = $null; trainingProfilesFingerprint = $null
            profileData = $null; profileDataSha256 = $null; optimizedBuild = $null
        }
        Write-PgoState -State $state -Path $statePath
        $options = @{ Profile = "pgo"; PgoGenerateDirectory = $generation }
        Invoke-PgoPublisher -Options $options
        $proof = Get-PgoBuildProof -Options $options
        foreach ($entry in $proof.entries) {
            $destination = Join-Path $bundle $entry.relativePath
            [void][IO.Directory]::CreateDirectory((Split-Path -Parent $destination))
            Copy-Item -LiteralPath $entry.sourcePath -Destination $destination
            if ($entry.sha256 -cne (Get-PgoFileHash $destination)) { throw "Training bundle changed while copying. Rerun Prepare." }
        }
        Assert-PgoSource $state
        $state.entries = @($proof.entries)
        $state.prepared = $true
        Write-PgoState -State $state -Path $statePath
        Write-Output "PGO instrumented bundle ready. Next: just publish-local-codex-pgo -Stage Train -WorkDir `"$WorkDir`" -TrainingScript <workloads.ps1>"
        return
    }

    $state = Read-PgoState -Path $statePath -Root $WorkDir
    Assert-PgoSource $state
    Assert-PgoTrainingBundle $state
    Assert-PgoTrainingStopped $state
    if ($Stage -eq "Train") {
        $state.trained = $false
        $state.profileData = $null
        $state.profileDataSha256 = $null
        $state.optimizedBuild = $null
        $state.rawDirectory = Join-Path $state.runDirectory "raw/$([Guid]::NewGuid().ToString('N'))"
        [void][IO.Directory]::CreateDirectory($state.rawDirectory)
        $state.trainingScriptSha256 = Get-PgoFileHash $TrainingScript
        Write-PgoState -State $state -Path $statePath
        $environment = @{
            LLVM_PROFILE_FILE = Join-Path $state.rawDirectory "%m-%p.profraw"
            CODEX_CLI_PATH = Join-Path $state.bundleDirectory "codex.exe"
            CODEX_CODE_MODE_HOST_PATH = Join-Path $state.bundleDirectory "codex-code-mode-host.exe"
            CODEX_HOME = $LocalCodexHome
            CODEX_SQLITE_HOME = Join-Path $LocalCodexHome "sqlite"
        }
        $previous = @{}
        try {
            foreach ($name in $environment.Keys) {
                $previous[$name] = [Environment]::GetEnvironmentVariable($name, "Process")
                [Environment]::SetEnvironmentVariable($name, $environment[$name], "Process")
            }
            Invoke-PgoNative -Program "powershell.exe" -CommandArgs @("-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", $TrainingScript) -Description "PGO training script"
        }
        finally { foreach ($name in $previous.Keys) { [Environment]::SetEnvironmentVariable($name, $previous[$name], "Process") } }
        Assert-PgoTrainingStopped $state
        Assert-PgoSource $state
        Assert-PgoTrainingBundle $state
        if ($state.trainingScriptSha256 -cne (Get-PgoFileHash $TrainingScript)) { throw "TrainingScript changed during training. Rerun Train." }
        $state.trainingProfilesFingerprint = (Get-PgoRawSnapshot $state.rawDirectory).fingerprint
        $state.trained = $true
        Write-PgoState -State $state -Path $statePath
        Write-Output "PGO training complete. Next: just publish-local-codex-pgo -Stage Optimize -WorkDir `"$WorkDir`""
        return
    }
    if (-not $state.trained) { throw "No successful training run is recorded. Run Train first." }
    $raw = Get-PgoRawSnapshot $state.rawDirectory
    if ($raw.fingerprint -cne $state.trainingProfilesFingerprint) { throw "Training profiles changed after training completed. Rerun Train." }
    if ($Stage -eq "Optimize") {
        $state.profileData = $null
        $state.profileDataSha256 = $null
        $state.optimizedBuild = $null
        Write-PgoState -State $state -Path $statePath
        $inputList = Join-Path $state.runDirectory "merge-inputs.txt"
        # llvm-profdata's input list avoids the Windows native argument-length limit.
        $lines = @($raw.files | ForEach-Object { "1,$($_.FullName)" })
        [IO.File]::WriteAllLines($inputList, [string[]]$lines, [Text.UTF8Encoding]::new($false))
        $temporaryProfile = Join-Path $state.runDirectory "merged-$([Guid]::NewGuid().ToString('N')).profdata"
        Invoke-PgoNative -Program $tool -CommandArgs @("merge", "--sparse", "--failure-mode=any", "--input-files=$inputList", "-o", $temporaryProfile) -Description "PGO profile merge"
        Assert-PgoTrainingStopped $state
        Assert-PgoSource $state
        if ((Get-PgoRawSnapshot $state.rawDirectory).fingerprint -cne $raw.fingerprint) { throw "Training profiles changed during merge. Rerun Train." }
        $hash = Get-PgoFileHash $temporaryProfile
        $profilePath = Join-Path $state.runDirectory "profile-$hash.profdata"
        if (Test-Path -LiteralPath $profilePath) {
            if ((Get-PgoFileHash $profilePath) -cne $hash) { throw "An existing content-addressed profile was modified. Use a new WorkDir." }
            Remove-Item -LiteralPath $temporaryProfile
        }
        else { [IO.File]::Move($temporaryProfile, $profilePath) }
        $options = @{ Profile = "pgo"; PgoProfileData = $profilePath }
        Invoke-PgoPublisher -Options $options
        $state.optimizedBuild = Get-PgoBuildProof -Options $options
        Assert-PgoSource $state
        $state.profileData = $profilePath
        $state.profileDataSha256 = $hash
        Write-PgoState -State $state -Path $statePath
        Write-Output "PGO optimized bundle ready; no speedup is assumed. Compare held-out workloads before publishing."
        Write-Output "Next, outside Codex when chats are idle: just publish-local-codex-pgo -Stage Publish -WorkDir `"$WorkDir`""
        return
    }
    if (-not $state.profileData -or $null -eq $state.optimizedBuild -or (Get-PgoFileHash $state.profileData) -cne $state.profileDataSha256) {
        throw "The optimized PGO build/profile is missing or changed. Run Optimize first."
    }
    $options = @{ Profile = "pgo"; PgoProfileData = $state.profileData }
    $proof = Get-PgoBuildProof -Options $options
    if ($proof.fingerprint -cne $state.optimizedBuild.fingerprint) { throw "Optimized build identity changed. Rerun Optimize." }
    foreach ($entry in $proof.entries) {
        $recorded = @($state.optimizedBuild.entries | Where-Object { $_.relativePath -ceq $entry.relativePath })
        if ($recorded.Count -ne 1 -or $recorded[0].sha256 -cne $entry.sha256) {
            throw "Optimized bundle bytes changed after Optimize. Rerun Optimize."
        }
    }
    Invoke-PgoPublisher -Options $options -Publish
}
finally { if ($null -ne $lock) { $lock.Dispose() } }
