set working-directory := "codex-rs"
set positional-arguments
export JUST_SHELL := justfile_directory() / "scripts/build/just-shell.py"
set shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']
set windows-shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']

rust_parallelism := "8" # Match codex-rs/.cargo/config.toml; standard env/CLI overrides still win.
cargo_build_jobs := env_var_or_default("CARGO_BUILD_JOBS", rust_parallelism)
export CARGO_BUILD_JOBS := cargo_build_jobs
python := "python"
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
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane app-server-test-client --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _app-server-test-client-reserved @forwarded_args; exit $LASTEXITCODE

[windows]
_app-server-test-client-reserved *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; cargo build --target-dir $target_dir -p codex-cli; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo run --target-dir $target_dir -p codex-app-server-test-client -- --codex-bin (Join-Path $target_dir "debug\codex.exe") @forwarded_args

# Format the justfile and Rust code; --rust-package scopes Rust to one package.
[script("python")]
fmt *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/maintenance/format.py"
    sys.argv = [script, "--fast-local", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Check the high-frequency local formatter set without modifying files.
[script("python")]
fmt-check-fast *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/maintenance/format.py"
    sys.argv = [script, "--check", "--fast-local", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Format the justfile, Rust, Prettier targets, Python SDK code, and Python scripts.
[script("python")]
fmt-full *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/maintenance/format.py"
    sys.argv = [script, *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")

# Check formatting without modifying files.
[script("python")]
fmt-check *args:
    import runpy
    import sys
    script = r"{{ justfile_directory() }}/scripts/maintenance/format.py"
    sys.argv = [script, "--check", *sys.argv[1:]]
    runpy.run_path(script, run_name="__main__")
[windows]
fix *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --keep-going --fix --allow-dirty @forwarded_args; exit $LASTEXITCODE

[windows]
fix-workspace *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --keep-going --fix --allow-dirty @forwarded_args; exit $LASTEXITCODE

# Keep checking independent crates after an error, without hiding diagnostics.
[windows]
clippy *args:
    $forwarded_args = @($args | Select-Object -Skip 1); python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane auto --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --keep-going @forwarded_args; exit $LASTEXITCODE
[windows]
cargo-shear *args:
    @$forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo shear --version *> $null; if ($LASTEXITCODE -ne 0) { Write-Error "cargo-shear is not installed. Install with: cargo install cargo-shear"; exit 2 }; cargo shear --deny-warnings @forwarded_args

# Run both lint passes even if Clippy fails, then summarize their exit codes.
# Extra arguments select the Clippy scope; cargo-shear scans the workspace.
[windows]
[script("python")]
lint-rust *clippy_args:
    import subprocess
    import sys
    clippy_code = subprocess.run(
        ["just", "--set", "rust_validation_wait_seconds", "{{ rust_validation_wait_seconds }}", "clippy", *sys.argv[1:]],
        check=False,
    ).returncode
    shear_code = subprocess.run(["just", "cargo-shear"], check=False).returncode
    print(f"\nRust lint summary: Clippy exit {clippy_code}; cargo-shear exit {shear_code}.", flush=True)
    sys.exit(clippy_code or shear_code)

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
    $rustfmtToolchain = & {{ python }} "{{ justfile_directory() }}\scripts\common\tool_versions.py"
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    rustup toolchain install $rustfmtToolchain --profile minimal --component rustfmt
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo fetch
    exit $LASTEXITCODE

[no-cd]
[windows]
publish-local-codex-final *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\install\publish-local-codex.ps1" -Concise -AutoSkipBuild -Profile release -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User -RestartDesktopIfNeeded {args}

[no-cd]
[windows]
publish-local-codex-pgo *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\install\publish-local-codex-pgo.ps1" {args}

[no-cd]
[windows]
prepare-codex-release version:
    @{{ python }} "{{ justfile_directory() }}\scripts\codex_package\build_codex_package.py" --target x86_64-pc-windows-msvc --cargo-profile release --release-version "{{ version }}" --package-dir "{{ justfile_directory() }}\_build\packages\{{ version }}-x64" --release-dir "{{ justfile_directory() }}\_build\release\{{ version }}" --force
    @{{ python }} "{{ justfile_directory() }}\scripts\codex_package\build_codex_package.py" --target aarch64-pc-windows-msvc --cargo-profile release --release-version "{{ version }}" --package-dir "{{ justfile_directory() }}\_build\packages\{{ version }}-arm64" --release-dir "{{ justfile_directory() }}\_build\release\{{ version }}" --force

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
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\build\sccache-perf.ps1" stats

[no-cd]
[windows]
sccache-reset:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\build\sccache-perf.ps1" reset

[no-cd]
[windows]
sccache-restart:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\build\sccache-perf.ps1" restart

[windows]
rust-perf-env *args:
    $forwarded_args = @($args | Select-Object -Skip 1); & "{{ justfile_directory() }}\scripts\build\invoke-rust-perf-env.ps1" -CargoTargetLane "perf" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $forwarded_args; exit $LASTEXITCODE

cargo-fetch:
    cargo fetch

release package:
    cargo build --profile release -p {{ package }}

[windows]
cargo-lane lane *args:
    @python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane "{{ lane }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
cargo-lane-isolated-home lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\build\cargo-lane.ps1" -Lane "{{ lane }}" -IsolateCargoHome -WarmWaitSeconds "{{ rust_validation_wait_seconds }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE
[no-cd]
target-disk:
    @{python-recipe}{{ python }}

[no-cd]
target-prune *args:
    @{python-recipe}{{ python }}

# Preview target cache cleanup with `just target-optimize-dry-run` before pruning.
[no-cd]
target-optimize *args:
    @{python-recipe}{{ python }}

[no-cd]
target-optimize-dry-run *args:
    @{python-recipe}{{ python }}

[no-cd]
lanes:
    @{python-recipe}{{ python }}

[windows]
check-lane package *args:
    @$forwarded_args = @($args | Select-Object -Skip 2); python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo check -p "{{ package }}" @forwarded_args; exit $LASTEXITCODE

[windows]
clippy-lane package *args:
    @python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --keep-going -p "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
watch-lane package *args:
    @python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- just _watch-lane-reserved "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
_watch-lane-reserved package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); $target_dir = $env:CODEX_CARGO_LANE_TARGET_DIR; Remove-Item Env:CODEX_CARGO_LANE_TARGET_DIR -ErrorAction SilentlyContinue; if ([string]::IsNullOrWhiteSpace($target_dir)) { throw "missing Cargo lane reservation" }; cargo watch -x "check --target-dir $target_dir -p {{ package }}" @forwarded_args

# Fix only the named package in its own lane.
[windows]
fix-lane package *args:
    @python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane "{{ package }}" --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo clippy --keep-going --fix --allow-dirty -p "{{ package }}" @($args | Select-Object -Skip 2); exit $LASTEXITCODE

[windows]
release-lane *args:
    @python "{{ justfile_directory() }}\scripts\build\rust_build_status.py" run-lane --lane release --warm-wait-seconds "{{ rust_validation_wait_seconds }}" -- cargo build --release @($args | Select-Object -Skip 1); exit $LASTEXITCODE

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
    $forwarded_args = @($args | Select-Object -Skip 1); $command_args = @("cargo", "build", "--release", "-p", "codex-cli") + $forwarded_args; & "{{ justfile_directory() }}\scripts\build\invoke-rust-perf-env.ps1" -CargoTargetLane "release-cli" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $command_args; exit $LASTEXITCODE

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
generate-exec-server-relay-proto:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-exec-server --example generate-relay-proto

generate-exec-server-relay-proto-check:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-exec-server --example generate-relay-proto -- --check

# Check both checked-in protobuf bindings using their existing freshness checks.
[windows]
protos-check: generate-config-proto-check generate-exec-server-relay-proto-check
# Typecheck the TypeScript SDK.
[windows]
sdk-ts-check:
    pnpm --dir "{{ justfile_directory() }}" --filter @openai/codex-sdk run typecheck

# Lint the Python SDK.
[windows]
sdk-python-check:
    $env:UV_PROJECT_ENVIRONMENT = "{{ justfile_directory() }}/.cache/sdk-python-check"; uv sync --directory "{{ justfile_directory() }}/sdk/python" --group dev --no-install-package openai-codex-cli-bin; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; uv run --directory "{{ justfile_directory() }}/sdk/python" --no-sync ruff check .; exit $LASTEXITCODE

# Lint the codex-cli npm wrapper entrypoint.
[no-cd]
codex-cli-wrapper-check:
    node --check "{{ justfile_directory() }}/codex-cli/bin/codex.js"
# Regenerate hook schema artifacts through the Rust workspace from any cwd.
write-hooks-schema:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
[windows]
argument-comment-lint *args:
    $forwarded_args = {args}; {{ python }} "{{ justfile_directory() }}/tools/argument-comment-lint/run.py" @forwarded_args

# Tail logs from the state SQLite database
[windows]
log *args:
    $forwarded_args = {args}; if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo run -p codex-state --bin logs_client -- @forwarded_args
