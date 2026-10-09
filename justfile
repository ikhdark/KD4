set working-directory := "codex-rs"
set positional-arguments
export JUST_SHELL := justfile_directory() / "scripts/just-shell.py"
set shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']
set windows-shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']

rust_min_stack := "8388608" # 8 MiB
rust_parallelism := "8" # Match codex-rs/.cargo/config.toml; standard env/CLI overrides still win.
cargo_build_jobs := env_var_or_default("CARGO_BUILD_JOBS", rust_parallelism)
export CARGO_BUILD_JOBS := cargo_build_jobs
rust_test_threads := env_var_or_default("RUST_TEST_THREADS", "4") # Match Cargo's libtest default.
export RUST_TEST_THREADS := rust_test_threads
nextest_test_threads := env_var_or_default("NEXTEST_TEST_THREADS", "8") # Match nextest.toml.
export NEXTEST_TEST_THREADS := nextest_test_threads
python := "python"
# One reserved Cargo lane shared by every named core test target and gate, so
# they never compile against a target directory another build can invalidate.
core_test_lane := "core-tests"
# Admission never queues by default; opt in with --set rust_validation_wait_seconds 5.
rust_validation_wait_seconds := "0"

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    cargo run --bin codex -- {args}

# Prefer the already-built debug binary (may be stale); fall back to `cargo run`.
[windows]
codex-fast *args:
    just codex-stale-ok {args}

codex-lane *args:
    just cargo-lane codex cargo run --bin codex -- {args}; exit $LASTEXITCODE

[windows]
codex-stale-ok *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $bin = "target\debug\codex.exe"; if (Test-Path -Path $bin -PathType Leaf) { & $bin @forwarded_args; exit $LASTEXITCODE }; cargo run --bin codex -- @forwarded_args

# `codex exec`
exec *args:
    cargo run --bin codex -- exec {args}

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- {args}

# Build the CLI and run the app-server test client in one target lane.
[windows]
app-server-test-client *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane app-server-test-client --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _app-server-test-client-reserved @forwarded_args; exit $LASTEXITCODE

[windows]
_app-server-test-client-reserved *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; cargo build --target-dir $target_dir -p codex-cli; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo run --target-dir $target_dir -p codex-app-server-test-client -- --codex-bin (Join-Path $target_dir "debug\codex.exe") @forwarded_args

