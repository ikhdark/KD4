set working-directory := "codex-rs"
set positional-arguments
export JUST_SHELL := justfile_directory() / "scripts/just-shell.py"
set shell := ["python3", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']
set windows-shell := ["python", "-c", 'import os, runpy; runpy.run_path(os.environ["JUST_SHELL"], run_name="__main__")']

rust_min_stack := "8388608" # 8 MiB
cargo_build_jobs := env_var_or_default("CARGO_BUILD_JOBS", "-2") # Leave two logical CPUs free on any host.
export CARGO_BUILD_JOBS := cargo_build_jobs
python := if os_family() == "windows" { "python" } else { "python3" }

# Display help
help:
    just -l

# `codex`
alias c := codex
codex *args:
    cargo run --bin codex -- {args}

# Prefer the already-built debug binary (may be stale); fall back to `cargo run`.
codex-fast *args:
    just codex-stale-ok {args}

codex-lane *args:
    just cargo-lane codex cargo run --bin codex -- {args}

[unix]
codex-stale-ok *args:
    if [ -x target/debug/codex ]; then target/debug/codex "$@"; else cargo run --bin codex -- "$@"; fi

[windows]
codex-stale-ok *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $bin = "target\debug\codex.exe"; if (Test-Path -Path $bin -PathType Leaf) { & $bin @forwarded_args; exit $LASTEXITCODE }; cargo run --bin codex -- @forwarded_args

# `codex exec`
exec *args:
    cargo run --bin codex -- exec {args}

# Start `codex exec-server` and run codex-tui.
[no-cd]
[positional-arguments]
[unix]
tui-with-exec-server *args:
    {{ justfile_directory() }}/scripts/run_tui_with_exec_server.sh "$@"

# Run the CLI version of the file-search crate.
file-search *args:
    cargo run --bin codex-file-search -- {args}

# Run the structured source content search wrapper.
[no-cd]
source-search *args:
    cargo run --manifest-path "{{ justfile_directory() }}/codex-rs/Cargo.toml" --bin codex-source-search -- {args}

# Build the CLI and run the app-server test client in one target lane.
[unix]
app-server-test-client *args:
    cargo build --target-dir target/lanes/app-server-test-client -p codex-cli
    cargo run --target-dir target/lanes/app-server-test-client -p codex-app-server-test-client -- --codex-bin ./target/lanes/app-server-test-client/debug/codex "$@"

[windows]
app-server-test-client *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $target_dir = "target\lanes\app-server-test-client"; cargo build --target-dir $target_dir -p codex-cli; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; cargo run --target-dir $target_dir -p codex-app-server-test-client -- --codex-bin ".\$target_dir\debug\codex.exe" @forwarded_args

# Format the justfile and Rust code for the high-frequency local edit path.
fmt:
    {{ python }} ../scripts/format.py --fast-local

# Check the high-frequency local formatter set without modifying files.
fmt-check-fast:
    {{ python }} {{ justfile_directory() }}/scripts/format.py --check --fast-local

# Format the justfile, Rust, Prettier targets, Python SDK code, and Python scripts.
fmt-full:
    {{ python }} ../scripts/format.py

# Check formatting without modifying files.
fmt-check:
    {{ python }} ../scripts/format.py --check

[no-cd]
verify-local *args:
    @{{ python }} {{ justfile_directory() }}/scripts/verify_local.py {args}

[no-cd]
check-kd4-features *args:
    @{{ python }} {{ justfile_directory() }}/scripts/check_kd4_features.py {args}

[no-cd]
kd4-sync-audit *args:
    @{{ python }} {{ justfile_directory() }}/scripts/kd4_sync_audit.py {args}

[no-cd]
kd4-perf-snapshot *args:
    @{{ python }} {{ justfile_directory() }}/scripts/kd4_perf_snapshot.py {args}

[no-cd]
audit-scripts *args:
    @{{ python }} {{ justfile_directory() }}/scripts/root_maintenance.py audit-scripts {args}

[no-cd]
dev-env-doctor *args:
    @{{ python }} {{ justfile_directory() }}/scripts/dev_env_doctor.py {args}

[no-cd]
git-doctor *args:
    @{{ python }} {{ justfile_directory() }}/scripts/git_doctor.py {args}

[no-cd]
vscode-runtime-proof *args:
    @{{ python }} {{ justfile_directory() }}/scripts/vscode_runtime_proof.py {args}

[no-cd]
dead-code *args:
    @just --justfile "{{ justfile_directory() }}/justfile" cargo-shear {args}

[unix]
fix *args:
    @if [ "$#" -eq 0 ]; then echo "Pass a package/filter to 'just fix', or use 'just fix-workspace' for the broad workspace clippy fix."; exit 2; fi
    cargo clippy --fix --tests --allow-dirty {args}

[windows]
fix *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -eq 0) { Write-Error "Pass a package/filter to 'just fix', or use 'just fix-workspace' for the broad workspace clippy fix."; exit 2 }; powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --fix --tests --allow-dirty @forwarded_args

