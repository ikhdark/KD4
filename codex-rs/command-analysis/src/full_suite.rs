//! KD4's full Rust suite admission policy. This is a launch limit, not a result
//! cache: edits, failures, cancellation and force_fresh never replenish it.
//! Arbitrary programs/scripts remain outside this command-recognition boundary.
//! So are launches from a local working directory outside a KD4 checkout: other
//! projects keep their own test conventions.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use super::argv_commands;
use super::infer_direct_shell_type;
use codex_shell_command::shell_detect::ShellType;

#[derive(Debug, Default)]
pub struct FullSuiteBudget(Mutex<Option<SuiteReservation>>, Option<(String, String)>);

#[derive(Debug)]
struct SuiteReservation {
    owner: String,
    started: bool,
    acknowledgement: Option<(PathBuf, String)>,
}

/// Dropping a prelaunch reservation releases it. Possible execution never does.
pub struct FullSuitePermit {
    budget: Arc<FullSuiteBudget>,
    owner: String,
}

const BUDGET_EXHAUSTED: &str = "Full-suite launch blocked: this user task already reserved or started its full Rust suite allowance. Failure, cancellation, source edits and force_fresh do not reset a started allowance. Resume a live run or run focused tests; another full suite requires explicit user permission.";

impl FullSuiteBudget {
    pub fn for_task(task: String, grant: String) -> Self {
        Self(Mutex::new(None), Some((task, grant)))
    }

    pub fn task_scope(&self) -> Option<&(String, String)> {
        self.1.as_ref()
    }

    pub fn admit(&self, full_suite: bool) -> Result<(), String> {
        if full_suite {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.is_some() {
                return Err(BUDGET_EXHAUSTED.into());
            }
            *state = Some(SuiteReservation {
                owner: String::new(),
                started: true,
                acknowledgement: None,
            });
        }
        Ok(())
    }

    pub fn reserve(
        self: &Arc<Self>,
        full_suite: bool,
        owner: String,
    ) -> Result<Option<FullSuitePermit>, String> {
        if !full_suite {
            return Ok(None);
        }
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.is_some() {
            return Err(BUDGET_EXHAUSTED.into());
        }
        *state = Some(SuiteReservation {
            owner: owner.clone(),
            started: false,
            acknowledgement: None,
        });
        Ok(Some(FullSuitePermit {
            budget: Arc::clone(self),
            owner,
        }))
    }

    pub async fn refresh_owned_nonexecution(&self) {
        // Read storage outside the state lock. Reconcile only the exact owner
        // sampled before the read; a newer reservation must never be refunded.
        let previous = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|state| {
                state
                    .acknowledgement
                    .clone()
                    .map(|ack| (state.owner.clone(), ack))
            });
        let observation = previous.clone();
        let refund = if observation.is_some() {
            tokio::task::spawn_blocking(move || {
                observation
                    .is_some_and(|(_, (path, attempt))| owned_suite_nonexecution(&path, &attempt))
            })
            .await
            .unwrap_or(false)
        } else {
            false
        };
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if refund
            && state.as_ref().is_some_and(|state| {
                previous
                    .as_ref()
                    .is_some_and(|(owner, _)| state.owner == *owner)
            })
        {
            *state = None;
        }
    }

    pub fn reserved_for(&self, owner: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|state| state.owner == owner && !state.started)
    }

    pub fn started_for(&self, owner: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|state| state.owner == owner && state.started)
    }

    pub fn start(
        &self,
        owner: &str,
        acknowledgement: Option<(PathBuf, String)>,
    ) -> Result<(), String> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(state) = state
            .as_mut()
            .filter(|state| state.owner == owner && !state.started)
        else {
            return Err(BUDGET_EXHAUSTED.into());
        };
        state.started = true;
        state.acknowledgement = acknowledgement;
        Ok(())
    }
}

impl Drop for FullSuitePermit {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .as_ref()
            .is_some_and(|state| state.owner == self.owner && !state.started)
        {
            *state = None;
        }
    }
}

/// This is a private, first-write-wins owner channel, not stdout or exit 75.
pub fn owned_suite_nonexecution(path: &Path, attempt: &str) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(4097).read_to_end(&mut bytes).is_err() || bytes.len() > 4096 {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|record| {
        record["version"] == 1 && record["attempt"] == attempt && record["status"] == "not_started"
    })
}