# Format the justfile and Rust code; --rust-package scopes Rust to one package.
[script("python")]
fmt *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/format.py"
    sys.argv = [script, "--fast-local", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Check the high-frequency local formatter set without modifying files.
[script("python")]
fmt-check-fast *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/format.py"
    sys.argv = [script, "--check", "--fast-local", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Format the justfile, Rust, Prettier targets, Python SDK code, and Python scripts.
[script("python")]
fmt-full *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/format.py"
    sys.argv = [script, *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Check formatting without modifying files.
[script("python")]
fmt-check *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/format.py"
    sys.argv = [script, "--check", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

[no-cd]
kd4-perf-snapshot *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/kd4_perf_snapshot.py" {args}

# Compare rejected Desktop methods with this checkout's generated request schema.
[working-directory("..")]
[script("python")]
desktop-protocol-drift *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/desktop_protocol_drift.py"
    sys.argv = [script, *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Inspect committed blobs; pass --base, --head, and an explicit --allowlist.
[working-directory("..")]
[script("python")]
check-blob-size *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/check_blob_size.py"
    sys.argv = [script, *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

[no-cd]
audit-scripts *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/root_maintenance.py" audit-scripts {args}

[no-cd]
dev-env-doctor *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/dev_env_doctor.py" {args}

[no-cd]
git-doctor *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/git_doctor.py" {args}

[no-cd]
vscode-runtime-proof *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/vscode_runtime_proof.py" {args}

[windows]
fix *args:
    $forwarded_args = @($args | Select-Object -Skip 1); . "{{ justfile_directory() }}\scripts\common-rust-env.ps1"; $has_package = @(Get-CodexCargoPackageSpecs -CommandArgs $forwarded_args).Count -gt 0; $broad = $false; foreach ($arg in $forwarded_args) { if ($arg -ceq '--') { break }; if ($arg -cin @('--workspace', '--all')) { $broad = $true } }; if (-not $has_package -or $broad) { Write-Error "Pass a package selection (-p/--package) to 'just fix', or use 'just fix-workspace' for workspace scope."; exit 2 }; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --fix --tests --allow-dirty @forwarded_args; exit $LASTEXITCODE

[windows]
fix-workspace *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --fix --tests --allow-dirty @forwarded_args; exit $LASTEXITCODE

[windows]
clippy *args:
    $forwarded_args = @($args | Select-Object -Skip 1); . "{{ justfile_directory() }}\scripts\common-rust-env.ps1"; $has_package = @(Get-CodexCargoPackageSpecs -CommandArgs $forwarded_args).Count -gt 0; $broad = $false; foreach ($arg in $forwarded_args) { if ($arg -ceq '--') { break }; if ($arg -cin @('--workspace', '--all')) { $broad = $true } }; if (-not $has_package -or $broad) { Write-Error "Pass a package selection (-p/--package) to 'just clippy', or use 'just clippy-workspace' for workspace scope."; exit 2 }; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --tests @forwarded_args; exit $LASTEXITCODE

[windows]
clippy-workspace *args:
    $forwarded_args = @($args | Select-Object -Skip 1); & "{{ justfile_directory() }}\scripts\cargo-workspace-analyzer.ps1" -Analyzer clippy --workspace @forwarded_args; exit $LASTEXITCODE

[windows]
cargo-shear *args:
    @$forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo shear --version *> $null; if ($LASTEXITCODE -ne 0) { Write-Error "cargo-shear is not installed. Install with: cargo install cargo-shear"; exit 2 }; cargo shear --deny-warnings @forwarded_args

[windows]
rust-dead-code-matrix *args:
    @$forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; & "{{ justfile_directory() }}\scripts\cargo-workspace-analyzer.ps1" -Analyzer dead-code @forwarded_args; exit $LASTEXITCODE

[windows]
install:
    #!powershell.exe -File
    $requiredPwshVersion = [version]"7.5"
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        Write-Error "PowerShell $requiredPwshVersion or newer is required. Install it before running setup."
        exit 2
    }
    $actualPwshVersion = & $pwsh.Source -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
    if ([version]$actualPwshVersion -lt $requiredPwshVersion) {
        Write-Error "PowerShell $requiredPwshVersion or newer is required; found $actualPwshVersion."
        exit 2
    }
    rustup show active-toolchain
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $rustfmtToolchain = & {{ python }} "{{ justfile_directory() }}\scripts\tool_versions.py"
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    rustup toolchain install $rustfmtToolchain --profile minimal --component rustfmt
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo fetch
    exit $LASTEXITCODE

[no-cd]
[windows]
publish-local-codex-final *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -Concise -AutoSkipBuild -Profile local-release -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User -RestartDesktopIfNeeded {args}

[no-cd]
[windows]
prepare-codex-release version:
    @{{ python }} "{{ justfile_directory() }}\scripts\build_codex_package.py" --target x86_64-pc-windows-msvc --cargo-profile release --release-version "{{ version }}" --package-dir "{{ justfile_directory() }}\_build\packages\{{ version }}-x64" --release-dir "{{ justfile_directory() }}\_build\release\{{ version }}" --force
    @{{ python }} "{{ justfile_directory() }}\scripts\build_codex_package.py" --target aarch64-pc-windows-msvc --cargo-profile release --release-version "{{ version }}" --package-dir "{{ justfile_directory() }}\_build\packages\{{ version }}-arm64" --release-dir "{{ justfile_directory() }}\_build\release\{{ version }}" --force

[windows]
_sign-codex-release-preflight:
    if (-not (Get-Command cosign -ErrorAction SilentlyContinue)) { throw "cosign is required" }

[windows]
_publish-codex-release-preflight:
    if ([string]::IsNullOrWhiteSpace($env:CODEX_RELEASE_CERTIFICATE_IDENTITY) -or [string]::IsNullOrWhiteSpace($env:CODEX_RELEASE_OIDC_ISSUER)) { throw "Set CODEX_RELEASE_CERTIFICATE_IDENTITY and CODEX_RELEASE_OIDC_ISSUER to the authorized Sigstore identity" }; if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "gh is required" }

# Dot-prefixed files in a release dir are packager state (publication locks), not assets.
[no-cd]
[windows]
sign-codex-release version: _sign-codex-release-preflight (prepare-codex-release version)
    $releaseDir = "{{ justfile_directory() }}\_build\release\{{ version }}"; foreach ($asset in @(Get-ChildItem -LiteralPath $releaseDir -File | Where-Object { $_.Name -notlike '*.sigstore.json' -and $_.Name -notlike '.*' })) { cosign sign-blob --yes --bundle "$($asset.FullName).sigstore.json" $asset.FullName; if ($LASTEXITCODE -ne 0) { throw "cosign failed for $($asset.Name)" } }

[no-cd]
[windows]
publish-codex-release version: _publish-codex-release-preflight (sign-codex-release version)
    $releaseDir = "{{ justfile_directory() }}\_build\release\{{ version }}"; $tag = "rust-v{{ version }}"; foreach ($asset in @(Get-ChildItem -LiteralPath $releaseDir -File | Where-Object { $_.Name -notlike '*.sigstore.json' -and $_.Name -notlike '.*' })) { cosign verify-blob --bundle "$($asset.FullName).sigstore.json" --certificate-identity $env:CODEX_RELEASE_CERTIFICATE_IDENTITY --certificate-oidc-issuer $env:CODEX_RELEASE_OIDC_ISSUER $asset.FullName; if ($LASTEXITCODE -ne 0) { throw "Sigstore verification failed for $($asset.Name)" } }; $assets = @(Get-ChildItem -LiteralPath $releaseDir -File | Where-Object { $_.Name -notlike '.*' } | Select-Object -ExpandProperty FullName); gh release create $tag @assets --repo ikhdark/KD4 --verify-tag --title $tag; if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }; $expected = @($assets | ForEach-Object { Split-Path -Leaf $_ } | Sort-Object); $actual = @(gh release view $tag --repo ikhdark/KD4 --json assets --jq '.assets[].name' | Sort-Object); if (Compare-Object $expected $actual) { throw "Published release inventory does not match the prepared assets" }

[no-cd]
[windows]
sccache-stats:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\sccache-perf.ps1" stats

[no-cd]
[windows]
sccache-reset:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\sccache-perf.ps1" reset

[no-cd]
[windows]
sccache-restart:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\sccache-perf.ps1" restart

[windows]
rust-perf-env *args:
    $forwarded_args = @($args | Select-Object -Skip 1); & "{{ justfile_directory() }}\scripts\invoke-rust-perf-env.ps1" -CargoTargetLane "perf" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $forwarded_args; exit $LASTEXITCODE

# Run nextest with --no-fail-fast so all tests are run.
#
# Run `cargo install cargo-nextest` if you don't have it installed.
# core_test_support enables deterministic process IDs at runtime, without a
# separate crate feature. There is no need to add `--all-features`.
[windows]
test *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo nextest run --no-fail-fast @forwarded_args; exit $LASTEXITCODE

# Fast local test loop: finish the selected tests without flaky retries.
[windows]
test-fast *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo nextest run @forwarded_args; exit $LASTEXITCODE

# Named `codex-core` test targets and gates. `codex-rs/.config/kd4-rust-tests.toml`
# owns the package/target selection and the exact helper binaries each target
# needs, so the selection cannot drift and a zero-test selection always fails.
# `just core-test-list` prints the available names.
#
# Every one of these runs takes a reserved Cargo lane. Sharing `codex-rs/target`
# with another build invalidates the whole graph whenever the two disagree on a
# compiler setting, which costs far more than the tests themselves. They share
# one lane rather than taking one each, so compatible codex-core artifacts and
# dependencies stay warm between targets. Feature/profile differences can still
# require separate artifacts. A concurrent run
# reuses an idle warm sibling lane with matching build settings (such as
# `core-tests-2`); without one it reports busy immediately rather than quietly
# queuing another validation. Exit 75 leaves validation pending, not passed.
# Default wait: 0 seconds. `just --set rust_validation_wait_seconds 5 core-gate ...`
# opts into a short wait in the same invocation; never loop on busy results.
# No background resumption is scheduled. Cold overflow remains opt-in.

# Run a named core target in the shared core lane; finish the whole selection.
[script("python")]
[windows]
core-test target *args:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_build_status.py", ["run-lane", "--lane", "{{ core_test_lane }}", "--warm-wait-seconds", "{{ rust_validation_wait_seconds }}", "--", "just", "_core-test-reserved", "local", sys.argv[1], "--no-fail-fast", *sys.argv[2:]]))

# Fast local loop for a named core target: finish the selection, no retries.
[script("python")]
[windows]
core-test-fast target *args:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_build_status.py", ["run-lane", "--lane", "{{ core_test_lane }}", "--warm-wait-seconds", "{{ rust_validation_wait_seconds }}", "--", "just", "_core-test-reserved", "fast", *sys.argv[1:]]))

# Opt-in symbol-free builds; keep separate artifacts for a fair dev/dev-small comparison.
[script("python")]
[windows]
core-test-small target *args:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_build_status.py", ["run-lane", "--lane", "core-tests-small", "--warm-wait-seconds", "{{ rust_validation_wait_seconds }}", "--", "just", "_core-test-small-reserved", *sys.argv[1:]]))

[windows]
_core-test-small-reserved target *args:
    $forwarded_args = @($args | Select-Object -Skip 2); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" --target-dir $target_dir --cargo-profile dev-small run-target --profile fast "{{ target }}" @forwarded_args; exit $LASTEXITCODE

# Run a named core target in a lane of its own, apart from the shared core lane.
[script("python")]
[windows]
core-test-lane target *args:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_build_status.py", ["run-lane", "--lane", sys.argv[1], "--warm-wait-seconds", "{{ rust_validation_wait_seconds }}", "--", "just", "_core-test-reserved", "fast", *sys.argv[1:]]))

[windows]
_core-test-reserved profile target *args:
    $forwarded_args = @($args | Select-Object -Skip 3); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "{{ profile }}"; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" --target-dir $target_dir run-target "{{ target }}" @forwarded_args; exit $LASTEXITCODE

# Run gates together, sharing helper builds, overlapping tests, and one nextest
# invocation per package and helper set. Every declared step must select and
# complete exactly its declared test IDs in its own test binary.
[script("python")]
[windows]
core-gate +gates:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_build_status.py", ["run-lane", "--lane", "{{ core_test_lane }}", "--warm-wait-seconds", "{{ rust_validation_wait_seconds }}", "--", "just", "_core-gate-reserved", *sys.argv[1:]]))

[windows]
_core-gate-reserved +gates:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" --target-dir $target_dir run-gate @forwarded_args; exit $LASTEXITCODE

# Run transport and real continuation boundary regressions deliberately.
[windows]
test-slow-boundaries:
    just core-gate --profile local test-slow-boundaries; exit $LASTEXITCODE

# List the named core targets and gates.
[no-cd]
[script("python")]
core-test-list:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_test_runner.py", ["list-targets"], program=r"{{ python }}"))

# Show the resolved selection, helper builds, and commands for a target or gate.
[no-cd]
[script("python")]
core-test-plan name:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_test_runner.py", ["plan", *sys.argv[1:]], program=r"{{ python }}"))

# List the gates whose declared tests live in the modules owning changed Rust files.
[no-cd]
[script("python")]
core-test-gates-for +paths:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_test_runner.py", ["gates-for", *sys.argv[1:]], program=r"{{ python }}"))

# Validate the manifest against `cargo metadata --no-deps` without building tests.
[no-cd]
[script("python")]
core-test-manifest-check:
    import runpy, sys
    adapter = runpy.run_path(r"{{ justfile_directory() }}/scripts/just-shell.py")
    raise SystemExit(adapter["run_python"](r"{{ justfile_directory() }}/scripts/rust_test_runner.py", ["check-manifest"], program=r"{{ python }}"))

# Isolated non-incremental experiment; this also changes the target directory.
[windows]
test-fast-nosccache *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; $command_args = @("cargo", "nextest", "run") + $forwarded_args; & "{{ justfile_directory() }}\scripts\invoke-rust-perf-env.ps1" -NoSccache -CargoTargetLane "perf-nextest-nosccache" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $command_args; exit $LASTEXITCODE

# Warm the same automatic package lane used by test and test-fast.
[windows]
test-compile *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo nextest run --no-run @forwarded_args; exit $LASTEXITCODE

[windows]
test-windows-sandbox-processes *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $env:CODEX_REQUIRE_WINDOWS_SANDBOX_PROCESS_TESTS = "1"; just core-gate windows-process windows-sandbox-core-exec @forwarded_args; exit $LASTEXITCODE

cargo-fetch:
    cargo fetch

build-dev-small package:
    cargo build --profile dev-small -p {{ package }}

# The named `package` parameter is also part of the forwarded positionals, so
# each platform must skip it explicitly instead of using the plain {args}
# expansion (which would pass the package name to the binary again).
[windows]
run-dev-small package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); cargo run --profile dev-small -p {{ package }} -- @forwarded_args

local-release package:
    cargo build --profile local-release -p {{ package }}

# Run nextest in a caller-named target directory so multiple terminals can
# validate different slices without contending on the default Cargo target lock.
[windows]
test-lane lane *args:
    $forwarded_args = @($args | Select-Object -Skip 2); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ lane }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _test-lane-local-reserved @forwarded_args; exit $LASTEXITCODE

[windows]
test-lane-main *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane main --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _test-lane-local-reserved @forwarded_args; exit $LASTEXITCODE

# Fast isolated local test loop for parallel validation lanes.
[windows]
test-lane-fast lane *args:
    $forwarded_args = @($args | Select-Object -Skip 2); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ lane }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _test-lane-fast-reserved @forwarded_args; exit $LASTEXITCODE

