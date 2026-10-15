//! KD4's full Rust suite admission policy. This is a launch limit, not a result
//! cache: edits, failures, cancellation and force_fresh never replenish it.
//! Arbitrary programs/scripts remain outside this command-recognition boundary.
//! So are launches from a local working directory outside a KD4 checkout: other
//! projects keep their own test conventions.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::argv_commands;
use super::infer_direct_shell_type;
use crate::shell::ShellType;

#[derive(Debug, Default)]
pub(crate) struct FullSuiteBudget(AtomicBool);

impl FullSuiteBudget {
    pub(crate) fn admit(&self, full_suite: bool) -> Result<(), String> {
        if full_suite && self.0.swap(true, Ordering::Relaxed) {
            return Err("Full-suite launch blocked: this user turn already admitted its one full Rust suite attempt. Failure, cancellation, source edits and force_fresh do not reset the allowance. Resume a live run or run focused tests; another full suite requires explicit user permission.".into());
        }
        Ok(())
    }
}

pub(crate) async fn check_full_suite_command(
    command: &[String],
    shell_type: Option<ShellType>,
    local_cwd: Option<&Path>,
) -> Result<bool, String> {
    let command = command.to_vec();
    let local_cwd = local_cwd.map(Path::to_path_buf);
    crate::tools::run_blocking_command_analysis(move || {
        // An unknown or remote working directory stays governed.
        if local_cwd
            .as_deref()
            .is_some_and(|cwd| !in_kd4_checkout(cwd))
        {
            return Ok(false);
        }
        check_command(&command, shell_type, 0)
    })
    .await
    .map_err(|error| format!("full-suite policy worker failed: {error}"))?
}

fn in_kd4_checkout(cwd: &Path) -> bool {
    cwd.ancestors()
        .any(|root| root.join("codex-rs/.config/kd4-rust-tests.toml").is_file())
}