[unix]
fix-workspace *args:
    cargo clippy --fix --tests --allow-dirty {args}

[windows]
fix-workspace *args:
    $forwarded_args = @($args | Select-Object -Skip 1); powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --fix --tests --allow-dirty @forwarded_args

[unix]
clippy *args:
    @if [ "$#" -eq 0 ]; then echo "Pass a package/filter to 'just clippy', or use 'just clippy-workspace' for the broad workspace clippy check."; exit 2; fi
    cargo clippy --tests {args}

[windows]
clippy *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -eq 0) { Write-Error "Pass a package/filter to 'just clippy', or use 'just clippy-workspace' for the broad workspace clippy check."; exit 2 }; powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --tests @forwarded_args

[unix]
clippy-workspace *args:
    cargo clippy --tests {args}

[windows]
clippy-workspace *args:
    $forwarded_args = @($args | Select-Object -Skip 1); powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --tests @forwarded_args

[unix]
cargo-shear *args:
    @if [ "${1:-}" = "--" ]; then shift; fi; cargo shear --version >/dev/null 2>&1 || { echo "cargo-shear is not installed. Install with: cargo install --locked cargo-shear" >&2; exit 2; }; cargo shear --deny-warnings "$@"

[windows]
cargo-shear *args:
    @$forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo shear --version *> $null; if ($LASTEXITCODE -ne 0) { Write-Error "cargo-shear is not installed. Install with: cargo install --locked cargo-shear"; exit 2 }; cargo shear --deny-warnings @forwarded_args

[unix]
rust-dead-code-matrix *args:
    @if [ "${1:-}" = "--" ]; then shift; fi; workspace_arg="--workspace"; for arg in "$@"; do case "$arg" in -p|--package|--manifest-path) workspace_arg="";; esac; done; RUSTFLAGS="-Ddead_code" cargo check ${workspace_arg:+$workspace_arg} --all-targets "$@"