[windows]
_test-lane-local-reserved *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --target-dir $target_dir --no-fail-fast @forwarded_args

[windows]
_test-lane-fast-reserved *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir @forwarded_args

# Emit Cargo's build-timing HTML report for the selected local test slice,
# built in the same automatic package lane as test and test-fast.
[windows]
test-timings *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo nextest run --no-fail-fast --timings @forwarded_args; exit $LASTEXITCODE

# Focused crate test without repo-wide formatting.
validate-crate-focused crate *args:
    $forwarded_args = @($args | Select-Object -Skip 2); just _validate-crate focused "{{ crate }}" @forwarded_args; exit $LASTEXITCODE

# Validation ladder: fast local formatting alongside a focused crate test.
validate-crate crate *args:
    $forwarded_args = @($args | Select-Object -Skip 2); just _validate-crate local "{{ crate }}" @forwarded_args; exit $LASTEXITCODE

# Full validation ladder for release-like source hygiene plus a focused crate test.
validate-crate-full crate *args:
    $forwarded_args = @($args | Select-Object -Skip 2); just _validate-crate full "{{ crate }}" @forwarded_args; exit $LASTEXITCODE

# Resolve the whole ladder before starting any formatter. Core uses named gates.
[script("python")]
_validate-crate mode crate *args:
    import sys
    sys.path.insert(0, r"{{ justfile_directory() }}")
    from scripts.rust_test_runner import RunnerError, run_crate_validation
    try:
        code = run_crate_validation(sys.argv[1], sys.argv[2], sys.argv[3:])
    except RunnerError as error:
        print(error, file=sys.stderr)
        raise SystemExit(2)
    raise SystemExit(code)

