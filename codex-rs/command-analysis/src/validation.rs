use crate::command_shape::CommandInvocation;

pub use codex_shell_command::validation::ValidationClassification;
pub use codex_shell_command::validation::ValidationOperation;
pub use codex_shell_command::validation::ValidationPolicyClassification;
use codex_shell_command::validation::classify_argv;
use codex_shell_command::validation::classify_powershell_script;
use codex_shell_command::validation::combine_validation_classifications;

/// Launch-time metadata, retained with the process through polling. Repository
/// trust is never deserialized from model arguments or inferred from stdout.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandValidation {
    pub execution_context: Option<ValidationExecutionContext>,
    pub declared: Option<codex_protocol::validation::ValidationCommandContext>,
    pub classification: ValidationClassification,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ValidationExecutionContext {
    pub environment_id: String,
    pub cwd: Option<std::path::PathBuf>,
    /// Exact owner-resolved argv (including any shell wrapper). Do not infer
    /// shell/argv equivalence from display text or overlapping path scopes.
    pub command: Vec<String>,
    pub environment_fingerprint: String,
}



impl CommandValidation {
    pub fn is_validation(&self) -> bool {
        matches!(
            self.classification,
            ValidationClassification::Validation { .. }
        )
    }

    pub fn is_test(&self) -> bool {
        matches!(&self.classification, ValidationClassification::Validation { leaves, .. }
            if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test
                && leaf.mode == codex_shell_command::validation::ValidationExecutionMode::Execution))
    }

    pub fn signal(&self) -> serde_json::Value {
        let modes = match &self.classification {
            ValidationClassification::Validation { leaves, .. } => {
                leaves.iter().map(|leaf| leaf.mode).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        serde_json::json!({
            "validation": self.is_validation(),
            "tests": self.is_test(),
            "execution_modes": modes,
            "execution_context": self.execution_context,
        })
    }
}

#[cfg(test)]
#[test]
fn validation_signals_require_executing_or_checking_modes() {
    for (command, _proof, tests, mode) in [
        ("cargo test", true, true, "execution"),
        ("cargo bench --no-run", true, false, "compilation_only"),
        ("cargo check --tests", true, false, "checking"),
        ("cargo test --no-run", true, false, "compilation_only"),
        ("cargo test -- --list", false, false, "listing"),
        ("cargo test -- --help", false, false, "listing"),
        ("cargo test -- -h", false, false, "listing"),
        ("cargo fuzz list", false, false, "listing"),
        ("cargo fuzz run target", true, false, "execution"),
        ("cargo fmt", false, false, "mutation"),
        ("cargo fmt --check", true, false, "checking"),
        ("just --dry-run test", false, false, "dry_run"),
        (
            "cargo nextest run --no-run",
            true,
            false,
            "compilation_only",
        ),
    ] {
        let validation = CommandValidation {
            execution_context: None,
            declared: None,
            classification: classify_validation_script(command),
        };
        let signal = validation.signal();
        assert!(signal.get("proof").is_none(), "{command}");
        assert_eq!(signal["tests"], tests, "{command}");
        assert_eq!(
            signal["execution_modes"],
            serde_json::json!([mode]),
            "{command}"
        );
    }
}

#[cfg(test)]
#[test]
fn syntax_only_shell_wrappers_do_not_prove_validation() {
    // POSIX `-n` reads commands without executing them. A successful syntax
    // check of a validation command is not execution of that validation.
    for program in ["sh", "bash"] {
        for (args, _proof) in [
            (vec!["-n", "-c", "cargo check"], false),
            (vec!["-nc", "cargo check"], false),
            (vec!["-c", "-n", "cargo check"], false),
            (vec!["-o", "noexec", "-c", "cargo check"], false),
            (vec!["-c", "cargo check"], true),
            (vec!["-lc", "cargo check"], true),
            (vec!["-n", "+n", "-c", "cargo check"], true),
            (vec!["-n", "-c", "+n", "cargo check"], true),
        ] {
            let invocation = CommandInvocation::Argv {
                program: program.to_string(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
            };
            let validation = CommandValidation {
                execution_context: None,
                declared: None,
                classification: classify_validation(&invocation),
            };
            assert!(validation.signal().get("proof").is_none(), "{program} {args:?}");
        }
    }
}


#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryRunners {
    version: u32,
    runners: Vec<codex_shell_command::validation::RepositoryRunner>,
}

type Runners = Vec<codex_shell_command::validation::RepositoryRunner>;
type RunnerCacheSlot = std::sync::Arc<std::sync::OnceLock<Option<Runners>>>;

#[derive(Default)]
struct RepositoryRunnerCache {
    entries:
        std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, String), RunnerCacheSlot>>,
}

