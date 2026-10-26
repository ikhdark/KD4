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
    match invocation {
        CommandInvocation::Argv { program, args } => classify_argv(program, args),
        CommandInvocation::Script(script) => {
            codex_shell_command::validation::classify_script(script)
        }
        CommandInvocation::PowerShellScript(script) => classify_powershell_script(script),
    }
}

pub fn classify_validation_script(script: &str) -> ValidationClassification {
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




pub fn classify_validation_invocations(
    invocations: &[CommandInvocation],
) -> ValidationClassification {
    combine_validation_classifications(invocations.iter().map(classify_validation))
}