[windows]
cargo-lane lane *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ lane }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
cargo-lane-isolated-home lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane "{{ lane }}" -IsolateCargoHome -WarmWaitSeconds "{{ rust_validation_wait_seconds }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[working-directory("..")]
test-release-tooling:
    {{ python }} -m unittest scripts.test_build_tooling_policy scripts.test_check_blob_size scripts.test_stage_npm_packages

[no-cd]
rust-build-doctor:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" doctor

[no-cd]
target-disk:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" disk

[no-cd]
target-prune *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" prune {args}

# Preview target cache cleanup with `just target-optimize-dry-run` before pruning.
[no-cd]
target-optimize *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" optimize --keep-warm-per-base 2 --max-age-days 14 --max-lane-gib 25 {args}

[no-cd]
target-optimize-dry-run *args:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" optimize --dry-run --keep-warm-per-base 2 --max-age-days 14 --max-lane-gib 25 {args}

[no-cd]
lanes:
    @{{ python }} "{{ justfile_directory() }}/scripts/rust_build_status.py" lanes

[windows]
test-lane-package package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _test-lane-package-reserved "{{ package }}" @forwarded_args; exit $LASTEXITCODE

[windows]
_test-lane-package-reserved package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; python "{{ justfile_directory() }}\scripts\rust_test_runner.py" _guard-generic -- "-p" "{{ package }}" @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir -p "{{ package }}" @forwarded_args