fn check_command(
    command: &[String],
    shell_type: Option<ShellType>,
    depth: usize,
) -> Result<bool, String> {
    if depth > 4 {
        return Err(
            "Full-suite policy cannot inspect more than four command wrappers; use direct argv."
                .into(),
        );
    }
    let shell_type = shell_type.or_else(|| infer_direct_shell_type(command));
    if let Some(shell_type) = shell_type {
        let commands = argv_commands(command, Some(shell_type));
        let Some(commands) = commands else {
            // Fail closed for visibly test-bearing opaque scripts, not for all
            // scripting. This is deliberately not an OS subprocess sandbox.
            if command.iter().any(|arg| visibly_runs_tests(arg)) {
                return Err("Cannot verify full-suite scope in this script. Use one standalone test command (and the workdir argument), not a loop or generated command.".into());
            }
            return Ok(false);
        };
        let mut full_suite = false;
        for argv in &commands {
            full_suite |= check_command(argv, None, depth + 1)?;
        }
        if full_suite {
            // A flattened AST alone cannot prove that a leaf runs once (loops,
            // functions and pipelines may repeat it). Require a standalone argv.
            let script = match shell_type {
                ShellType::PowerShell => {
                    codex_shell_command::powershell::extract_powershell_command(command)
                }
                _ => None,
            };
            let standalone = if let Some((_, script)) = script {
                codex_shell_command::validation::standalone_powershell_argv(script).is_some()
            } else if matches!(shell_type, ShellType::Bash | ShellType::Sh | ShellType::Zsh) {
                command
                    .last()
                    .and_then(|script| codex_shell_command::validation::standalone_argv(script))
                    .is_some()
            } else {
                false
            };
            if commands.len() != 1 || !standalone {
                return Err("A full suite must be a single standalone command, not a compound script, pipeline or loop. Set workdir separately.".into());
            }
        }
        return Ok(full_suite);
    }
    let Some((program, args)) = command.split_first() else {
        return Ok(false);
    };
    let program = basename(program);
    if matches!(program.as_str(), "env" | "command") {
        let start = args
            .iter()
            .position(|arg| !arg.starts_with('-') && !arg.contains('='));
        return start.map_or(Ok(false), |start| {
            check_command(&args[start..], None, depth + 1)
        });
    }
    if matches!(program.as_str(), "python" | "python3" | "py") {
        if let Some(index) = args
            .iter()
            .position(|arg| basename(arg) == "rust_build_status.py")
        {
            if let Some(separator) = args[index + 1..].iter().position(|arg| arg == "--") {
                return check_command(&args[index + separator + 2..], None, depth + 1);
            }
        }
        if let Some(index) = args
            .iter()
            .position(|arg| basename(arg) == "rust_test_runner.py")
        {
            if let Some(run) = args[index + 1..].iter().position(|arg| arg == "run-target") {
                return check_test_args(&args[index + run + 2..], 1, false);
            }
        }
        return Ok(false);
    }
    if program == "cargo" {
        let mut index = 0;
        while let Some(arg) = args.get(index) {
            if matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V") {
                return Ok(false);
            }
            if matches!(
                arg.as_str(),
                "--config" | "--color" | "-C" | "-Z" | "--manifest-path"
            ) {
                index += 2;
            } else if arg.starts_with(['-', '+']) {
                index += 1;
            } else {
                break;
            }
        }
        if !args
            .get(index)
            .is_some_and(|arg| matches!(arg.as_str(), "test" | "nextest"))
        {
            return Ok(false);
        }
        if args[index] == "nextest" {
            if args.get(index + 1).map(String::as_str) != Some("run") {
                return Ok(false);
            }
            return check_test_args(&args[index + 2..], 0, true);
        }
        return check_test_args(&args[index + 1..], 0, false);
    }
    if program == "just" {
        let Some(index) = just_recipe_index(args) else {
            return Ok(false);
        };
        let (positionals, nextest) = match args[index].as_str() {
            "core-test"
            | "core-test-fast"
            | "core-test-small"
            | "core-test-lane"
            | "_core-test-small-reserved" => (1, false),
            "_core-test-reserved" => (2, false),
            "test"
            | "test-fast"
            | "test-timings"
            | "test-lane-main"
            | "test-fast-nosccache"
            | "_test-lane-local-reserved"
            | "_test-lane-fast-reserved" => (0, true),
            "test-lane"
            | "test-lane-fast"
            | "test-lane-package"
            | "_test-lane-package-reserved" => (1, true),
            "validate-crate" | "validate-crate-full" | "validate-crate-focused" => (1, true),
            "_validate-crate" => (2, true),
            _ => return Ok(false),
        };
        return check_test_args(&args[index + 1..], positionals, nextest);
    }
    Ok(false)
}

fn basename(program: &str) -> String {
    let name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

/// Index of the recipe among `just` arguments; `None` when nothing runs.
fn just_recipe_index(args: &[String]) -> Option<usize> {
    // Skip only documented global option values, never search arbitrary
    // arguments for a recipe name (e.g. `just --show test`).
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if matches!(
            arg.as_str(),
            "--dry-run" | "--show" | "--list" | "--summary" | "--help" | "--version"
        ) {
            return None;
        }
        if arg == "--set" {
            index += 3;
        } else if matches!(
            arg.as_str(),
            "--justfile" | "-f" | "--working-directory" | "-d" | "--shell" | "--shell-arg"
        ) {
            index += 2;
        } else if arg.starts_with('-') || arg.contains('=') {
            index += 1;
        } else {
            break;
        }
    }
    (index < args.len()).then_some(index)
}