impl RepositoryRunnerCache {
    fn get_or_load(
        &self,
        root: &std::path::Path,
        oid: &str,
        load: impl FnOnce() -> Option<Runners>,
    ) -> Runners {
        let key = (root.to_path_buf(), oid.to_string());
        let slot = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !entries.contains_key(&key) && entries.len() >= 64 {
                // Never evict an in-flight key: its waiters must coalesce.
                entries.retain(|_, slot| std::sync::Arc::strong_count(slot) > 1);
                if entries.len() >= 64 {
                    return Vec::new();
                }
            }
            std::sync::Arc::clone(entries.entry(key.clone()).or_default())
        };
        // Only same-key callers wait here. No process or parsing work holds
        // the global map lock. None remains retryable; Some(empty) is negative.
        let cached = slot.get_or_init(load).clone();
        if cached.is_none() {
            // Current waiters share this failed lookup too, but a later request
            // gets a fresh attempt. Never remove a replacement slot.
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if entries
                .get(&key)
                .is_some_and(|current| std::sync::Arc::ptr_eq(current, &slot))
            {
                entries.remove(&key);
            }
        }
        cached.unwrap_or_default()
    }
}

pub fn repository_runners(cwd: &std::path::Path) -> Runners {
    static CACHE: std::sync::OnceLock<RepositoryRunnerCache> = std::sync::OnceLock::new();
    let Some(root) = codex_git_utils::get_git_repo_root(cwd) else {
        return Vec::new();
    };
    let Some(oid) = repository_head_oid(&root) else {
        return Vec::new();
    };
    let mut runners = CACHE
        .get_or_init(Default::default)
        .get_or_load(&root, &oid, || load_repository_runners(&root, &oid));
    for runner in &mut runners {
        runner.path_context = Some((root.clone(), cwd.to_path_buf()));
    }
    runners
}

const MAX_RUNNER_DECLARATION_BYTES: usize = 64 * 1024;

fn parse_repository_runners(bytes: &[u8]) -> Runners {
    let Ok(config) = serde_json::from_slice::<RepositoryRunners>(bytes) else {
        return Vec::new();
    };
    if config.version != 1
        || config.runners.len() > 256
        || config
            .runners
            .iter()
            .any(|runner| runner.options.values().any(|count| *count > 2))
        || config.runners.iter().any(|runner| {
            runner
                .passthrough_after
                .as_ref()
                .is_some_and(|separator| separator.is_empty() || runner.receipt_runner.is_some())
                || runner.passthrough_args.is_some_and(|skip| {
                    skip == 0 || skip > 16 || runner.receipt_runner.is_some()
                        || runner.passthrough_after.is_some()
                })
        })
    {
        return Vec::new();
    }
    config.runners
}

fn load_repository_runners(root: &std::path::Path, oid: &str) -> Option<Runners> {
    // Batch mode distinguishes immutable absence (a successful "missing"
    // response) from execution failures, without a second Git invocation.
    let object = format!("{oid}:.codex/test-runners.json");
    let mut command = std::process::Command::new(codex_git_utils::git_executable());
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch-command"])
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    let (success, bytes) = bounded_runner_lookup(
        command,
        format!("info {oid}\ncontents {object}\n").as_bytes(),
        std::time::Duration::from_secs(5),
    )?;
    // First prove the commit exists. A missing/corrupt commit object is a
    // retryable lookup failure, not proof that its optional declaration is absent.
    let newline = bytes.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&bytes[..newline]).ok()?;
    let fields = header.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || fields[0] != oid || fields[1] != "commit" {
        return None;
    }
    // `info` proves the commit's type/existence without transferring its body.
    // Commit messages have no relationship to the declaration's size limit.
    fields[2].parse::<usize>().ok()?;
    let bytes = bytes.get(newline + 1..)?;
    let newline = bytes.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&bytes[..newline]).ok()?;
    if header == format!("{object} missing") {
        return success.then(Vec::new);
    }
    let fields = header.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let size = fields[2].parse::<usize>().ok()?;
    // A valid object header proves an immutable invalid declaration even when
    // the bounded reader intentionally kills Git before the body completes.
    if size > MAX_RUNNER_DECLARATION_BYTES {
        return Some(Vec::new());
    }
    if !success {
        return None;
    }
    if fields[1] != "blob" {
        return Some(Vec::new());
    }
    let body = bytes.get(newline + 1..)?;
    if body.len() != size + 1 || body.last() != Some(&b'\n') {
        return None;
    }
    Some(parse_repository_runners(&body[..size]))
}

/// Runs only Git plumbing (no hooks, filters or shell). The owning blocking
/// worker retains/reaps the child even if its async caller is cancelled.
fn bounded_runner_lookup(
    mut command: std::process::Command,
    input: &[u8],
    timeout: std::time::Duration,
) -> Option<(bool, Vec<u8>)> {
    use std::io::Read;
    use std::io::Write;
    use std::process::Stdio;
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    std::thread::scope(|scope| {
        let deadline = std::time::Instant::now() + timeout;
        let mut child = ChildGuard(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );
        child.0.stdin.take()?.write_all(input).ok()?;
        let stdout = child.0.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        // Commit metadata is one header; only the declaration has a body.
        let limit = MAX_RUNNER_DECLARATION_BYTES + 1024;
        scope.spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = tx.send(result);
        });
        let bytes = rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .ok()?
            .ok()?;
        if bytes.len() > limit {
            return Some((false, bytes));
        }
        loop {
            if let Some(status) = child.0.try_wait().ok()? {
                return Some((status.success(), bytes));
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // ChildGuard drops before the scope joins its reader, also on timeout.
    })
}