[windows]
check-lane package *args:
    @$forwarded_args = @($args | Select-Object -Skip 2); python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo check -p "{{ package }}" @forwarded_args; exit $LASTEXITCODE

[windows]
clippy-lane package *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --tests -p "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
watch-lane package *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _watch-lane-reserved "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
_watch-lane-reserved package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; cargo watch -x "check --target-dir $target_dir -p {{ package }}" @forwarded_args

[windows]
coverage-lane package *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo llvm-cov -p "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

# Fix only the named package in its own lane.
[windows]
fix-lane package *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --fix --tests --allow-dirty -p "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
release-lane *args:
    @python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane release --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo build --release @($args | Select-Object -Skip 1); exit $LASTEXITCODE

# Build the default Cargo workspace members with the release profile.
build-for-release *args:
    cargo build --release {args}

# Show duplicate crate versions in the CLI build graph.
deps-duplicates *args:
    cargo tree -d -p codex-cli {args}

# Show duplicate crate versions across the Windows workspace build graph.
deps-duplicates-workspace *args:
    cargo tree -d --workspace {args}

[windows]
release-build-fast *args:
    # Standalone release compile proof only; publish-local-codex reads the profile target, not this lane artifact.
    $forwarded_args = @($args | Select-Object -Skip 1); $command_args = @("cargo", "build", "--release", "-p", "codex-cli") + $forwarded_args; & "{{ justfile_directory() }}\scripts\invoke-rust-perf-env.ps1" -CargoTargetLane "release-cli" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $command_args; exit $LASTEXITCODE