#[cfg(test)]
mod permit_lifecycle_tests {
    use super::*;

    #[test]
    fn prelaunch_rejection_releases_but_possible_start_never_does() {
        let budget = Arc::new(FullSuiteBudget::default());
        let rejected = budget.reserve(true, "rejected".into()).unwrap();
        assert!(budget.reserve(true, "competitor".into()).is_err());
        drop(rejected);
        let started = budget.reserve(true, "started".into()).unwrap();
        budget.start("started", None).unwrap();
        drop(started);
        assert!(budget.reserve(true, "cancelled-or-failed".into()).is_err());
        assert!(budget.reserve(false, "focused".into()).unwrap().is_none());
    }

    #[tokio::test]
    async fn only_exact_owner_nonexecution_can_release_a_dispatched_wrapper() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ack.json");
        let budget = Arc::new(FullSuiteBudget::default());
        let permit = budget.reserve(true, "wrapper".into()).unwrap();
        budget
            .start("wrapper", Some((path.clone(), "nonce".into())))
            .unwrap();
        drop(permit);
        // Each rejected acknowledgement must survive a refresh; without one
        // these reservations would fail regardless of the record.
        budget.refresh_owned_nonexecution().await;
        assert!(budget.reserve(true, "missing-ack".into()).is_err());
        std::fs::write(
            &path,
            r#"{"version":1,"attempt":"other","status":"not_started"}"#,
        )
        .unwrap();
        budget.refresh_owned_nonexecution().await;
        assert!(budget.reserve(true, "wrong-owner".into()).is_err());
        std::fs::write(
            &path,
            r#"{"version":1,"attempt":"nonce","status":"started"}"#,
        )
        .unwrap();
        budget.refresh_owned_nonexecution().await;
        assert!(budget.reserve(true, "counterfeit-busy".into()).is_err());
        std::fs::write(
            &path,
            r#"{"version":1,"attempt":"nonce","status":"not_started"}"#,
        )
        .unwrap();
        budget.refresh_owned_nonexecution().await;
        let retry = budget.reserve(true, "retry".into()).unwrap();
        budget.start("retry", None).unwrap();
        drop(retry);
        assert!(budget.reserve(true, "second-start".into()).is_err());
    }

    #[test]
    fn racing_reservations_allow_exactly_one_possible_start() {
        let budget = Arc::new(FullSuiteBudget::default());
        let barrier = std::sync::Barrier::new(8);
        let starts = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|index| {
                    let budget = Arc::clone(&budget);
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let owner = index.to_string();
                        if let Ok(permit) = budget.reserve(true, owner.clone()) {
                            budget.start(&owner, None).unwrap();
                            drop(permit);
                            1
                        } else {
                            0
                        }
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(starts, 1);
    }
}