/// Whether an unparsed script launches tests, judged one statement at a time.
/// Naming a test tool as a path, pattern or lane (`lanes/core-tests`, a process
/// filter, `rg core-test justfile`) launches nothing and stays inspectable.
fn visibly_runs_tests(script: &str) -> bool {
    script
        .split(|ch: char| matches!(ch, ';' | '|' | '&' | '(' | ')' | '{' | '}' | '\n' | '\r'))
        .any(|statement| {
            let tokens = statement
                .split_whitespace()
                .map(|token| basename(token.trim_matches(['\'', '"'])))
                .collect::<Vec<_>>();
            tokens.iter().enumerate().any(|(index, token)| {
                let rest = &tokens[index + 1..];
                match token.as_str() {
                    "cargo" => rest
                        .first()
                        .is_some_and(|next| matches!(next.as_str(), "test" | "nextest")),
                    "just" => just_recipe_index(rest).is_some_and(|recipe| {
                        rest[recipe].starts_with("core-test") || rest[recipe].starts_with("test")
                    }),
                    "rust_test_runner.py" => {
                        // Exempt only a literal subcommand help query, not a
                        // script that merely contains --help elsewhere. Use
                        // raw arguments: basename normalization can turn an
                        // option value such as path/--help into a help flag.
                        let mut args = statement.split_whitespace().skip(index + 1);
                        if matches!(
                            (args.next(), args.next(), args.next()),
                            (
                                Some("run-target" | "run-gate" | "check-gates"),
                                Some("--help" | "-h"),
                                None
                            )
                        ) {
                            return false;
                        }
                        rest.iter().any(|arg| {
                            matches!(arg.as_str(), "run-target" | "run-gate" | "check-gates")
                        })
                    }
                    _ => false,
                }
            })
        })
}

fn check_test_args(args: &[String], mut positionals: usize, nextest: bool) -> Result<bool, String> {
    let mut focused = false;
    let mut explicit_all = false;
    let mut no_fail_fast = false;
    let mut zero_retries = false;
    let mut conflict = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
        if matches!(
            flag,
            "--help" | "-h" | "--version" | "--no-run" | "--list" | "--dry-run"
        ) {
            return Ok(false);
        }
        let takes_value = matches!(
            flag,
            "-E" | "--filterset"
                | "--retries"
                | "--profile"
                | "--cargo-profile"
                | "-p"
                | "--package"
                | "--test"
                | "--bin"
                | "--example"
                | "--bench"
                | "--manifest-path"
                | "--manifest"
                | "--target-dir"
                | "--target"
                | "--features"
                | "-F"
                | "--exclude"
                | "--skip"
                | "--run-ignored"
                | "--partition"
                | "--test-threads"
                | "-j"
                | "--jobs"
                | "--color"
                | "--status-level"
                | "--final-status-level"
                | "--success-output"
                | "--failure-output"
                | "--show-progress"
                | "--command-timeout-seconds"
                | "--admission-timeout-seconds"
                | "--config-file"
                | "--config"
                | "--no-tests"
                | "--message-format"
                | "-P"
                | "--archive-file"
                | "--workspace-remap"
                | "--binaries-metadata"
                | "--cargo-metadata"
                | "--build-jobs"
                | "--cargo-message-format"
                | "--platform-filter"
                | "--flaky-result"
                | "--max-fail"
                | "--stress-count"
                | "--stress-duration"
                | "--debugger"
                | "--tracer"
                | "--max-progress-running"
                | "--message-format-version"
                | "--archive-format"
                | "--extract-to"
                | "--target-dir-remap"
                | "--build-dir-remap"
                | "--user-config-file"
                | "--tool-config-file"
                | "-R"
                | "--rerun"
        );
        let value = if takes_value {
            inline.or_else(|| args.get(index + 1).map(String::as_str))
        } else {
            inline
        };
        match flag {
            "--all" | "--all-tests" => explicit_all = true,
            "--no-fail-fast" | "--nff" => no_fail_fast = true,
            "--fail-fast" | "--ff" | "--max-fail" | "--stress-count" | "--stress-duration" => {
                conflict = true;
            }
            "--retries" => {
                zero_retries |= value == Some("0");
                conflict |= value != Some("0");
            }
            "-E" | "--filterset" => {
                // Literal test names/prefixes establish a focused exception.
                // Expressions such as all(), regexes, and negations do not.
                focused |= value.is_some_and(focused_filter);
            }
            _ if !arg.starts_with('-') => {
                if positionals > 0 {
                    positionals -= 1;
                } else {
                    focused |= !arg.is_empty() && !arg.contains(['*', '?']);
                }
            }
            _ => {}
        }
        index += if takes_value && inline.is_none() {
            2
        } else {
            1
        };
    }
    if focused && !explicit_all {
        return Ok(false);
    }
    if conflict || !no_fail_fast || (nextest && !zero_retries) {
        let retries = if nextest { " and --retries 0" } else { "" };
        return Err(format!(
            "Full-suite command rejected before admission: use --no-fail-fast{retries}; fail-fast, max-fail, retries and stress/repeat overrides are not allowed. The one-run allowance has not been consumed."
        ));
    }
    // Named runner paths already force --retries 0 when reporting run-target
    // results. Cargo test has no automatic retry option.
    Ok(true)
}