# Run the MCP server
mcp-server-run *args:
    cargo run -p codex-mcp-server -- {args}

# Regenerate the thread-config protobuf bindings through the platform-native wrapper.
[no-cd]
[windows]
generate-config-proto *args:
    $forwarded_args = @($args | Select-Object -Skip 1); powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\codex-rs\config\scripts\generate-proto.ps1" @forwarded_args; exit $LASTEXITCODE

# Verify the checked-in thread-config protobuf binding without replacing it.
[no-cd]
[windows]
generate-config-proto-check:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\codex-rs\config\scripts\generate-proto.ps1" -Check

# Regenerate or verify the checked-in exec-server relay protobuf binding.
[no-cd]
generate-exec-server-relay-proto:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-exec-server --example generate-relay-proto

[no-cd]
generate-exec-server-relay-proto-check:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-exec-server --example generate-relay-proto -- --check

# Check both checked-in protobuf bindings using their existing freshness checks.
[windows]
protos-check: generate-config-proto-check generate-exec-server-relay-proto-check

# Run focused config schema fixture validation without regenerating schemas.
config-schema-protocol-check:
    just core-gate config-schema-protocol; exit $LASTEXITCODE

# Check config schema freshness without modifying generated output.
[no-cd]
config-schema-check:
    {{ python }} "{{ justfile_directory() }}/scripts/config_schema_check.py" --mode check

# Explicitly regenerate config schema under the repository generation lock.
[no-cd]
config-schema-regenerate owner:
    {{ python }} "{{ justfile_directory() }}/scripts/config_schema_check.py" --mode force --owner "{{ owner }}"

# Run focused app-server runtime validation without regenerating schemas.
app-server-runtime-check:
    just core-gate app-server-command-exec app-server-process-exec app-server-thread-status; exit $LASTEXITCODE

tui-large-widget-check:
    just core-gate tui-large-widget; exit $LASTEXITCODE

deps-duplicates-check *args:
    {{ python }} "{{ justfile_directory() }}/scripts/check_duplicate_deps.py" {args}

# Refresh the advisory database and audit the locked dependency graph.
deps-audit:
    just deps-advisories-check
    cargo audit

# Enforce the shared advisory exceptions before either dependency-policy gate.
[working-directory("..")]
deps-advisories-check:
    {{ python }} -m unittest scripts.test_build_tooling_policy.BuildToolingPolicyTest.test_advisory_ignores_match_between_audit_and_deny

# Dependency policy gate for the dependency-cleanup surface: duplicate report
# plus the offline cargo-deny checks. Advisories need network access, so they
# stay in the separate `deps-audit` gate configured by .cargo/audit.toml.
deps-policy-check *args:
    just deps-advisories-check
    just _cargo-deny-installed
    just deps-duplicates-check {args}
    cargo deny check bans sources licenses