fn repository_head_oid(root: &std::path::Path) -> Option<String> {
    let root = codex_utils_absolute_path::AbsolutePathBuf::try_from(root).ok()?;
    let (git_dir, common_dir) = crate::git_dirs::resolve_git_directories(&root)?;
    let mut value = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    // HEAD often contains a stable symbolic ref across commits. Read its target,
    // including packed refs and linked-worktree common dirs, on every lookup.
    for _ in 0..8 {
        let Some(reference) = value.trim().strip_prefix("ref:").map(str::trim) else {
            let oid = value.trim();
            return ((oid.len() == 40 || oid.len() == 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| oid.to_string());
        };
        if let Ok(next) = std::fs::read_to_string(common_dir.join(reference)) {
            value = next;
            continue;
        }
        if let Ok(packed) = std::fs::read_to_string(common_dir.join("packed-refs"))
            && let Some(oid) = packed.lines().find_map(|line| {
                let (oid, name) = line.split_once(' ')?;
                (name == reference).then(|| oid.to_string())
            })
        {
            value = oid;
            continue;
        }
        // Unborn branches and alternate ref storage (e.g. reftable) remain
        // Git-owned. Ordinary loose/packed refs need no subprocess here.
        let mut command = std::process::Command::new(codex_git_utils::git_executable());
        command
            .arg("-C")
            .arg(root.as_path())
            .args(["rev-parse", "--verify", "HEAD"]);
        let (success, bytes) =
            bounded_runner_lookup(command, &[], std::time::Duration::from_secs(5))?;
        if !success {
            return None;
        }
        value = String::from_utf8(bytes).ok()?;
    }
    None
}

fn classify_with_runners(
    invocation: &CommandInvocation,
    runners: &[codex_shell_command::validation::RepositoryRunner],
) -> ValidationClassification {
    use codex_shell_command::validation as commands;
    match invocation {
        CommandInvocation::Argv { program, args } => {
            commands::classify_argv_with_runners(program, args, runners)
        }
        CommandInvocation::Script(script) => {
            commands::classify_script_with_runners(script, runners)
        }
        CommandInvocation::PowerShellScript(script) => {
            commands::classify_powershell_script_with_runners(script, runners)
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub async fn resolve_command_validation(
    invocation: &CommandInvocation,
    cwd: Option<&std::path::Path>,
    declared: Option<codex_protocol::validation::ValidationCommandContext>,
) -> Option<CommandValidation> {
    resolve_command_validation_with_shell(invocation, None, cwd, declared).await
}

pub async fn resolve_command_validation_with_shell(
    invocation: &CommandInvocation,
    shell_type: Option<codex_shell_command::shell_detect::ShellType>,
    cwd: Option<&std::path::Path>,
    declared: Option<codex_protocol::validation::ValidationCommandContext>,
) -> Option<CommandValidation> {
    resolve_command_launch_analysis_with_shell(invocation, shell_type, cwd, declared)
        .await
        .0
}

/// Keep native syntax work on the analysis worker, including observation policy.
/// Argument decoding and Code Mode preflight must remain structural only.
pub async fn resolve_command_launch_analysis_with_shell(
    invocation: &CommandInvocation,
    shell_type: Option<codex_shell_command::shell_detect::ShellType>,
    cwd: Option<&std::path::Path>,
    declared: Option<codex_protocol::validation::ValidationCommandContext>,
) -> (Option<CommandValidation>, bool) {
    let invocation = match invocation {
        CommandInvocation::Script(script)
            if shell_type == Some(codex_shell_command::shell_detect::ShellType::PowerShell) =>
        {
            CommandInvocation::PowerShellScript(script.clone())
        }
        _ => invocation.clone(),
    };
    let cwd = cwd.map(std::path::Path::to_path_buf);
    crate::run_blocking_command_analysis(move || {
        let runners = cwd.as_deref().map(repository_runners).unwrap_or_default();
        let argv = match &invocation {
            CommandInvocation::Argv { program, args } => Some(
                std::iter::once(program.clone())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>(),
            ),
            CommandInvocation::Script(script) => {
                codex_shell_command::validation::standalone_argv(script)
            }
            CommandInvocation::PowerShellScript(script) => {
                codex_shell_command::validation::standalone_powershell_argv(script).or_else(|| {
                    // Canonical numeric --set values are not direct-execution
                    // proof. Admit them for receipts only through an exact,
                    // committed just runner's constrained variable grammar.
                    let argv =
                        codex_shell_command::powershell::standalone_powershell_inspection_argv(
                            script,
                        )?;
                    let (program, args) = argv.split_first()?;
                    (matches!(program.as_str(), "just" | "just.exe")
                        && runners.iter().any(|runner| {
                            !runner.just_numeric_variables.is_empty()
                                && runner.matches(program, args)
                        }))
                    .then_some(argv)
                })
            }
        };
        // Classification and receipt matching must describe the same literal
        // PowerShell argv; POSIX shlex removes unquoted Windows backslashes.
        let classification = if matches!(invocation, CommandInvocation::PowerShellScript(_))
            && let Some((program, args)) = argv.as_ref().and_then(|argv| argv.split_first())
        {
            codex_shell_command::validation::classify_argv_with_runners(program, args, &runners)
        } else {
            classify_with_runners(&invocation, &runners)
        };
        let long_running = matches!(classification, ValidationClassification::Validation { .. })
            || prefers_long_observation_wait(&invocation);
        let validation = (declared.is_some()
            || matches!(classification, ValidationClassification::Validation { .. }))
        .then_some(CommandValidation {
            execution_context: None,
            declared,
            classification,
        });
        (validation, long_running)
    })
    .await
    .unwrap_or_default()
}

/// Policy classification shares the trusted runner declarations and analysis
/// worker with validation evidence, but never upgrades it into receipt proof.
pub async fn classify_validation_policy_with_shell(
    invocation: &CommandInvocation,
    shell_type: Option<codex_shell_command::shell_detect::ShellType>,
    cwd: Option<&std::path::Path>,
) -> ValidationPolicyClassification {
    analyze_validation_policy_with_shell(invocation, shell_type, cwd).await.0
}

pub async fn analyze_validation_policy_with_shell(
    invocation: &CommandInvocation,
    shell_type: Option<codex_shell_command::shell_detect::ShellType>,
    cwd: Option<&std::path::Path>,
) -> (ValidationPolicyClassification, String) {
    let invocation = match invocation {
        CommandInvocation::Script(script)
            if shell_type == Some(codex_shell_command::shell_detect::ShellType::PowerShell) =>
        {
            CommandInvocation::PowerShellScript(script.clone())
        }
        _ => invocation.clone(),
    };
    let cwd = cwd.map(std::path::Path::to_path_buf);
    crate::run_blocking_command_analysis(move || {
        let runners = cwd.as_deref().map(repository_runners).unwrap_or_default();
        analyze_policy_with_runners(&invocation, &runners)
    })
    .await
    .unwrap_or((ValidationPolicyClassification::Opaque, String::new()))
}

pub fn classify_validation_policy(
    invocation: &CommandInvocation,
) -> ValidationPolicyClassification {
    classify_policy_with_runners(invocation, &[])
}

fn classify_policy_with_runners(
    invocation: &CommandInvocation,
    runners: &[codex_shell_command::validation::RepositoryRunner],
) -> ValidationPolicyClassification {
    analyze_policy_with_runners(invocation, runners).0
}

fn analyze_policy_with_runners(
    invocation: &CommandInvocation,
    runners: &[codex_shell_command::validation::RepositoryRunner],
) -> (ValidationPolicyClassification, String) {
    use codex_shell_command::validation as commands;
    match invocation {
        CommandInvocation::Argv { program, args } => {
            commands::analyze_validation_policy_argv_with_runners(program, args, runners)
        }
        CommandInvocation::Script(script) => {
            commands::analyze_validation_policy_script_with_runners(script, runners, false)
        }
        CommandInvocation::PowerShellScript(script) => {
            commands::analyze_validation_policy_script_with_runners(script, runners, true)
        }
    }
}

pub fn classify_validation(invocation: &CommandInvocation) -> ValidationClassification {
    #[cfg(test)]
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(count.get() + 1));
    match invocation {
        CommandInvocation::Argv { program, args } => classify_argv(program, args),
        CommandInvocation::Script(script) => {
            codex_shell_command::validation::classify_script(script)
        }
        CommandInvocation::PowerShellScript(script) => classify_powershell_script(script),
    }
}

pub fn classify_validation_script(script: &str) -> ValidationClassification {
    #[cfg(test)]
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(count.get() + 1));
    codex_shell_command::validation::classify_script(script)
}

/// Observation policy, not evidence that tests executed. Build and discovery
/// commands can compile for minutes without proving a validation contract.
pub fn prefers_long_observation_wait(invocation: &CommandInvocation) -> bool {
    if matches!(
        classify_validation(invocation),
        ValidationClassification::Validation { .. }
    ) {
        return true;
    }
    match invocation {
        CommandInvocation::Argv { program, args } => {
            codex_shell_command::validation::is_build_or_discovery(program, args)
        }
        CommandInvocation::Script(script) | CommandInvocation::PowerShellScript(script) => {
            codex_shell_command::validation::script_prefers_long_observation_wait(script)
        }
    }
}

#[cfg(test)]
thread_local! {
    static VALIDATION_CLASSIFICATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn reset_validation_classification_count() {
    VALIDATION_CLASSIFICATION_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub fn validation_classification_count() -> usize {
    VALIDATION_CLASSIFICATION_COUNT.with(std::cell::Cell::get)
}

pub fn classify_validation_invocations(
    invocations: &[CommandInvocation],
) -> ValidationClassification {
    combine_validation_classifications(invocations.iter().map(classify_validation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_shell_command::validation::ValidationCommandDescriptor;

    fn argv(program: &str, args: &[&str]) -> CommandInvocation {
        CommandInvocation::Argv {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    fn is_validation(invocation: &CommandInvocation) -> bool {
        matches!(
            classify_validation(invocation),
            ValidationClassification::Validation { .. }
        )
    }

    mod manifest_runner {
        use super::*;

        #[test]
        fn runner_cache_negative_entries_and_failures_have_distinct_lifetimes() {
            let cache = RepositoryRunnerCache::default();
            let root = std::path::Path::new("repo");
            let calls = std::cell::Cell::new(0);
            for oid in ["first", "first", "second", "second"] {
                assert!(
                    cache
                        .get_or_load(root, oid, || {
                            calls.set(calls.get() + 1);
                            Some(parse_repository_runners(b"invalid immutable JSON"))
                        })
                        .is_empty()
                );
            }
            assert_eq!(calls.get(), 2);
            for _ in 0..2 {
                cache.get_or_load(root, "transient", || {
                    calls.set(calls.get() + 1);
                    None
                });
            }
            assert_eq!(calls.get(), 4, "execution failures must remain retryable");
        }

        #[test]
        fn runner_cache_coalesces_one_key_without_blocking_other_repositories() {
            let cache = RepositoryRunnerCache::default();
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let cache = &cache;
                let calls = &calls;
                let first = scope.spawn(move || {
                    cache.get_or_load(std::path::Path::new("slow"), "oid", || {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        started_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap();
                        Some(Vec::new())
                    })
                });
                started_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                let second = scope.spawn(move || {
                    cache.get_or_load(std::path::Path::new("slow"), "oid", || {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Some(Vec::new())
                    })
                });
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
                while !cache
                    .entries
                    .lock()
                    .unwrap()
                    .get(&(std::path::PathBuf::from("slow"), "oid".to_string()))
                    .is_some_and(|slot| std::sync::Arc::strong_count(slot) >= 3)
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "second caller must join the in-flight cache slot"
                    );
                    std::thread::yield_now();
                }
                let start = std::time::Instant::now();
                cache.get_or_load(std::path::Path::new("other"), "oid", || Some(Vec::new()));
                let elapsed = start.elapsed();
                release_tx.send(()).unwrap();
                assert!(first.join().unwrap().is_empty());
                assert!(second.join().unwrap().is_empty());
                assert!(elapsed < std::time::Duration::from_secs(1));
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            });
        }

        #[test]
        fn runner_declaration_absence_invalid_content_and_revision_are_cached() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let git = |args: &[&str]| {
                let output = std::process::Command::new(codex_git_utils::git_executable())
                    .arg("-C")
                    .arg(root)
                    .args([
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.com",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init"]);
            git(&["commit", "--allow-empty", "-m", "missing"]);
            let cache = RepositoryRunnerCache::default();
            let calls = std::cell::Cell::new(0);
            for contents in [None, Some("invalid"), Some(r#"{"version":1,"runners":[]}"#)] {
                if let Some(contents) = contents {
                    std::fs::create_dir_all(root.join(".codex")).unwrap();
                    std::fs::write(root.join(".codex/test-runners.json"), contents).unwrap();
                    git(&["add", ".codex/test-runners.json"]);
                    git(&["commit", "-m", "declaration"]);
                }
                let oid = repository_head_oid(root).unwrap();
                for _ in 0..2 {
                    cache.get_or_load(root, &oid, || {
                        calls.set(calls.get() + 1);
                        let loaded = load_repository_runners(root, &oid);
                        assert!(loaded.as_ref().is_some_and(Vec::is_empty));
                        loaded
                    });
                }
            }
            assert_eq!(calls.get(), 3);
            assert!(load_repository_runners(root, &"0".repeat(40)).is_none());
            std::fs::write(
                root.join(".codex/test-runners.json"),
                vec![b'x'; MAX_RUNNER_DECLARATION_BYTES * 4],
            )
            .unwrap();
            git(&["add", ".codex/test-runners.json"]);
            git(&["commit", "-m", "oversized"]);
            assert!(
                load_repository_runners(root, &repository_head_oid(root).unwrap())
                    .is_some_and(|runners| runners.is_empty())
            );
        }

        #[test]
        fn head_oid_tracks_loose_packed_detached_and_linked_refs() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let git = root.join(".git");
            std::fs::create_dir_all(git.join("refs/heads")).unwrap();
            let first = "a".repeat(40);
            let second = "b".repeat(40);
            std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(git.join("refs/heads/main"), &first).unwrap();
            assert_eq!(repository_head_oid(root), Some(first.clone()));
            std::fs::write(git.join("refs/heads/main"), &second).unwrap();
            assert_eq!(repository_head_oid(root), Some(second.clone()));
            std::fs::remove_file(git.join("refs/heads/main")).unwrap();
            std::fs::write(
                git.join("packed-refs"),
                format!("{first} refs/heads/main\n"),
            )
            .unwrap();
            assert_eq!(repository_head_oid(root), Some(first.clone()));
            std::fs::write(git.join("HEAD"), &second).unwrap();
            assert_eq!(repository_head_oid(root), Some(second));
            let linked = root.join("linked");
            let worktree_git = git.join("worktrees/linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::fs::create_dir_all(&worktree_git).unwrap();
            std::fs::write(linked.join(".git"), "gitdir: ../.git/worktrees/linked\n").unwrap();
            std::fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
            std::fs::write(worktree_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            assert_eq!(repository_head_oid(&linked), Some(first));
        }

        #[test]
        fn runner_cache_uses_committed_revision_and_rebinds_cwd() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args([
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.com",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init"]);
            std::fs::create_dir(root.join(".codex")).unwrap();
            std::fs::create_dir(root.join("nested")).unwrap();
            let manifest = root.join(".codex/test-runners.json");
            let config = |operation| {
                serde_json::json!({"version":1,"runners":[{
                    "programs":["python"],"prefixes":[["scripts/check.py"]],"operations":[operation]
                }]})
                .to_string()
            };
            std::fs::write(&manifest, config("test")).unwrap();
            git(&["add", ".codex/test-runners.json"]);
            // A large valid commit body must not consume the declaration's
            // independent bounded read or turn this immutable key retryable.
            let message = root.join("commit-message.txt");
            std::fs::write(&message, "x".repeat(MAX_RUNNER_DECLARATION_BYTES * 2)).unwrap();
            git(&["commit", "-F", message.to_str().unwrap()]);
            assert_eq!(
                repository_runners(root)[0].operations,
                vec![ValidationOperation::Test]
            );
            std::fs::write(&manifest, config("lint")).unwrap();
            let nested = root.join("nested");
            let cached = repository_runners(&nested);
            assert_eq!(cached[0].operations, vec![ValidationOperation::Test]);
            assert_eq!(cached[0].path_context, Some((root.to_path_buf(), nested)));
            git(&["add", ".codex/test-runners.json"]);
            git(&["commit", "-m", "second"]);
            assert_eq!(
                repository_runners(root)[0].operations,
                vec![ValidationOperation::Lint]
            );
        }


        fn classify_validation(invocation: &CommandInvocation) -> ValidationClassification {
            let runner = serde_json::from_value(serde_json::json!({
                "programs": ["python", "py"],
                "prefixes": [["scripts/validate.py", "run"]],
                "options": {"-I": 0},
                "allow_extra_args": true,
                "operations": ["test"]
            }))
            .unwrap();
            classify_with_runners(invocation, &[runner])
        }

        fn is_validation(invocation: &CommandInvocation) -> bool {
            matches!(
                classify_validation(invocation),
                ValidationClassification::Validation { .. }
            )
        }

        #[test]
        fn passthrough_runner_classifies_only_the_child_and_cannot_claim_receipts() {
            let mut runner: codex_shell_command::validation::RepositoryRunner =
                serde_json::from_value(serde_json::json!({
                    "programs": ["python"],
                    "prefixes": [["scripts/build/rust_build_status.py", "run-lane"]],
                    "allow_extra_args": true,
                    "passthrough_after": "--"
                }))
                .unwrap();
            for (child, expected) in [
                (vec!["cargo", "test", "-p", "example"], true),
                (vec!["cargo", "check"], true),
                (vec!["cargo", "build"], false),
                (vec!["cargo", "test", "--help"], false),
                (vec!["echo", "cargo", "test"], false),
                (vec![], false),
            ] {
                let mut args = vec![
                    "scripts/build/rust_build_status.py",
                    "run-lane",
                    "--lane",
                    "core-tests",
                    "--",
                ];
                args.extend(child);
                let invocation = argv("python", &args);
                assert_eq!(
                    matches!(
                        classify_with_runners(&invocation, &[runner.clone()]),
                        ValidationClassification::Validation { .. }
                    ),
                    expected,
                    "{args:?}"
                );
            }
            runner.receipt_runner = Some("rust_test_runner".into());
            assert!(
                !runner.matches(
                    "python",
                    &[
                        "scripts/build/rust_build_status.py",
                        "run-lane",
                        "--",
                        "cargo",
                        "test",
                    ]
                    .map(str::to_string)
                )
            );
        }

        #[test]
        fn manifest_validation_runner_recognizes_only_execution() {
            for invocation in [
                argv("python", &["scripts/validate.py", "run", "atomic-write"]),
                argv(
                    "python",
                    &[
                        "-I",
                        "scripts/validate.py",
                        "run",
                        "--changed",
                        "src/lib.rs",
                    ],
                ),
                argv("py", &["scripts/validate.py", "run"]),
                CommandInvocation::Script(
                    concat!(
                        "python scripts/validate.py plan atomic-write; ",
                        "python scripts/validate.py run atomic-write",
                    )
                    .to_string(),
                ),
            ] {
                assert!(
                    matches!(
                        classify_validation(&invocation),
                        ValidationClassification::Validation { ref leaves, .. }
                            if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                    ),
                    "{invocation:?}"
                );
            }
            for args in [
                vec!["scripts/validate.py"],
                vec!["scripts/validate.py", "plan", "atomic-write"],
                vec!["scripts/validate.py", "list"],
                vec!["scripts/validate.py", "--help"],
                vec!["scripts/validate.py", "run", "--help"],
                vec!["scripts/not_validate.py", "run"],
                vec!["script.py", "scripts/validate.py", "run"],
            ] {
                assert!(!is_validation(&argv("python", &args)), "{args:?}");
            }
        }

        #[test]
        fn guarded_manifest_runs_from_rollout_are_validation() {
            // Exact `exec_command` scripts a recorded session sent.
            let guarded_runs = [
                concat!(
                    "python scripts/validate.py plan validation-tooling; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run validation-tooling }; ",
                    "exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py plan --changed src/atomic_write.rs; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run --changed src/atomic_write.rs }; ",
                    "exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py run --changed src/atomic_write.rs; ",
                    "if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }; ",
                    "rg -n '^mod tests|^    mod tests' src/config/mod.rs src/workspace.rs src/test_plan.rs ",
                    "src/validation_identity.rs src/execution_control.rs src/analysis_jobs.rs; ",
                    "git diff --check -- .cargo/config.toml .config/nextest.toml ",
                    "scripts/check_test_targets.py AGENTS.md; exit $LASTEXITCODE",
                ),
                concat!(
                    "python scripts/validate.py plan validation-tooling atomic-write; ",
                    "if ($LASTEXITCODE -eq 0) { python scripts/validate.py run validation-tooling atomic-write }; ",
                    "exit $LASTEXITCODE",
                ),
            ];

            for script in guarded_runs {
                let invocation = CommandInvocation::Script(script.to_string());
                assert!(
                    matches!(
                        classify_validation(&invocation),
                        ValidationClassification::Validation {
                            ref leaves,
                            exit_code_is_authoritative: false,
                            ..
                        } if !leaves.is_empty()
                            && leaves.iter().all(|leaf| leaf.operation == ValidationOperation::Test)
                    ),
                    "{script}"
                );
            }
            for script in [
                concat!(
                    "python scripts/validate.py plan --changed src/validation_identity.rs ",
                    "--changed tests/config_runtime_contracts.rs --changed src/atomic_write.rs",
                ),
                "if ($LASTEXITCODE -eq 0) { python scripts/validate.py plan atomic-write }",
            ] {
                assert_eq!(
                    classify_validation(&CommandInvocation::Script(script.to_string())),
                    ValidationClassification::NonValidation,
                    "{script}"
                );
            }
        }

        #[test]
        fn powershell_if_blocks_keep_their_commands_together() {
            for script in [
                "if ($LASTEXITCODE -eq 0) { python scripts/validate.py plan a; python scripts/validate.py run a }",
                "if (Test-Path Cargo.toml) { echo skip } elseif ($env:CI) { echo ci } else { cargo test }",
                "If($ok){cargo test}",
            ] {
                assert!(
                    is_validation(&CommandInvocation::Script(script.to_string())),
                    "{script}"
                );
            }
            // An unclosed block leaves the command boundaries unknowable.
            assert_eq!(
                classify_validation(&CommandInvocation::Script(
                    "if ($ok) { cargo test".to_string()
                )),
                ValidationClassification::Opaque
            );
        }
    }

    #[test]
    fn python_launchers_and_interpreter_options_recognize_tests() {
        for invocation in [
            argv("py", &["-m", "pytest", "-q"]),
            argv("py.exe", &["-m", "unittest", "discover"]),
            argv("python3.12", &["-I", "-m", "pytest"]),
            argv("python", &["-X", "dev", "-m", "pytest"]),
            argv("python", &["-Xdev", "-Werror", "-m", "unittest"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn python_module_markers_after_a_script_or_double_dash_are_not_tests() {
        for invocation in [
            argv("python", &["script.py", "-m", "pytest"]),
            argv("python", &["--", "-m", "pytest"]),
            argv("python", &["-c", "print('pytest')", "-m", "pytest"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn wrapper_target_with_validation_word_is_opaque() {
        let invocation = argv("make", &["format-report"]);
        assert_eq!(
            classify_validation(&invocation),
            ValidationClassification::Opaque
        );
    }

    #[test]
    fn cargo_global_options_nextest_and_fmt_preserve_validation_operations() {
        for invocation in [
            argv("cargo", &["nextest", "run"]),
            argv("cargo", &["--locked", "test"]),
            argv("cargo", &["--color", "always", "test"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1
                            && leaves[0].operation == ValidationOperation::Test
                ),
                "{invocation:?}"
            );
        }

        let offline_check = argv("cargo", &["--offline", "check"]);
        assert!(matches!(
            classify_validation(&offline_check),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Check
        ));

        for invocation in [
            argv("cargo", &["fmt", "--check"]),
            argv("cargo", &["+nightly", "fmt", "--", "--check"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1
                            && leaves[0].operation == ValidationOperation::Lint
                ),
                "{invocation:?}"
            );
        }

        assert_eq!(
            classify_validation(&argv("cargo", &["--unknown-global", "test"])),
            ValidationClassification::Opaque
        );
    }

    #[test]
    fn parsed_pipeline_stages_preserve_validation_classification() {
        let invocations = [
            argv("cargo", &["test"]),
            argv("Select-Object", &["-First", "1"]),
        ];
        assert!(matches!(
            classify_validation_invocations(&invocations),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
        ));
    }

    #[test]
    fn operation_recognition_does_not_parse_runner_modes() {
        for invocation in [
            argv("task", &["--summary", "test"]),
            argv("go", &["test", "-c"]),
            argv("dotnet", &["test", "--list-tests"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }

        for invocation in [
            argv("npm", &["--help"]),
            argv("just", &["--list"]),
            argv("cargo", &["--version"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn value_taking_node_flags_do_not_hide_the_script_selector() {
        for invocation in [
            argv("npm", &["--prefix", "web", "run", "lint"]),
            argv("pnpm", &["--filter", "api", "test"]),
            argv("yarn", &["--cwd", "web", "test"]),
            argv("yarn", &["workspace", "web", "run", "check"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn forwarded_runner_arguments_are_not_validation_operations() {
        for invocation in [
            argv("cargo", &["run", "--", "test"]),
            argv("dotnet", &["run", "test"]),
            argv("go", &["run", "test"]),
            argv("npm", &["install", "test"]),
            argv("gradle", &["-p", "test", "build"]),
            argv("mvn", &["-f", "test", "package"]),
            argv("make", &["-f", "test", "all"]),
            argv("task", &["--dir", "test", "build"]),
        ] {
            assert!(!is_validation(&invocation), "{invocation:?}");
        }
    }

    #[test]
    fn just_scans_past_options_and_non_validation_recipes() {
        let invocations = [
            argv("just", &["--justfile", "Justfile", "test"]),
            argv("just", &["prepare", "test"]),
        ];

        for invocation in invocations {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.iter().any(|leaf| leaf.operation == ValidationOperation::Test)
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn just_option_values_are_not_recipe_targets() {
        for invocation in [
            argv("just", &["--shell", "bash", "test"]),
            argv("just", &["--color", "always", "test"]),
            argv("just", &["--set", "MODE", "test", "test"]),
        ] {
            assert!(
                matches!(
                    classify_validation(&invocation),
                    ValidationClassification::Validation { ref leaves, .. }
                        if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
                ),
                "{invocation:?}"
            );
        }
    }

    #[test]
    fn simple_commands_are_recognized_without_shell_emulation() {
        let script = CommandInvocation::Script("cargo test".to_string());
        assert!(is_validation(&script));

        let wrapped = argv("env", &["MODE=test", "bash", "-lc", "cargo test"]);
        assert!(is_validation(&wrapped));

        let powershell = CommandInvocation::PowerShellScript(
            "Invoke-Expression -Verbose -Command 'cargo test'".to_string(),
        );
        assert!(!is_validation(&powershell));

        let powershell_pipeline = CommandInvocation::PowerShellScript(
            "cargo test -p codex-core | Select-Object -First 1".to_string(),
        );
        assert!(is_validation(&powershell_pipeline));

        let cmd_for = argv("cmd", &["/c", "for %i in (do) do cargo test"]);
        assert!(!is_validation(&cmd_for));

        let nested = CommandInvocation::Script("echo $(cargo test)".to_string());
        assert_eq!(
            classify_validation(&nested),
            ValidationClassification::Opaque
        );
    }

    #[test]
    fn deterministic_compound_script_recognizes_tests() {
        let invocation =
            CommandInvocation::Script("echo preparing && cargo test --workspace".to_string());
        assert!(matches!(
            classify_validation(&invocation),
            ValidationClassification::Validation { ref leaves, .. }
                if leaves.len() == 1 && leaves[0].operation == ValidationOperation::Test
        ));

        assert_eq!(
            classify_validation(&CommandInvocation::Script(
                "echo 'cargo test && still text'".to_string(),
            )),
            ValidationClassification::NonValidation
        );
    }

    #[test]
    fn incomplete_yarn_workspace_arguments_do_not_panic_or_invent_validation() {
        for args in [
            vec!["workspace"],
            vec!["--cwd", "web", "workspace"],
            vec!["workspace", "web"],
            vec!["workspace", "web", "run"],
        ] {
            assert_eq!(
                classify_validation(&argv("yarn", &args)),
                ValidationClassification::NonValidation,
            );
        }
        assert_eq!(
            classify_validation(&argv("yarn", &["workspace", "web", "run", "check"])),
            ValidationClassification::Validation {
                leaves: vec![ValidationCommandDescriptor {
                    operation: ValidationOperation::Check,
                    mode: codex_shell_command::validation::ValidationExecutionMode::Checking,
                }],
                has_unclassified_targets: true,
                exit_code_is_authoritative: false,
            },
        );
    }

    #[test]
    fn runner_flags_flow_through_operation_recognition() {
        for invocation in [
            argv("cargo", &["test", "--", "--ignored"]),
            argv("pytest", &["--maxfail=1", "tests/unit"]),
            argv("dotnet", &["test", "--blame-hang"]),
            argv("go", &["test", "-run", "Case", "./pkg"]),
        ] {
            assert!(is_validation(&invocation), "{invocation:?}");
        }
    }
}

#[cfg(test)]
mod audit36_validation_regressions {
    use super::*;



}

#[cfg(test)]
#[path = "validation_runner_resolution_tests.rs"]
mod runner_resolution_tests;

#[cfg(test)]
mod validation_policy_tests {
    use super::*;

    #[tokio::test]
    async fn policy_analysis_keeps_scope_and_wrapper_selection_separate_from_proof() {
        let direct = CommandInvocation::Argv {
            program: "cargo".into(),
            args: ["check", "-p", "codex-core", "--tests"].map(str::to_owned).to_vec(),
        };
        let wrapped = CommandInvocation::Script("cargo check -p codex-core --tests".into());
        let direct = analyze_validation_policy_with_shell(&direct, None, None).await;
        let wrapped = analyze_validation_policy_with_shell(&wrapped, None, None).await;
        assert_eq!(direct.0, ValidationPolicyClassification::TargetedCompilation);
        assert_eq!(direct, wrapped);
        assert_eq!(
            classify_validation_policy(&CommandInvocation::Script("cargo fmt --check".into())),
            ValidationPolicyClassification::Checks,
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn policy_analysis_uses_the_actual_powershell_parser() {
        let invocation = CommandInvocation::Script("& cargo test -p codex-core".into());
        assert_eq!(
            classify_validation_policy_with_shell(
                &invocation,
                Some(codex_shell_command::shell_detect::ShellType::PowerShell),
                None,
            ).await,
            ValidationPolicyClassification::Tests,
        );
    }
}