pub async fn check_full_suite_command(
    command: &[String],
    shell_type: Option<ShellType>,
    local_cwd: Option<&Path>,
) -> Result<bool, String> {
    let command = command.to_vec();
    let local_cwd = local_cwd.map(Path::to_path_buf);
    crate::run_blocking_command_analysis(move || {
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
        // Execution adds this exact UTF-8 prelude to PowerShell scripts. It
        // configures output, not launch count; inspect the original script.
        let normalized_command = match shell_type {
            ShellType::PowerShell => codex_shell_command::powershell::extract_powershell_command(
                command,
            )
            .and_then(|(shell, script)| {
                script
                    .trim_start()
                    .strip_prefix(codex_shell_command::powershell::UTF8_OUTPUT_PREFIX)
                    .map(|script| vec![shell.to_owned(), "-Command".into(), script.to_owned()])
            }),
            _ => None,
        };
        let command = normalized_command.as_deref().unwrap_or(command);
        let commands = argv_commands(command, Some(shell_type));
        let Some(commands) = commands else {
            // Fail closed for visibly test-bearing opaque scripts, not for all
            // scripting. This is deliberately not an OS subprocess sandbox.
            let visible = match shell_type {
                ShellType::Bash | ShellType::Sh | ShellType::Zsh => {
                    command.last().is_some_and(|script| {
                        codex_shell_command::bash::possible_command_argvs(script)
                            .iter()
                            .any(|argv| visibly_runs_test_argv(argv, 0))
                    })
                }
                _ => command.iter().any(|arg| visibly_runs_tests(arg)),
            };
            if visible {
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
                codex_shell_command::powershell::is_standalone_powershell_command(script)
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
    if matches!(program.as_str(), "env" | "command" | "time" | "exec" | "rustup") {
        use codex_shell_command::validation::ExecutionWrapper;
        return match codex_shell_command::validation::decode_execution_wrapper(&program, args) {
            ExecutionWrapper::Child(start) => check_command(&args[start..], None, depth + 1),
            ExecutionWrapper::NonExecution | ExecutionWrapper::Opaque => Ok(false),
        };
    }
    if matches!(program.as_str(), "python" | "python3" | "py") {
        let Some(index) = codex_shell_command::validation::python_script_index(args) else {
            return Ok(false);
        };
        if basename(&args[index]) == "rust_build_status.py"
            && args.get(index + 1).is_some_and(|arg| arg == "run-lane")
            && let Some(separator) = args[index + 1..].iter().position(|arg| arg == "--")
        {
            return check_command(&args[index + separator + 2..], None, depth + 1);
        }
        if basename(&args[index]) == "rust_test_runner.py"
            && let Some(run) = args[index + 1..].iter().position(|arg| arg == "run-target")
        {
            return check_test_args(&args[index + run + 2..], 1, false);
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
            .is_some_and(|arg| matches!(arg.as_str(), "test" | "t" | "nextest"))
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
        if let Some(child) = codex_shell_command::validation::just_lane_child(&args[index..]) {
            return check_command(child, None, depth + 1);
        }
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
            "--dry-run" | "-n" | "--show" | "--list" | "--summary" | "--help" | "--version"
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

/// Source-owned possible launches; this is not proof that a leaf executes.
fn visibly_runs_tests(script: &str) -> bool {
    let commands = super::possible_powershell_commands(script);
    if commands.iter().any(|argv| visibly_runs_test_argv(argv, 0)) {
        return true;
    }
    // Invoke-Expression of a variable or pipeline input evaluates source that
    // is not its literal argument. Quoted script data may then be that source,
    // so quoting test command text must not hide the launch.
    commands.iter().any(|argv| evaluates_dynamic_source(argv))
        && commands.iter().flatten().any(|word| {
            super::possible_powershell_commands(word)
                .iter()
                .any(|argv| visibly_runs_test_argv(argv, 1))
        })
}

fn evaluates_dynamic_source(argv: &[String]) -> bool {
    let Some((program, rest)) = argv.split_first() else {
        return false;
    };
    matches!(basename(program).as_str(), "iex" | "invoke-expression")
        && (rest.iter().all(|arg| arg.starts_with('-')) || rest.iter().any(|arg| arg.contains('$')))
}

fn visibly_runs_test_argv(argv: &[String], depth: usize) -> bool {
    if depth > 4 {
        return true;
    }
    let Some((program, rest)) = argv.split_first() else {
        return false;
    };
    match basename(program).as_str() {
        "cargo" => matches!(
            codex_shell_command::validation::classify_argv(program, rest),
            codex_shell_command::validation::ValidationClassification::Validation { leaves, .. }
                if leaves.iter().any(|leaf| leaf.operation == codex_shell_command::validation::ValidationOperation::Test)
        ),
        "just" => just_recipe_index(rest).is_some_and(|recipe| {
            rest[recipe].starts_with("core-test") || rest[recipe].starts_with("test")
        }),
        "env" | "command" | "time" | "exec" | "rustup" => {
            use codex_shell_command::validation::ExecutionWrapper;
            use codex_shell_command::validation::decode_execution_wrapper;
            match decode_execution_wrapper(&basename(program), rest) {
                ExecutionWrapper::Child(index) => visibly_runs_test_argv(&rest[index..], depth + 1),
                ExecutionWrapper::NonExecution | ExecutionWrapper::Opaque => false,
            }
        }
        "iex" | "invoke-expression" => rest.iter().any(|source| {
            super::possible_powershell_commands(source)
                .iter()
                .any(|argv| visibly_runs_test_argv(argv, depth + 1))
        }),
        _ => {
            let runner = if basename(program) == "rust_test_runner.py" {
                Some(0)
            } else if matches!(basename(program).as_str(), "python" | "python3" | "py") {
                codex_shell_command::validation::python_script_index(rest)
                    .filter(|index| basename(&rest[*index]) == "rust_test_runner.py")
                    .map(|index| index + 1)
            } else {
                None
            };
            let Some(runner) = runner else {
                return false;
            };
            let args = &argv[runner + 1..];
            if matches!(args, [command, help] if matches!(command.as_str(), "run-target" | "run-gate" | "check-gates") && matches!(help.as_str(), "--help" | "-h"))
            {
                return false;
            }
            args.iter()
                .any(|arg| matches!(arg.as_str(), "run-target" | "run-gate" | "check-gates"))
        }
    }
}

fn check_test_args(args: &[String], mut positionals: usize, nextest: bool) -> Result<bool, String> {
    let runner_all = positionals > 0;
    let mut libtest = false;
    let mut focused = false;
    let mut explicit_all = false;
    let mut no_fail_fast = false;
    let mut zero_retries = false;
    let mut conflict = false;
    let mut unknown_arity = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            libtest = !nextest;
            index += 1;
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(key, value)| (key, Some(value)));
        if matches!(
            flag,
            "--help" | "-h" | "--version" | "--no-run" | "--list" | "--dry-run"
        ) {
            return Ok(false);
        }
        let takes_value = if libtest {
            matches!(
                flag,
                "--format"
                    | "--color"
                    | "--logfile"
                    | "--test-threads"
                    | "--skip"
                    | "--shuffle-seed"
            )
        } else {
            matches!(
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
            )
        };
        let value = if takes_value {
            inline.or_else(|| args.get(index + 1).map(String::as_str))
        } else {
            inline
        };
        // Unknown options may own the next bare word. They cannot manufacture
        // a focused exception to the full-suite budget.
        if arg.starts_with('-')
            && !takes_value
            && inline.is_none()
            && !matches!(
                flag,
                "--all"
                    | "--all-tests"
                    | "--workspace"
                    | "--no-fail-fast"
                    | "--nff"
                    | "--fail-fast"
                    | "--ff"
                    | "--lib"
                    | "--bins"
                    | "--tests"
                    | "--benches"
                    | "--examples"
                    | "--all-targets"
                    | "--doc"
                    | "--release"
                    | "--locked"
                    | "--frozen"
                    | "--offline"
                    | "--all-features"
                    | "--no-default-features"
                    | "-q"
                    | "--quiet"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "--exact"
                    | "--ignored"
                    | "--include-ignored"
                    | "--nocapture"
                    | "--show-output"
                    | "--shuffle"
                    | "--exclude-should-panic"
                    | "--report-time"
                    | "--ensure-time"
                    | "--bench"
                    | "--test"
            )
        {
            unknown_arity = true;
        }
        match flag {
            "--all" if runner_all => explicit_all = true,
            "--all-tests" => explicit_all = true,
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
    if focused && !explicit_all && !unknown_arity {
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
    fn audit148_aliases_transports_and_previews_share_launch_semantics() {
        for command in [
            "cargo t --no-fail-fast",
            "time -p cargo t --no-fail-fast",
            "env MODE=test cargo test --no-fail-fast",
            "exec cargo test --no-fail-fast",
            "just cargo-lane core-tests cargo nextest run --no-fail-fast --retries 0",
            "just cargo-lane-isolated-home core-tests cargo test --no-fail-fast",
        ] {
            assert_eq!(check_command(&words(command), None, 0), Ok(true), "{command}");
        }
        for command in [
            "cargo t --no-run",
            "cargo test -- --list",
            "just -n core-test-fast core_lib --all --no-fail-fast",
            "just --dry-run core-test-fast core_lib --all --no-fail-fast",
            "just cargo-lane core-tests cargo check --tests",
            "just cargo-lane core-tests cargo test --no-run",
            "python -c preview rust_test_runner.py run-target core_lib --all --no-fail-fast",
            "python -m unrelated rust_test_runner.py run-target core_lib --all --no-fail-fast",
        ] {
            assert_eq!(check_command(&words(command), None, 0), Ok(false), "{command}");
        }
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
            (
                "cargo nextest run --workspace --profile local --no-fail-fast --retries 0",
                true,
            ),
            (
                "python -B scripts/build/rust_build_status.py run-lane --lane core-tests -- cargo nextest run --workspace --profile local --no-fail-fast --retries 0",
                true,
            ),
            ("cargo nextest run --workspace --retries 0", false),
            ("cargo test --no-fail-fast | Out-Null", false),
            (
                "try { [Console]::OutputEncoding=[System.Text.Encoding]::UTF8 } catch { cargo test --no-fail-fast }; cargo test --no-fail-fast",
                false,
            ),
        ] {
            // Shell execution adds this prefix before either handler checks
            // admission. It must not turn a single launch into a compound one.
            let command = codex_shell_command::powershell::prefix_powershell_script_with_utf8(&[
                "pwsh".into(),
                "-NoProfile".into(),
                "-Command".into(),
                script.into(),
            ]);
            for command in [
                vec![
                    "pwsh".into(),
                    "-NoProfile".into(),
                    "-Command".into(),
                    script.into(),
                ],
                command,
            ] {
                let result =
                    check_full_suite_command(&command, Some(ShellType::PowerShell), None).await;
                if allowed {
                    assert_eq!(result, Ok(true), "{command:?}");
                } else {
                    assert!(result.is_err(), "{command:?}: {result:?}");
                }
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
}

#[cfg(test)]
mod audit36_suite_regressions {
    use super::*;

    #[test]
    fn wrapper_and_option_values_do_not_change_suite_scope() {
        for command in [
            "env -u NAME cargo test --no-fail-fast",
            "env --unset=NAME cargo test --no-fail-fast",
            "env -C somewhere cargo test --no-fail-fast",
            "cargo test --no-fail-fast -- --format pretty",
            "cargo test --no-fail-fast -- --format=pretty --color never",
        ] {
            let argv = command
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert_eq!(check_command(&argv, None, 0), Ok(true), "{command}");
        }
        for command in [
            "cargo test --all one_test",
            "cargo test --workspace one_test",
            "env --version cargo test",
            "command -v cargo test",
        ] {
            let argv = command
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert_eq!(check_command(&argv, None, 0), Ok(false), "{command}");
        }
        let budget = FullSuiteBudget::default();
        budget.admit(true).unwrap();
        assert!(budget.admit(true).is_err());
        assert!(budget.admit(false).is_ok());
    }

    #[test]
    fn comments_and_quoted_examples_do_not_launch_tests() {
        for (shell, flag, script) in [
            ("bash", "-c", "echo harmless # cargo test"),
            ("bash", "-c", "cat <<'EOF'\ncargo test\nEOF"),
            (
                "pwsh",
                "-Command",
                "$example='example; cargo test'; Write-Output $example",
            ),
        ] {
            assert_eq!(
                check_command(&[shell.into(), flag.into(), script.into()], None, 0),
                Ok(false),
                "{script}"
            );
        }
        for script in ["for x in 1 2; do cargo test; done", "echo $(cargo test)"] {
            assert!(check_command(&["bash".into(), "-c".into(), script.into()], None, 0).is_err());
        }
    }
}

#[cfg(test)]
mod audit120_suite_regressions {
    use super::*;

    #[test]
    fn output_options_and_rustup_do_not_create_focused_exceptions() {
        for wrapper in [
            "",
            "env -u RUST_LOG ",
            "rustup run stable ",
            "rustup run --install stable ",
            "rustup --quiet run stable -- ",
        ] {
            for output in [
                "--format pretty",
                "--format=pretty",
                "--logfile output.log",
                "--logfile=output.log",
            ] {
                let argv = format!("{wrapper}cargo test --no-fail-fast -- {output}")
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                assert_eq!(check_command(&argv, None, 0), Ok(true), "{argv:?}");
                let without_flags = argv
                    .iter()
                    .filter(|arg| *arg != "--no-fail-fast")
                    .cloned()
                    .collect::<Vec<_>>();
                assert!(
                    check_command(&without_flags, None, 0).is_err(),
                    "{without_flags:?}"
                );
                let mut focused = without_flags;
                focused.push("real_test_name".into());
                assert_eq!(check_command(&focused, None, 0), Ok(false), "{focused:?}");
            }
        }
    }
}