[windows]
_cargo-deny-installed:
    @cargo deny --version *> $null; if ($LASTEXITCODE -ne 0) { Write-Error "cargo-deny is not installed. Install with: cargo install cargo-deny"; exit 2 }

# Typecheck the TypeScript SDK, then test it against this checkout's runtime.
[windows]
sdk-ts-check:
    pnpm --dir "{{ justfile_directory() }}" --filter @openai/codex-sdk run typecheck
    python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane sdk-runtime --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _sdk-runtime-reserved pnpm --dir "{{ justfile_directory() }}" --filter @openai/codex-sdk run test; exit $LASTEXITCODE

# Lint the Python SDK, then run its suite against this checkout's runtime
# (default marker exclusions apply).
[windows]
sdk-python-check:
    uv run --directory "{{ justfile_directory() }}/sdk/python" --group dev ruff check .
    python "{{ justfile_directory() }}\scripts\rust_build_status.py" run-lane --lane sdk-runtime --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _sdk-runtime-reserved uv run --directory "{{ justfile_directory() }}/sdk/python" --group dev pytest; exit $LASTEXITCODE

# Build the runtime helper pair in the reserved lane, then run an SDK suite
# with CODEX_EXEC_PATH pointing at that build.
[windows]
_sdk-runtime-reserved *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; cargo build --target-dir $target_dir -p codex-cli -p codex-code-mode-host; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; $env:CODEX_EXEC_PATH = Join-Path $target_dir "debug\codex.exe"; & $forwarded_args[0] @($forwarded_args | Select-Object -Skip 1); exit $LASTEXITCODE

# Lint the codex-cli npm wrapper entrypoint.
[no-cd]
codex-cli-wrapper-check:
    node --check "{{ justfile_directory() }}/codex-cli/bin/codex.js"
    node --check "{{ justfile_directory() }}/codex-rs/responses-api-proxy/npm/bin/codex-responses-api-proxy.js"

app-server-command-exec-check:
    just core-gate app-server-command-exec; exit $LASTEXITCODE

app-server-process-exec-check:
    just core-gate app-server-process-exec; exit $LASTEXITCODE

app-server-thread-status-check:
    just core-gate app-server-thread-status; exit $LASTEXITCODE

app-server-schema-protocol-check:
    just core-gate app-server-schema-fixtures; exit $LASTEXITCODE

# Check app-server schema fixtures without modifying generated output.
# Forwards checker flags, notably `--allow-stable-break <issue>` for a reviewed
# stable API break. Stable checks require `--compatibility-baseline <rev>` or
# CODEX_SCHEMA_COMPATIBILITY_BASELINE; choose the contract revision explicitly.
[no-cd]
app-server-schema-check *args:
    {{ python }} "{{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py" --mode check {args}

# Explicitly regenerate app-server schemas under their generation lock.
# Stable regeneration requires CODEX_SCHEMA_COMPATIBILITY_BASELINE.
# --experimental exports to dist/app-server-schema-experimental, not stable fixtures.
[no-cd]
app-server-schema-regenerate owner experimental="":
    {{ python }} "{{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py" --mode force --owner "{{ owner }}" -- {{ if experimental == "--experimental" { "--experimental" } else if experimental == "" { "" } else { error("app-server-schema-regenerate only accepts --experimental") } }}

# Regenerate hook schema artifacts through the Rust workspace from any cwd.
[no-cd]
write-hooks-schema:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-hooks --bin write_hooks_schema_fixtures

# Compare generated hook schemas in a temporary directory with checked-in fixtures.
hooks-schema-check:
    just cargo-lane core-tests cargo nextest run --profile local --no-tests=fail -p codex-hooks --lib -E 'test(=schema::tests::generated_hook_schemas_match_fixtures)'; exit $LASTEXITCODE

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
[windows]
argument-comment-lint *args:
    $forwarded_args = {args}; {{ python }} "{{ justfile_directory() }}/tools/argument-comment-lint/run.py" @forwarded_args

# Tail logs from the state SQLite database
[windows]
log *args:
    $forwarded_args = {args}; if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo run -p codex-state --bin logs_client -- @forwarded_args