[windows]
rust-dead-code-matrix *args:
    @$forwarded_args = @($args | Select-Object -Skip 1); if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; $cargo_args = @("check"); if (-not (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package") -or ($forwarded_args -contains "--manifest-path"))) { $cargo_args += "--workspace" }; $cargo_args += "--all-targets"; $cargo_args += $forwarded_args; $env:RUSTFLAGS = "-Ddead_code"; cargo @cargo_args

[unix]
install:
    rustup show active-toolchain
    cargo fetch --locked

[windows]
install:
    #!powershell.exe -File
    $pwsh = Get-Command pwsh.exe -ErrorAction SilentlyContinue
    if (-not $pwsh) {
        winget install --exact --id Microsoft.PowerShell --source winget --accept-package-agreements --accept-source-agreements
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    rustup show active-toolchain
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo fetch --locked
    exit $LASTEXITCODE

[no-cd]
[windows]
publish-local-codex-dry-run *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -DryRun -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[unix]
publish-local-codex-dry-run *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -DryRun -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[windows]
publish-local-codex *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -AutoSkipBuild -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[unix]
publish-local-codex *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -AutoSkipBuild -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[windows]
publish-local-codex-final *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[unix]
publish-local-codex-final *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[windows]
publish-local-codex-final-dry-run *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -DryRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[unix]
publish-local-codex-final-dry-run *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -DryRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[windows]
publish-local-codex-runtime-proof *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -DryRun -SkipBuild -RunDoctor -RuntimeProof -FailOnStaleSourceBuild -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[unix]
publish-local-codex-runtime-proof *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -DryRun -SkipBuild -RunDoctor -RuntimeProof -FailOnStaleSourceBuild -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[no-cd]
[windows]
publish-local-codex-final-test-run *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -TestRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 {args}

[no-cd]
[unix]
publish-local-codex-final-test-run *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -TestRun -AutoSkipBuild -Profile release -RunDoctor -CloseRunningTargetTimeoutSeconds 30 {args}

[no-cd]
[windows]
publish-local-codex-build-only *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -BuildOnly {args}

[no-cd]
[unix]
publish-local-codex-build-only *args:
    @{{ justfile_directory() }}/scripts/publish-local-codex-wsl.sh -BuildOnly {args}

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
# Run `cargo install --locked cargo-nextest` if you don't have it installed.
# Workspace crate features are banned, so there should be no need to add
# `--all-features`.
[unix]
test *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast "$@"

[windows]
test *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if (($forwarded_args -contains "codex-core") -and (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package"))) { just _core-test-helpers-if-needed @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast @forwarded_args

# Fast local test loop: stop at the first failure and skip flaky retries.
[unix]
test-fast *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run "$@"

[windows]
test-fast *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if (($forwarded_args -contains "codex-core") -and (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package"))) { just _core-test-helpers-if-needed @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run @forwarded_args

[windows]
_core-test-helpers-if-needed *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $text = ($forwarded_args -join " "); $has_filter = ($forwarded_args -contains "-E") -or ($forwarded_args -contains "--filter-expr") -or ($text -match "--filter-expr="); if (-not $has_filter) { just _core-test-helpers; exit $LASTEXITCODE }; if ($text -match "(?i)rmcp|mcp|plugin|test_stdio_server") { just _core-test-helpers-mcp; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; if ($text -match "(?i)windows_sandbox|windows-sandbox|sandbox|codex_command_runner") { just _core-test-helpers-windows-sandbox; exit $LASTEXITCODE }

[windows]
_core-test-helpers:
    just _core-test-helpers-mcp
    just _core-test-helpers-windows-sandbox

[windows]
_core-test-helpers-mcp:
    cargo build -p codex-rmcp-client --bin test_stdio_server

[windows]
_core-test-helpers-windows-sandbox:
    cargo build -p codex-windows-sandbox --bin codex-windows-sandbox-setup
    cargo build -p codex-windows-sandbox --bin codex-command-runner

[windows]
test-fast-nosccache *args:
    $forwarded_args = @($args | Select-Object -Skip 1); $command_args = @("cargo", "nextest", "run") + $forwarded_args; & "{{ justfile_directory() }}\scripts\invoke-rust-perf-env.ps1" -NoSccache -CargoTargetLane "perf-nextest-nosccache" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $command_args; exit $LASTEXITCODE

[windows]
test-compile *args:
    $forwarded_args = @($args | Select-Object -Skip 1); powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo nextest run --no-run @forwarded_args

[unix]
test-windows-sandbox-processes *args:
    RUST_MIN_STACK={{ rust_min_stack }} cargo nextest run -p codex-windows-sandbox legacy_capture_cancellation_is_not_reported_as_timeout "$@"

[windows]
test-windows-sandbox-processes *args:
    $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; cargo nextest run -p codex-windows-sandbox legacy_capture_cancellation_is_not_reported_as_timeout @($args | Select-Object -Skip 1)

# Full local gate with benchmark startup smoke coverage.
test-full *args:
    just test {args}

test-full-with-bench *args:
    just test {args}
    just bench-smoke

cargo-fetch:
    cargo fetch --locked

build-dev-small package:
    cargo build --profile dev-small -p {{ package }}

# The named `package` parameter is also part of the forwarded positionals, so
# each platform must skip it explicitly instead of using the plain {args}
# expansion (which would pass the package name to the binary again).
[unix]
run-dev-small package *args:
    shift; cargo run --profile dev-small -p {{ package }} -- "$@"

[windows]
run-dev-small package *args:
    $forwarded_args = @($args | Select-Object -Skip 2); cargo run --profile dev-small -p {{ package }} -- @forwarded_args

local-release package:
    cargo build --profile local-release -p {{ package }}

# Run nextest in a caller-named target directory so multiple terminals can
# validate different slices without contending on the default Cargo target lock.
[unix]
test-lane lane *args:
    shift; RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --target-dir "target/lanes/{{ lane }}" --no-fail-fast "$@"

[windows]
test-lane lane *args:
    $forwarded_args = @($args | Select-Object -Skip 2); if (($forwarded_args -contains "codex-core") -and (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package"))) { just _core-test-helpers-if-needed @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $target_dir = "target\lanes\{{ lane }}"; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --target-dir $target_dir --no-fail-fast @forwarded_args

[windows]
test-lane-main *args:
    $forwarded_args = @($args | Select-Object -Skip 1); if (($forwarded_args -contains "codex-core") -and (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package"))) { just _core-test-helpers-if-needed @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $target_dir = "target\lanes\main"; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --target-dir $target_dir --no-fail-fast @forwarded_args

# Fast isolated local test loop for parallel validation lanes.
[unix]
test-lane-fast lane *args:
    shift; RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run --target-dir "target/lanes/{{ lane }}" "$@"

[windows]
test-lane-fast lane *args:
    $forwarded_args = @($args | Select-Object -Skip 2); if (($forwarded_args -contains "codex-core") -and (($forwarded_args -contains "-p") -or ($forwarded_args -contains "--package"))) { just _core-test-helpers-if-needed @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $target_dir = "target\lanes\{{ lane }}"; $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "fast"; cargo nextest run --target-dir $target_dir @forwarded_args

# Emit nextest timing reports for the selected local test slice.
[unix]
test-timings *args:
    RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=local cargo nextest run --no-fail-fast --timings=html,json "$@"

[windows]
test-timings *args:
    $env:RUST_MIN_STACK = "{{ rust_min_stack }}"; $env:NEXTEST_PROFILE = "local"; cargo nextest run --no-fail-fast --timings=html,json @($args | Select-Object -Skip 1)

# Focused crate test without repo-wide formatting.
validate-crate-focused crate:
    just test-fast -p {{ crate }}

# Validation ladder: fast local formatting, then a focused crate test.
validate-crate crate:
    just fmt-check-fast
    just test-fast -p {{ crate }}

# Full validation ladder for release-like source hygiene plus a focused crate test.
validate-crate-full crate:
    just fmt-check
    just test-fast -p {{ crate }}

# Validation ladder: prove local publish wiring without replacing the installed binary.
[no-cd]
[windows]
validate-local-publish *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\publish-local-codex.ps1" -DryRun -SkipBuild -FailOnStaleSourceBuild -ConfigureDesktopLocalCli -DesktopCliEnvironmentTarget User {args}

[unix]
cargo-lane lane *args:
    shift; target_dir="target/lanes/{{ lane }}"; if [ "${1:-}" = "cargo" ]; then shift; if [ "${1:-}" = "nextest" ] && { [ "${2:-}" = "run" ] || [ "${2:-}" = "archive" ]; }; then nextest_cmd="$2"; shift 2; cargo nextest "$nextest_cmd" --target-dir "$target_dir" "$@"; elif case "${1:-}" in bench|build|check|clippy|doc|fix|run|rustc|test) true;; *) false;; esac; then cargo_cmd="$1"; shift; cargo "$cargo_cmd" --target-dir "$target_dir" "$@"; else cargo "$@"; fi; else "$@"; fi

[windows]
cargo-lane lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane "{{ lane }}" @($args | Select-Object -Skip 2)

[windows]
cargo-lane-main *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane "main" @($args | Select-Object -Skip 1)

[windows]
cargo-lane-isolated-home lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane "{{ lane }}" -IsolateCargoHome @($args | Select-Object -Skip 2)

[windows]
cargo-lane-home lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane "{{ lane }}" -IsolateCargoHome @($args | Select-Object -Skip 2)

# Run from the repository root so scripts that resolve paths from `cwd` see
# the same layout they use in GitHub Actions.
[no-cd]
test-github-scripts:
    @{{ python }} -c "from pathlib import Path; import subprocess, sys; scripts = Path(r'{{ justfile_directory() }}') / '.github' / 'scripts'; print('No .github/scripts directory present; skipping.') if not scripts.is_dir() else None; sys.exit(0 if not scripts.is_dir() else subprocess.call([sys.executable, '-m', 'unittest', 'discover', '-s', str(scripts), '-p', 'test_*.py']))"

[no-cd]
rust-build-doctor:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py doctor

[no-cd]
target-disk:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py disk

[no-cd]
target-prune *args:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py prune {args}

# Preview target cache cleanup with `just target-optimize-dry-run` before pruning.
[no-cd]
target-optimize *args:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py optimize --keep-warm-per-base 2 --max-age-days 14 --max-lane-gib 25 {args}

[no-cd]
target-optimize-dry-run *args:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py optimize --dry-run --keep-warm-per-base 2 --max-age-days 14 --max-lane-gib 25 {args}

[no-cd]
lanes:
    @{{ python }} {{ justfile_directory() }}/scripts/rust_build_status.py lanes

[unix]
test-lane-package package *args:
    shift; RUST_MIN_STACK={{ rust_min_stack }} NEXTEST_PROFILE=fast cargo nextest run --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"

[windows]
test-lane-package package *args:
    @$forwarded_args = @($args | Select-Object -Skip 2); if ("{{ package }}" -eq "codex-core") { just _core-test-helpers-if-needed "-p" "{{ package }}" @forwarded_args; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE } }; $env:NEXTEST_PROFILE = "fast"; powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo nextest run -p "{{ package }}" @forwarded_args

[unix]
check-lane package *args:
    shift; cargo check --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"

[windows]
check-lane package *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo check -p "{{ package }}" @($args | Select-Object -Skip 2)

[unix]
clippy-lane package *args:
    shift; cargo clippy --tests --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"

[windows]
clippy-lane package *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --tests -p "{{ package }}" @($args | Select-Object -Skip 2)

[unix]
watch-lane package *args:
    shift; cargo watch -x "check -p {{ package }} --target-dir target/lanes/{{ package }}" "$@"

[windows]
watch-lane package *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo watch -x "check -p {{ package }}" @($args | Select-Object -Skip 2)

[unix]
coverage-lane package *args:
    shift; cargo llvm-cov --target-dir "target/lanes/{{ package }}" -p {{ package }} "$@"

[windows]
coverage-lane package *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo llvm-cov -p "{{ package }}" @($args | Select-Object -Skip 2)

# Match the Windows variant: fix only the named package in its own lane.
[unix]
fix-lane package *args:
    shift; cargo clippy --target-dir "target/lanes/{{ package }}" --fix --tests --allow-dirty -p {{ package }} "$@"

[windows]
fix-lane package *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane auto cargo clippy --fix --tests --allow-dirty -p "{{ package }}" @($args | Select-Object -Skip 2)

[unix]
release-lane *args:
    cargo build --release --target-dir target/lanes/release "$@"

[windows]
release-lane *args:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\scripts\cargo-lane.ps1" -Lane release cargo build --release @($args | Select-Object -Skip 1)

# Run an explicit benchmark target.
[unix]
bench package bench_name *args:
    shift 2; cargo bench -p {{ package }} --bench {{ bench_name }} "$@"

[windows]
bench package bench_name *args:
    $forwarded_args = @($args | Select-Object -Skip 3); cargo bench -p "{{ package }}" --bench "{{ bench_name }}" @forwarded_args

# Run every workspace benchmark only when explicitly requested.
bench-workspace *args:
    cargo bench --workspace --bench '*' {args}

# Run benchmark targets once to ensure they start successfully.
bench-smoke:
    cargo bench -p codex-utils-image --bench prompt_images -- --test

# Build the default Cargo workspace members with the release profile.
build-for-release *args:
    cargo build --release {args}

# Show duplicate crate versions in the CLI build graph.
deps-duplicates *args:
    cargo tree -d -p codex-cli {args}

# Show duplicate crate versions across the whole workspace and all target platforms.
deps-duplicates-workspace *args:
    cargo tree -d --workspace --target all {args}

[windows]
release-build-fast *args:
    # Standalone release compile proof only; publish-local-codex reads the profile target, not this lane artifact.
    $forwarded_args = @($args | Select-Object -Skip 1); $command_args = @("cargo", "build", "--release", "-p", "codex-cli") + $forwarded_args; & "{{ justfile_directory() }}\scripts\invoke-rust-perf-env.ps1" -CargoTargetLane "release-cli" -WorkingDirectory "{{ justfile_directory() }}\codex-rs" -ProgramArgs $command_args; exit $LASTEXITCODE

# Run the MCP server
mcp-server-run *args:
    cargo run -p codex-mcp-server -- {args}

# Regenerate the thread-config protobuf bindings through the platform-native wrapper.
[no-cd]
[unix]
generate-config-proto *args:
    {{ justfile_directory() }}/codex-rs/config/scripts/generate-proto.sh "$@"

[no-cd]
[windows]
generate-config-proto *args:
    $forwarded_args = @($args | Select-Object -Skip 1); powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\codex-rs\config\scripts\generate-proto.ps1" @forwarded_args; exit $LASTEXITCODE

# Verify the checked-in thread-config protobuf binding without replacing it.
[no-cd]
[unix]
generate-config-proto-check:
    {{ justfile_directory() }}/codex-rs/config/scripts/generate-proto.sh --check

[no-cd]
[windows]
generate-config-proto-check:
    @powershell -NoProfile -ExecutionPolicy Bypass -File "{{ justfile_directory() }}\codex-rs\config\scripts\generate-proto.ps1" -Check

# Regenerate the json schema for config.toml from the current config types.
write-config-schema:
    cargo run -p codex-core --bin codex-write-config-schema

# Run focused config schema fixture validation without regenerating schemas.
config-schema-protocol-check:
    cargo nextest run -p codex-core -E 'test(config_schema_matches_fixture) | test(config_schema_hides_unsupported_inline_mcp_bearer_token)'

# Regenerate config schema only when config inputs changed, then check freshness.
[no-cd]
config-schema-check:
    {{ python }} {{ justfile_directory() }}/scripts/config_schema_check.py --mode auto

# Force config schema regeneration, then check freshness.
[no-cd]
config-schema-check-force:
    {{ python }} {{ justfile_directory() }}/scripts/config_schema_check.py --mode force

# Regenerate vendored app-server protocol schema artifacts.
write-app-server-schema *args:
    cargo run -p codex-app-server-protocol --bin write_schema_fixtures -- {args}

# Run focused app-server runtime validation without regenerating schemas.
app-server-runtime-check:
    just app-server-command-exec-check
    just app-server-process-exec-check
    just app-server-thread-status-check

source-map-check:
    {{ python }} "{{ justfile_directory() }}/scripts/asciicheck.py" "{{ justfile_directory() }}/SOURCEMAP.md"
    {{ python }} "{{ justfile_directory() }}/scripts/readme_toc.py" "{{ justfile_directory() }}/SOURCEMAP.md"

tui-large-widget-check:
    cargo nextest run -p codex-tui -E 'test(footer_collapse_snapshots) | test(handle_paste_large_uses_placeholder_and_replaces_on_submit) | test(resume_picker)'
    cargo check -p codex-tui

deps-duplicates-check *args:
    just deps-duplicates {args}

# Refresh the advisory database and audit the locked dependency graph.
deps-audit:
    cargo audit

# Dependency policy gate for the dependency-cleanup surface: duplicate report
# plus the offline cargo-deny checks. Advisories need network access, so they
# stay in the separate `deps-audit` gate configured by .cargo/audit.toml.
deps-policy-check *args:
    just _cargo-deny-installed
    just deps-duplicates {args}
    cargo deny check bans sources licenses

[unix]
_cargo-deny-installed:
    @cargo deny --version >/dev/null 2>&1 || { echo "cargo-deny is not installed. Install with: cargo install --locked cargo-deny" >&2; exit 2; }

[windows]
_cargo-deny-installed:
    @cargo deny --version *> $null; if ($LASTEXITCODE -ne 0) { Write-Error "cargo-deny is not installed. Install with: cargo install --locked cargo-deny"; exit 2 }

# Typecheck and test the TypeScript SDK.
[no-cd]
sdk-ts-check:
    pnpm --dir "{{ justfile_directory() }}" --filter @openai/codex-sdk run typecheck
    pnpm --dir "{{ justfile_directory() }}" --filter @openai/codex-sdk run test

# Run the Python SDK test suite (default marker exclusions apply).
[no-cd]
sdk-python-check:
    uv run --directory "{{ justfile_directory() }}/sdk/python" --extra dev pytest

# Lint the codex-cli npm wrapper entrypoint.
[no-cd]
codex-cli-wrapper-check:
    pnpm --dir "{{ justfile_directory() }}" run lint:js

app-server-command-exec-check:
    cargo nextest run -p codex-app-server-protocol -E 'test(command_exec_response_round_trips_runtime_status)'
    cargo nextest run -p codex-app-server -E 'test(suite::v2::command_exec::command_exec_non_streaming_respects_output_cap)'
    cargo check -p codex-app-server

app-server-process-exec-check:
    cargo nextest run -p codex-app-server-protocol -E 'test(process_notifications_round_trip)'
    cargo nextest run -p codex-app-server -E 'test(process_spawn_reports_buffered_output_cap_reached)'
    cargo check -p codex-app-server

app-server-thread-status-check:
    cargo nextest run -p codex-core -E 'test(validated_invalidated_tracker_still_requests_diff_fallback)'
    cargo nextest run -p codex-app-server -E 'test(thread_status::tests::stale_active_running_thread_resume_clears_watch_status) | test(thread_status::tests::stale_active_repair_preserves_pending_approval_status)'
    cargo check -p codex-app-server

app-server-schema-protocol-check:
    cargo nextest run -p codex-app-server-protocol -E 'test(typescript_schema_fixtures_match_generated) | test(json_schema_fixtures_match_generated)'

# Regenerate app-server schemas only when protocol inputs changed, then check schema fixtures.
[no-cd]
app-server-schema-check:
    {{ python }} {{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py --mode auto

# Force app-server schema regeneration, then check schema fixtures.
[no-cd]
app-server-schema-check-force:
    {{ python }} {{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py --mode force

# Compatibility wrapper for callers that still want schema and runtime proof together.
[no-cd]
app-server-schema-runtime-check:
    {{ python }} {{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py --mode auto --runtime

[no-cd]
app-server-schema-runtime-check-with-runtime:
    {{ python }} {{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py --mode auto --runtime

# Compatibility wrapper for forced schema regeneration plus runtime proof.
[no-cd]
app-server-schema-runtime-check-force:
    {{ python }} {{ justfile_directory() }}/scripts/app_server_schema_runtime_check.py --mode force --runtime

# Regenerate hook schema artifacts through the Rust workspace from any cwd.
[no-cd]
write-hooks-schema:
    cargo run --manifest-path {{ justfile_directory() }}/codex-rs/Cargo.toml -p codex-hooks --bin write_hooks_schema_fixtures

# Run the argument-comment Dylint checks across codex-rs.
[no-cd]
[unix]
argument-comment-lint *args:
    {{ justfile_directory() }}/tools/argument-comment-lint/run-prebuilt-linter.py "$@"

[no-cd]
[windows]
argument-comment-lint *args:
    $forwarded_args = {args}; {{ python }} {{ justfile_directory() }}/tools/argument-comment-lint/run-prebuilt-linter.py @forwarded_args

[no-cd]
[unix]
argument-comment-lint-from-source *args:
    {{ justfile_directory() }}/tools/argument-comment-lint/run.py "$@"

[no-cd]
[windows]
argument-comment-lint-from-source *args:
    $forwarded_args = {args}; {{ python }} {{ justfile_directory() }}/tools/argument-comment-lint/run.py @forwarded_args

# Tail logs from the state SQLite database
[unix]
log *args:
    if [ "${1:-}" = "--" ]; then shift; fi; cargo run -p codex-state --bin logs_client -- "$@"

[windows]
log *args:
    $forwarded_args = {args}; if ($forwarded_args.Count -gt 0 -and $forwarded_args[0] -eq "--") { $forwarded_args = @($forwarded_args | Select-Object -Skip 1) }; cargo run -p codex-state --bin logs_client -- @forwarded_args