fn focused_filter(filter: &str) -> bool {
    filter.split('|').all(|term| {
        term.trim()
            .strip_prefix("test(")
            .and_then(|term| term.strip_suffix(')'))
            .map(|name| name.strip_prefix('=').unwrap_or(name))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ':' | '-'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(command: &str) -> Vec<String> {
        command.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn full_suite_rust_entrypoints_share_one_allowance() {
        for command in [
            "cargo test --workspace --no-fail-fast",
            "cargo nextest run --no-fail-fast --retries 0",
            "just core-test-fast core_lib --all --no-fail-fast",
            "python -I -B scripts/build/rust_test_runner.py run-target --profile fast core_lib --all --no-fail-fast",
            "just --set rust_validation_wait_seconds 5 test-fast -p codex-utils --no-fail-fast --retries=0",
            "python scripts/build/rust_build_status.py run-lane --lane core-tests -- cargo test --no-fail-fast",
            "C:\\tools\\cargo.exe +stable test -p example --no-fail-fast",
            "just core-test-lane core_lib --all --no-fail-fast",
            "just _core-test-reserved fast core_lib --all --no-fail-fast",
        ] {
            assert_eq!(
                check_command(&words(command), None, 0),
                Ok(true),
                "{command}"
            );
        }
        let budget = FullSuiteBudget::default();
        budget
            .admit(check_command(&words("cargo test --no-fail-fast"), None, 0).unwrap())
            .unwrap();
        let denial = budget
            .admit(
                check_command(
                    &words("just core-test core_lib --all --no-fail-fast"),
                    None,
                    0,
                )
                .unwrap(),
            )
            .expect_err("a second full suite must be denied");
        assert!(denial.contains("explicit user permission"), "{denial}");
        assert!(!denial.contains("requires a new user turn"), "{denial}");
        assert!(
            budget.admit(false).is_ok(),
            "focused followups must remain possible"
        );
        assert!(
            FullSuiteBudget::default().admit(true).is_ok(),
            "a distinct runtime budget starts unused; this does not establish task-wide permission"
        );
    }

    #[test]
    fn full_suite_requires_continue_without_retries_before_consuming_budget() {
        for command in [
            "cargo test",
            "cargo nextest run --no-fail-fast",
            "cargo nextest run --retries 0",
            "cargo nextest run --no-fail-fast --retries 1 --retries 0",
            "just core-test core_lib --all --no-fail-fast --fail-fast",
            "cargo nextest run --no-fail-fast --retries=0 --ff",
            "cargo nextest run --no-fail-fast --retries 0 --max-fail 1",
            "cargo nextest run --no-fail-fast --retries 0 --stress-count 2",
            "cargo nextest run --no-fail-fast --retries 0 --stress-duration 1h",
        ] {
            assert!(
                check_command(&words(command), None, 0).is_err(),
                "{command}"
            );
        }
    }

    #[test]
    fn full_suite_does_not_charge_focused_tests_or_discovery() {
        for command in [
            "cargo test -p example one_test",
            "cargo nextest list",
            "cargo test --no-run",
            "cargo test -- --list",
            "just --dry-run test",
            "just core-test-plan core_lib",
            "just core-gate capability-command-preflight",
            "just core-test-fast core_lib -E test(=one_test)",
            "just core-test-fast core_lib -E test(one_module::)",
            "cargo test --workspace one_test",
            "cargo --help test",
            "cargo build --bin test",
            "python scripts/build/rust_test_runner.py run-target core_lib one_test",
            "echo cargo test",
            "rg cargo source.rs",
        ] {
            assert_eq!(
                check_command(&words(command), None, 0),
                Ok(false),
                "{command}"
            );
        }
        for filter in ["all()", "test(/.*/)", "test(=one)|all()"] {
            assert!(
                check_command(&words(&format!("cargo nextest run -E {filter}")), None, 0).is_err()
            );
        }
    }

    #[test]
    fn full_suite_atomic_admission_never_waits_for_the_running_test() {
        let budget = FullSuiteBudget::default();
        let barrier = std::sync::Barrier::new(8);
        let admitted = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        budget.admit(true).is_ok()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| usize::from(handle.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(admitted, 1);
        // There is no refund on drop, cancellation, a failed test or an edit.
        assert!(budget.admit(true).is_err());
        assert!(budget.admit(false).is_ok());
    }

    #[tokio::test]
    async fn full_suite_shell_policy_rejects_compound_and_loop_launches() {
        for script in [
            "cargo test --no-fail-fast; cargo test --no-fail-fast",
            "for x in 1 2; do cargo test --no-fail-fast; done",
        ] {
            assert!(
                check_full_suite_command(
                    &["bash".into(), "-lc".into(), script.into()],
                    Some(ShellType::Bash),
                    None
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            check_full_suite_command(
                &[
                    "bash".into(),
                    "-lc".into(),
                    "cargo test --no-fail-fast".into()
                ],
                Some(ShellType::Bash),
                None
            )
            .await,
            Ok(true)
        );
        assert_eq!(
            check_full_suite_command(
                &[
                    "bash".into(),
                    "-lc".into(),
                    "for x in 1 2; do cargo build; done".into()
                ],
                Some(ShellType::Bash),
                None
            )
            .await,
            Ok(false)
        );
    }

    #[test]
    fn full_suite_opaque_script_guard_separates_inspection_from_launch() {
        // Read-only lane and holder inspections that only name the test tools.
        for script in [
            "Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(cargo|cargo-nextest|python|just)(\\.exe)?$' -and $_.CommandLine -match 'core-tests|rust_test_runner|core-test|core-gate' } | Select-Object ProcessId,CommandLine | ConvertTo-Json",
            "Get-ChildItem -LiteralPath codex-rs/target/lanes/core-tests -Force -File | Select-Object Name,Length | ConvertTo-Json",
            "Get-Item scripts/build/rust_test_runner.py, justfile | ForEach-Object { $_.Length }; rg -n 'core-test|test-fast|rust_test_runner' justfile",
            "python -B scripts/build/rust_build_status.py lanes; if ($LASTEXITCODE -eq 0) { python -B scripts/build/rust_build_status.py run-lane --lane core-tests -- cargo check -p codex-cli }",
            "rg -n 'run-target' scripts/build/rust_test_runner.py | Select-Object -First 5",
        ] {
            assert!(!visibly_runs_tests(script), "{script}");
            // Exercise the actual policy entry branch too: a helper-only
            // assertion cannot detect shell extraction/recursion regressions.
            assert_eq!(
                check_command(
                    &[
                        "pwsh".into(),
                        "-NoProfile".into(),
                        "-Command".into(),
                        script.into()
                    ],
                    Some(ShellType::PowerShell),
                    0,
                ),
                Ok(false),
                "{script}",
            );
        }
        for script in [
            "foreach ($i in 1,2) { just core-test-fast core_lib --all --no-fail-fast }",
            "$log = 'out.txt'; just --set rust_validation_wait_seconds 5 core-test-fast core_lib -E 'test(=one)' | Tee-Object $log",
            "& \"C:\\kd4 checkout\\.venv\\python.exe\" \"C:\\kd4 checkout\\scripts\\build\\rust_test_runner.py\" --target-dir t run-target core_lib --all",
            "$r = python -I -B scripts/build/rust_test_runner.py run-gate demo | Out-String",
            "if ($true) { cargo nextest run --no-fail-fast --retries 0 }",
            "iex 'cargo test --workspace'",
            "just test-fast -p codex-core | Select-Object -Last 5",
        ] {
            assert!(visibly_runs_tests(script), "{script}");
        }
    }

    #[test]
    fn full_suite_opaque_runner_help_does_not_hide_test_launches() {
        let help = "python -I -B scripts/build/rust_test_runner.py run-target --help";
        let batch = format!(
            "python -I -B scripts/build/rust_test_runner.py --help; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; {help}; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; python -B scripts/maintenance/root_maintenance.py test-python --help; python -B scripts/maintenance/root_maintenance.py audit-scripts --help"
        );
        for script in [
            batch,
            format!("if ($true) {{ {help} }}"),
            "if ($true) { python scripts/build/rust_test_runner.py run-gate -h }".into(),
            "if ($true) { python scripts/build/rust_test_runner.py check-gates --help }".into(),
        ] {
            assert!(!visibly_runs_tests(&script), "{script}");
            assert_eq!(
                check_command(
                    &["pwsh".into(), "-Command".into(), script.clone()],
                    Some(ShellType::PowerShell),
                    0,
                ),
                Ok(false),
                "{script}",
            );
        }
        for script in [
            format!("{help}; foreach ($i in 1,2) {{ cargo test --no-fail-fast }}"),
            format!(
                "{help}; if ($true) {{ python scripts/build/rust_test_runner.py run-target core_lib --all }}"
            ),
        ] {
            assert!(visibly_runs_tests(&script), "{script}");
            assert!(
                check_command(
                    &["pwsh".into(), "-Command".into(), script.clone()],
                    Some(ShellType::PowerShell),
                    0,
                )
                .is_err(),
                "{script}",
            );
        }
        // Ambiguous tokens must not gain the opaque-script exemption. These
        // helper checks deliberately do not constrain the structured parser's
        // separate handling of focused tests or forwarded help arguments.
        for script in [
            "python scripts/build/rust_test_runner.py run-target path/--help",
            "python scripts/build/rust_test_runner.py run-target core_lib -- --help",
            "python scripts/build/rust_test_runner.py run-target core_lib; Write-Output --help",
        ] {
            assert!(visibly_runs_tests(script), "{script}");
        }
    }

    fn kd4_checkout() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("codex-rs/.config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("kd4-rust-tests.toml"), "").unwrap();
        root
    }

    #[tokio::test]
    async fn full_suite_policy_governs_only_kd4_checkouts_and_unknown_cwds() {
        let kd4 = kd4_checkout();
        let nested = kd4.path().join("codex-rs/core");
        std::fs::create_dir_all(&nested).unwrap();
        let foreign = tempfile::tempdir().unwrap();
        // `cargo test` lacks --no-fail-fast, which the policy rejects.
        for (cwd, governed) in [
            (None, true),
            (Some(kd4.path()), true),
            (Some(nested.as_path()), true),
            (Some(foreign.path()), false),
        ] {
            let result = check_full_suite_command(&words("cargo test"), None, cwd).await;
            if governed {
                assert!(result.is_err(), "{cwd:?}: {result:?}");
            } else {
                assert_eq!(result, Ok(false), "{cwd:?}");
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn full_suite_powershell_policy_preserves_single_launch_boundary() {
        for (script, allowed) in [
            ("cargo test --no-fail-fast", true),
            (
                "cargo test --no-fail-fast; cargo test --no-fail-fast",
                false,
            ),
            ("foreach ($i in 1,2) { cargo test --no-fail-fast }", false),
        ] {
            let result = check_full_suite_command(
                &[
                    "pwsh".into(),
                    "-NoProfile".into(),
                    "-Command".into(),
                    script.into(),
                ],
                Some(ShellType::PowerShell),
                None,
            )
            .await;
            if allowed {
                assert_eq!(result, Ok(true), "{script}");
            } else {
                assert!(result.is_err(), "{script}: {result:?}");
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn full_suite_powershell_data_output_is_not_a_test_launch() {
        for script in [
            "$commands = @('cargo test --workspace'); foreach ($command in $commands) { Write-Output $command }",
            "foreach ($command in @('just test-fast', 'cargo test')) { Write-Output $command }",
        ] {
            assert_eq!(
                check_full_suite_command(
                    &[
                        "pwsh".into(),
                        "-NoProfile".into(),
                        "-Command".into(),
                        script.into(),
                    ],
                    Some(ShellType::PowerShell),
                    None,
                )
                .await,
                Ok(false),
                "{script}",
            );
        }
        // Similar-looking scripts that actually invoke tests must still fail
        // closed. Quoted command text alone must never grant admission.
        for script in [
            "$commands = @('cargo test --workspace'); foreach ($command in $commands) { Invoke-Expression $command }",
            "foreach ($command in @('cargo test')) { Write-Output $(cargo test --workspace) }",
            "foreach ($command in @('cargo test')) { cargo test --workspace }",
        ] {
            assert!(
                check_full_suite_command(
                    &[
                        "pwsh".into(),
                        "-NoProfile".into(),
                        "-Command".into(),
                        script.into(),
                    ],
                    Some(ShellType::PowerShell),
                    None,
                )
                .await
                .is_err(),
                "{script}",
            );
        }
    }

    #[tokio::test]
    async fn full_suite_both_handlers_enforce_shared_budget_even_with_force_fresh() {
        use crate::session::step_context::StepContext;
        use crate::tools::context::ToolCallSource;
        use crate::tools::context::ToolInvocation;
        use crate::tools::context::ToolPayload;
        use crate::tools::handlers::ExecCommandHandler;
        use crate::tools::handlers::ShellCommandHandler;
        use crate::tools::registry::ToolExecutor;
        use std::sync::Arc;

        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        Arc::make_mut(&mut turn.config)
            .features
            .enable(codex_features::Feature::Kd4Runtime)
            .unwrap();
        // A missing executable ensures even a broken guard cannot run a suite.
        let temp = kd4_checkout();
        let program = temp.path().join("cargo.exe");
        turn.full_suite_budget.admit(true).unwrap();
        let turn = Arc::new(turn);
        let session = Arc::new(session);
        let invocation = |tool_name| ToolInvocation {
            session: Arc::clone(&session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            tracker: Arc::new(tokio::sync::Mutex::new(
                crate::turn_diff_tracker::TurnDiffTracker::new(),
            )),
            call_id: "full-suite-blocked".into(),
            tool_name,
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: serde_json::json!({
                    "program": program, "args": ["test", "--no-fail-fast"], "force_fresh": true,
                    "workdir": temp.path(),
                })
                .to_string(),
            },
            cancellation_token: tokio_util::sync::CancellationToken::new(),
        };
        let exec = ExecCommandHandler::default();
        let shell = ShellCommandHandler::default();
        for result in [
            exec.handle_call(invocation(exec.tool_name())).await,
            shell.handle_call(invocation(shell.tool_name())).await,
        ] {
            match result {
                Err(crate::FunctionCallError::RespondToModel(message)) => {
                    assert!(message.contains("Full-suite launch blocked"), "{message}")
                }
                _ => panic!("both command handlers must reject a second full suite before launch"),
            }
        }
    }
}
