use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout_at;
use tracing::Span;

use super::CommandShell;
use super::ConfiguredHandler;
use super::dispatcher::hook_execution_mode_label;
use super::dispatcher::hook_handler_type_label;
use super::dispatcher::hook_scope_label;
use super::dispatcher::scope_for_event;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;
#[cfg(windows)]
use codex_utils_pty::WINDOWS_CREATE_SUSPENDED;
#[cfg(windows)]
use codex_utils_pty::run_windows_process_operation;
#[cfg(windows)]
use codex_utils_pty::with_windows_child_creation;

const HOOK_STREAM_CAPTURE_MAX_BYTES: usize = 1024 * 1024;
const HOOK_STREAM_READ_BUFFER_BYTES: usize = 16 * 1024;
const HOOK_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);

struct ContainedProcessTree {
    managed: Arc<codex_utils_pty::ManagedRootProcess>,
    process_group_id: Option<u32>,
    armed: bool,
}

impl ContainedProcessTree {
    fn new(managed: codex_utils_pty::ManagedRootProcess, process_group_id: Option<u32>) -> Self {
        Self {
            managed: Arc::new(managed),
            process_group_id,
            armed: true,
        }
    }

    fn terminate_descendants(&self) -> io::Result<()> {
        #[cfg(windows)]
        self.managed.terminate()?;
        #[cfg(not(windows))]
        let _ = &self.managed;

        if let Some(process_group_id) = self.process_group_id {
            codex_utils_pty::process_group::kill_process_group(process_group_id)?;
        }
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ContainedProcessTree {
    fn drop(&mut self) {
        if self.armed
            && let Err(err) = self.terminate_descendants()
        {
            tracing::warn!("failed to terminate cancelled hook process tree: {err:?}");
        }
    }
}

#[derive(Debug)]
pub(crate) struct CommandRunResult {
    pub started_at: i64,
    pub completed_at: i64,
    pub duration_ms: i64,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

#[tracing::instrument(
    name = "codex.hooks.command",
    level = "trace",
    skip_all,
    fields(
        hook.event_name = handler.event_name.as_pascal_case_label(),
        hook.handler_type = hook_handler_type_label(HookHandlerType::Command),
        hook.execution_mode = hook_execution_mode_label(HookExecutionMode::Sync),
        hook.scope = hook_scope_label(scope_for_event(handler.event_name)),
        hook.source = handler.source.as_snake_case_label(),
        hook.display_order = handler.display_order,
        hook.configured_order = configured_order,
        hook.timeout_sec = handler.timeout_sec,
        hook.command_outcome = tracing::field::Empty,
    )
)]
pub(crate) async fn run_command(
    shell: &CommandShell,
    handler: &ConfiguredHandler,
    configured_order: usize,
    input_json: &str,
    cwd: &Path,
) -> CommandRunResult {
    run_command_with_reservation(
        shell,
        handler,
        input_json,
        cwd,
        codex_utils_pty::ManagedRootProcess::reserve_strict_with_reclaim(),
    )
    .await
}

/// Run one argv-based legacy hook while retaining ownership of its complete process tree.
///
/// The optional timeout covers process admission and execution. Dropping this future also
/// terminates the process tree, so cancellation cannot leave a detached hook descendant alive.
pub(crate) async fn run_contained_command(
    mut command: Command,
    execution_timeout: Option<Duration>,
) -> io::Result<ExitStatus> {
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(WINDOWS_CREATE_SUSPENDED);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(codex_utils_pty::process_group::detach_from_tty);
    }

    let execution_deadline =
        execution_timeout.map(|duration| tokio::time::Instant::now() + duration);
    let run = async {
        let managed = codex_utils_pty::ManagedRootProcess::reserve_strict_with_reclaim().await?;
        #[cfg(windows)]
        let mut child = run_windows_process_operation(
            windows_operation_timeout(execution_deadline),
            move || with_windows_child_creation(|_| command.spawn()),
        )
        .await?;
        #[cfg(not(windows))]
        let mut child = command.spawn()?;
        let process_group_id = child.id();
        let mut process_tree = ContainedProcessTree::new(managed, process_group_id);

        #[cfg(windows)]
        {
            let Some(process_id) = process_group_id else {
                let _ = terminate_command_tree(&mut child, &mut process_tree).await;
                return Err(io::Error::other("spawned hook process has no process id"));
            };
            if let Err(err) =
                attach_and_resume_with_timeout(&process_tree, process_id, execution_deadline).await
            {
                let _ = terminate_command_tree(&mut child, &mut process_tree).await;
                return Err(io::Error::other(format!(
                    "failed to contain hook process: {err}"
                )));
            }
        }

        let status = child.wait().await;
        let containment = terminate_command_tree(&mut child, &mut process_tree).await;
        let status = status?;
        containment?;
        Ok(status)
    };

    match (execution_timeout, execution_deadline) {
        (Some(duration), Some(deadline)) => timeout_at(deadline, run).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("hook timed out after {}s", duration.as_secs()),
            )
        })?,
        (None, None) => run.await,
        _ => unreachable!("execution timeout and deadline must be set together"),
    }
}

async fn run_command_with_reservation(
    shell: &CommandShell,
    handler: &ConfiguredHandler,
    input_json: &str,
    cwd: &Path,
    reservation: impl Future<Output = io::Result<codex_utils_pty::ManagedRootProcess>>,
) -> CommandRunResult {
    let started_at = chrono::Utc::now().timestamp();
    let started = Instant::now();
    let timeout_duration = Duration::from_secs(handler.timeout_sec);
    let timeout_deadline = tokio::time::Instant::now() + timeout_duration;

    let mut command = build_command(shell, handler);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(WINDOWS_CREATE_SUSPENDED);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(codex_utils_pty::process_group::detach_from_tty);
    }

    let managed = match timeout_at(timeout_deadline, reservation).await {
        Ok(Ok(managed)) => managed,
        Ok(Err(err)) => {
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("failed to reserve hook process containment: {err}")),
                    outcome: "spawn_error",
                },
            );
        }
        Err(_) => return finish_timeout(started_at, started, handler.timeout_sec),
    };

    if tokio::time::Instant::now() >= timeout_deadline {
        return finish_timeout(started_at, started, handler.timeout_sec);
    }

    #[cfg(windows)]
    let spawn_timeout = timeout_deadline.saturating_duration_since(tokio::time::Instant::now());
    #[cfg(windows)]
    let mut child = match run_windows_process_operation(spawn_timeout, move || {
        with_windows_child_creation(|_| command.spawn())
    })
    .await
    {
        Ok(child) => child,
        Err(err) if err.kind() == io::ErrorKind::TimedOut => {
            return finish_timeout(started_at, started, handler.timeout_sec);
        }
        Err(err) => {
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                    outcome: "spawn_error",
                },
            );
        }
    };
    #[cfg(not(windows))]
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                    outcome: "spawn_error",
                },
            );
        }
    };

    let process_group_id = child.id();
    let mut process_tree = ContainedProcessTree::new(managed, process_group_id);

    #[cfg(windows)]
    {
        let Some(process_id) = process_group_id else {
            let _ = terminate_command_tree(&mut child, &mut process_tree).await;
            return finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some("spawned hook process has no process id".to_string()),
                    outcome: "spawn_error",
                },
            );
        };
        match attach_and_resume_with_timeout(&process_tree, process_id, Some(timeout_deadline))
            .await
        {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                let _ = terminate_command_tree(&mut child, &mut process_tree).await;
                return finish_timeout(started_at, started, handler.timeout_sec);
            }
            Err(err) => {
                let _ = terminate_command_tree(&mut child, &mut process_tree).await;
                return finish_command_run(
                    started_at,
                    started,
                    CommandRunCompletion {
                        exit_code: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(format!("failed to contain hook process: {err}")),
                        outcome: "spawn_error",
                    },
                );
            }
        }
    }

    if tokio::time::Instant::now() >= timeout_deadline {
        let _ = terminate_command_tree(&mut child, &mut process_tree).await;
        return finish_timeout(started_at, started, handler.timeout_sec);
    }

    let stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        let _ = terminate_command_tree(&mut child, &mut process_tree).await;
        return finish_command_run(
            started_at,
            started,
            CommandRunCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("hook stdout pipe was unavailable".to_string()),
                outcome: "wait_error",
            },
        );
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = terminate_command_tree(&mut child, &mut process_tree).await;
        return finish_command_run(
            started_at,
            started,
            CommandRunCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("hook stderr pipe was unavailable".to_string()),
                outcome: "wait_error",
            },
        );
    };

    let write_stdin = async move {
        let Some(mut stdin) = stdin else {
            return Ok(());
        };
        match stdin.write_all(input_json.as_bytes()).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
            Err(err) => Err(err),
        }
    };
    let read_output = async {
        let ((), stdout, stderr) = tokio::try_join!(
            async { write_stdin.await.map_err(CommandRunError::Stdin) },
            async { capture_output(stdout).await.map_err(CommandRunError::Wait) },
            async { capture_output(stderr).await.map_err(CommandRunError::Wait) },
        )?;
        Ok::<_, CommandRunError>((stdout, stderr))
    };
    let wait_for_output = async {
        tokio::pin!(read_output);
        tokio::select! {
            status = child.wait() => {
                let status = status.map_err(CommandRunError::Wait)?;
                terminate_command_tree(&mut child, &mut process_tree)
                    .await
                    .map_err(CommandRunError::Containment)?;
                let (stdout, stderr) = read_output.await?;
                Ok::<_, CommandRunError>((status, stdout, stderr))
            }
            output = &mut read_output => {
                let (stdout, stderr) = match output {
                    Ok(output) => output,
                    Err(err) => {
                        let _ = terminate_command_tree(&mut child, &mut process_tree).await;
                        return Err(err);
                    }
                };
                let status = child.wait().await.map_err(CommandRunError::Wait)?;
                terminate_command_tree(&mut child, &mut process_tree)
                    .await
                    .map_err(CommandRunError::Containment)?;
                Ok((status, stdout, stderr))
            }
        }
    };
    match timeout_at(timeout_deadline, wait_for_output).await {
        Ok(Ok((status, stdout, stderr))) => {
            let exit_code = status.code();
            // A successful hook's stdout can be structured JSON, so never parse a
            // partial document as if it were complete. Exit-code-2 denials use
            // stderr and can safely retain the bounded head/tail preview.
            let stdout_exceeded_limit = exit_code == Some(0) && stdout.was_truncated();
            let error = stdout_exceeded_limit.then(|| {
                format!(
                    "hook stdout exceeded the {HOOK_STREAM_CAPTURE_MAX_BYTES}-byte capture limit"
                )
            });
            finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code,
                    stdout: stdout.into_string(),
                    stderr: stderr.into_string(),
                    error,
                    outcome: if stdout_exceeded_limit {
                        "output_limit"
                    } else {
                        "completed"
                    },
                },
            )
        }
        Ok(Err(err)) => {
            let _ = terminate_command_tree(&mut child, &mut process_tree).await;
            let (error, outcome) = match err {
                CommandRunError::Stdin(err) => {
                    (format!("failed to write hook stdin: {err}"), "stdin_error")
                }
                CommandRunError::Wait(err) => (err.to_string(), "wait_error"),
                CommandRunError::Containment(err) => (
                    format!("failed to terminate hook process descendants: {err}"),
                    "containment_error",
                ),
            };
            finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(error),
                    outcome,
                },
            )
        }
        Err(_) => {
            let _ = terminate_command_tree(&mut child, &mut process_tree).await;
            finish_timeout(started_at, started, handler.timeout_sec)
        }
    }
}

#[cfg(windows)]
fn windows_operation_timeout(deadline: Option<tokio::time::Instant>) -> Duration {
    deadline
        .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .unwrap_or(codex_utils_pty::WINDOWS_PROCESS_OPERATION_TIMEOUT)
}

#[cfg(windows)]
async fn attach_and_resume_with_timeout(
    process_tree: &ContainedProcessTree,
    process_id: u32,
    deadline: Option<tokio::time::Instant>,
) -> io::Result<()> {
    let managed = Arc::clone(&process_tree.managed);
    run_windows_process_operation(windows_operation_timeout(deadline), move || {
        managed.attach_and_resume(process_id)
    })
    .await
}

async fn terminate_command_tree(
    child: &mut tokio::process::Child,
    process_tree: &mut ContainedProcessTree,
) -> io::Result<()> {
    let descendant_result = process_tree.terminate_descendants();
    let child_result = match terminate_with_timeout(HOOK_TERMINATION_TIMEOUT, child.kill()).await {
        Ok(Err(err))
            if err.kind() != io::ErrorKind::InvalidInput
                && err.kind() != io::ErrorKind::NotFound =>
        {
            Err(err)
        }
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out after {:?} while killing hook process",
                HOOK_TERMINATION_TIMEOUT
            ),
        )),
        _ => Ok(()),
    };
    if descendant_result.is_ok() {
        process_tree.disarm();
    }
    descendant_result.and(child_result)
}

async fn terminate_with_timeout<F>(
    duration: Duration,
    termination: F,
) -> Result<io::Result<()>, tokio::time::error::Elapsed>
where
    F: Future<Output = io::Result<()>>,
{
    tokio::time::timeout(duration, termination).await
}

enum CommandRunError {
    Stdin(io::Error),
    Wait(io::Error),
    Containment(io::Error),
}

#[derive(Default)]
struct CapturedOutput {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: u64,
}

impl CapturedOutput {
    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));

        let head_limit = HOOK_STREAM_CAPTURE_MAX_BYTES / 2;
        let head_bytes = bytes.len().min(head_limit.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..head_bytes]);

        let tail_bytes = &bytes[head_bytes..];
        let tail_limit = HOOK_STREAM_CAPTURE_MAX_BYTES.saturating_sub(head_limit);
        if tail_bytes.len() >= tail_limit {
            self.tail.clear();
            self.tail
                .extend(&tail_bytes[tail_bytes.len().saturating_sub(tail_limit)..]);
            return;
        }

        let overflow = self
            .tail
            .len()
            .saturating_add(tail_bytes.len())
            .saturating_sub(tail_limit);
        self.tail.drain(..overflow);
        self.tail.extend(tail_bytes);
    }

    fn was_truncated(&self) -> bool {
        self.total_bytes > u64::try_from(HOOK_STREAM_CAPTURE_MAX_BYTES).unwrap_or(u64::MAX)
    }

    fn into_string(self) -> String {
        let retained_bytes = self.head.len().saturating_add(self.tail.len());
        let was_truncated = self.was_truncated();
        let omitted_bytes = self
            .total_bytes
            .saturating_sub(u64::try_from(retained_bytes).unwrap_or(u64::MAX));
        let mut bytes = self.head;
        if was_truncated {
            bytes.extend_from_slice(
                format!("\n... {omitted_bytes} bytes truncated from hook output ...\n").as_bytes(),
            );
        }
        bytes.extend(self.tail);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

async fn capture_output(mut output: impl AsyncRead + Unpin) -> io::Result<CapturedOutput> {
    let mut captured = CapturedOutput::default();
    let mut buffer = [0_u8; HOOK_STREAM_READ_BUFFER_BYTES];
    loop {
        let bytes_read = output.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(captured);
        }
        captured.push(&buffer[..bytes_read]);
    }
}

struct CommandRunCompletion {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    error: Option<String>,
    outcome: &'static str,
}

fn finish_command_run(
    started_at: i64,
    started: Instant,
    completion: CommandRunCompletion,
) -> CommandRunResult {
    Span::current().record("hook.command_outcome", completion.outcome);
    CommandRunResult {
        started_at,
        completed_at: chrono::Utc::now().timestamp(),
        duration_ms: started.elapsed().as_millis().try_into().unwrap_or(i64::MAX),
        exit_code: completion.exit_code,
        stdout: completion.stdout,
        stderr: completion.stderr,
        error: completion.error,
    }
}

fn finish_timeout(started_at: i64, started: Instant, timeout_sec: u64) -> CommandRunResult {
    finish_command_run(
        started_at,
        started,
        CommandRunCompletion {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(format!("hook timed out after {timeout_sec}s")),
            outcome: "timeout",
        },
    )
}

fn build_command(shell: &CommandShell, handler: &ConfiguredHandler) -> Command {
    let mut command = if shell.program.is_empty() {
        default_shell_command()
    } else {
        Command::new(&shell.program)
    };
    if shell.program.is_empty() {
        append_shell_command(&mut command, &handler.command, true);
    } else {
        command.args(&shell.args);

        append_shell_command(
            &mut command,
            &handler.command,
            shell.args.iter().any(|arg| arg.eq_ignore_ascii_case("/c")),
        );
    }
    command.envs(&handler.env);
    command
}

#[cfg(windows)]
fn append_shell_command(command: &mut Command, script: &str, use_raw_argument: bool) {
    if use_raw_argument {
        command.raw_arg(format!(r#""{script}""#));
    } else {
        command.arg(script);
    }
}

#[cfg(not(windows))]
fn append_shell_command(command: &mut Command, script: &str, _use_raw_argument: bool) {
    command.arg(script);
}

#[cfg(windows)]
fn default_shell_command() -> Command {
    let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    let mut command = Command::new(comspec);
    command.arg("/C");
    command
}

#[cfg(not(windows))]
fn default_shell_command() -> Command {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let mut command = Command::new(shell);
    command.arg("-c");
    command
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use codex_protocol::protocol::HookEventName;

    #[tokio::test]
    async fn hook_process_kill_has_an_independent_timeout() {
        let started = tokio::time::Instant::now();
        let result = terminate_with_timeout(
            Duration::from_millis(10),
            std::future::pending::<io::Result<()>>(),
        )
        .await;

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    use codex_protocol::protocol::HookSource;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn default_hook_shell_uses_the_platform_command_flag() {
        let command = default_shell_command();
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        #[cfg(windows)]
        assert_eq!(args, vec!["/C"]);
        #[cfg(not(windows))]
        assert_eq!(args, vec!["-c"]);
    }

    fn test_handler(command: String, timeout_sec: u64, cwd: &AbsolutePathBuf) -> ConfiguredHandler {
        ConfiguredHandler {
            event_name: HookEventName::PreToolUse,
            matcher: None,
            command,
            timeout_sec,
            status_message: None,
            source_path: cwd.join("hooks.json"),
            source: HookSource::User,
            display_order: 0,
            env: HashMap::new(),
        }
    }

    fn redirected_descendant_command(
        directory: &Path,
        keep_root_alive: bool,
    ) -> (String, std::path::PathBuf, std::path::PathBuf) {
        let started = directory.join("descendant-started.txt");
        let escaped = directory.join("descendant-escaped.txt");

        #[cfg(windows)]
        let command = {
            let child_script = directory.join("redirected-descendant.ps1");
            let stdout = directory.join("redirected-descendant.stdout");
            let stderr = directory.join("redirected-descendant.stderr");
            let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
            std::fs::write(
                &child_script,
                format!(
                    "Set-Content -LiteralPath '{}' -Value started\nStart-Sleep -Seconds 2\nSet-Content -LiteralPath '{}' -Value escaped\n",
                    quote(&started),
                    quote(&escaped),
                ),
            )
            .expect("write redirected descendant script");
            format!(
                "$null = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile', '-File', '{}') -WindowStyle Hidden -RedirectStandardOutput '{}' -RedirectStandardError '{}'; while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}{}",
                quote(&child_script),
                quote(&stdout),
                quote(&stderr),
                quote(&started),
                if keep_root_alive {
                    "; Start-Sleep -Seconds 60"
                } else {
                    ""
                },
            )
        };

        #[cfg(not(windows))]
        let command = {
            let stdout = directory.join("redirected-descendant.stdout");
            let stderr = directory.join("redirected-descendant.stderr");
            let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
            format!(
                "(printf started > '{}'; sleep 2; printf escaped > '{}') </dev/null > '{}' 2> '{}' & while [ ! -f '{}' ]; do sleep 0.01; done{}",
                quote(&started),
                quote(&escaped),
                quote(&stdout),
                quote(&stderr),
                quote(&started),
                if keep_root_alive { "; sleep 60" } else { "" },
            )
        };

        (command, started, escaped)
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_breakaway_attempt_helper() {
        use std::os::windows::process::CommandExt as _;

        let Some(child_script) = std::env::var_os("CODEX_HOOK_BREAKAWAY_CHILD_SCRIPT") else {
            return;
        };
        let denied = std::env::var_os("CODEX_HOOK_BREAKAWAY_DENIED_MARKER")
            .expect("breakaway denied marker");
        let launched = std::env::var_os("CODEX_HOOK_BREAKAWAY_LAUNCHED_MARKER")
            .expect("breakaway launched marker");

        let spawn = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-File"])
            .arg(child_script)
            // CREATE_BREAKAWAY_FROM_JOB: a strict hook Job must reject this flag.
            .creation_flags(0x0100_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match spawn {
            Err(error) => {
                std::fs::write(denied, error.to_string()).expect("write breakaway denied marker");
                assert_eq!(
                    error.raw_os_error(),
                    Some(5),
                    "explicit Job breakaway should fail with access denied"
                );
            }
            Ok(child) => {
                std::fs::write(launched, child.id().to_string())
                    .expect("write breakaway launched marker");
                panic!("strict hook Job allowed explicit descendant breakaway");
            }
        }
    }

    #[cfg(windows)]
    fn explicit_test_shell() -> CommandShell {
        CommandShell {
            program: "powershell.exe".to_string(),
            args: vec!["-NoProfile".to_string(), "-Command".to_string()],
        }
    }

    #[cfg(not(windows))]
    fn explicit_test_shell() -> CommandShell {
        CommandShell {
            program: "/bin/sh".to_string(),
            args: vec!["-lc".to_string()],
        }
    }

    #[tokio::test]
    async fn timeout_covers_a_blocked_stdin_write() {
        let cwd = AbsolutePathBuf::current_dir().expect("current directory");

        #[cfg(windows)]
        let command = "Start-Sleep -Seconds 60".to_string();
        #[cfg(not(windows))]
        let command = "sleep 60".to_string();

        let handler = test_handler(command, 1, &cwd);
        let input_json = "x".repeat(4 * 1024 * 1024);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_command(
                &explicit_test_shell(),
                &handler,
                0,
                &input_json,
                cwd.as_path(),
            ),
        )
        .await
        .expect("run_command should enforce its timeout while writing stdin");

        assert_eq!(result.exit_code, None);
        assert_eq!(result.stdout, "");
        assert_eq!(result.stderr, "");
        assert_eq!(result.error, Some("hook timed out after 1s".to_string()));
    }

    #[tokio::test]
    async fn timeout_covers_process_admission_before_spawn() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let marker = temp_dir.path().join("spawned-after-admission-timeout.txt");
        let cwd = AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute cwd");

        #[cfg(windows)]
        let command = {
            let marker = marker.to_string_lossy().replace('\'', "''");
            format!("Set-Content -LiteralPath '{marker}' -Value spawned")
        };
        #[cfg(not(windows))]
        let command = {
            let marker = marker.to_string_lossy().replace('\'', "'\\''");
            format!("printf spawned > '{marker}'")
        };

        let handler = test_handler(command, 1, &cwd);
        let blocked_admission =
            std::future::pending::<io::Result<codex_utils_pty::ManagedRootProcess>>();

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_command_with_reservation(
                &explicit_test_shell(),
                &handler,
                "{}",
                cwd.as_path(),
                blocked_admission,
            ),
        )
        .await
        .expect("run_command should enforce its timeout during process admission");

        assert_eq!(result.error, Some("hook timed out after 1s".to_string()));
        assert!(!marker.exists(), "the hook spawned after its deadline");
    }

    #[tokio::test]
    async fn timeout_terminates_descendant_processes() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let marker = temp_dir.path().join("escaped-descendant.txt");
        let cwd = AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute cwd");

        #[cfg(windows)]
        let command = {
            let marker = marker.to_string_lossy().replace('\'', "''");
            format!(
                "Start-Job -ScriptBlock {{ Start-Sleep -Seconds 3; Set-Content -LiteralPath '{marker}' -Value done }} | Out-Null; Start-Sleep -Seconds 60"
            )
        };
        #[cfg(not(windows))]
        let command = {
            let marker = marker.to_string_lossy().replace('\'', "'\\''");
            format!("(sleep 3; printf done > '{marker}') & sleep 60")
        };

        let handler = test_handler(command, 1, &cwd);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_command(&explicit_test_shell(), &handler, 0, "{}", cwd.as_path()),
        )
        .await
        .expect("run_command should enforce its timeout");

        assert_eq!(result.error, Some("hook timed out after 1s".to_string()));
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !marker.exists(),
            "a descendant survived the hook timeout and wrote {}",
            marker.display()
        );
    }

    #[tokio::test]
    async fn successful_hook_terminates_redirected_descendants_before_returning() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cwd = AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute cwd");
        let (command, started, escaped) =
            redirected_descendant_command(temp_dir.path(), /*keep_root_alive*/ false);
        let handler = test_handler(command, 15, &cwd);

        let result = run_command(&explicit_test_shell(), &handler, 0, "{}", cwd.as_path()).await;

        assert_eq!(result.exit_code, Some(0), "{:?}", result.error);
        assert_eq!(result.error, None);
        assert!(started.exists(), "the descendant did not actually start");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !escaped.exists(),
            "a redirected descendant survived successful hook completion"
        );
    }

    #[tokio::test]
    async fn cancelling_hook_terminates_redirected_descendants() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cwd = AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute cwd");
        let (command, started, escaped) =
            redirected_descendant_command(temp_dir.path(), /*keep_root_alive*/ true);
        let handler = test_handler(command, 30, &cwd);
        let shell = explicit_test_shell();

        let hook_task =
            tokio::spawn(
                async move { run_command(&shell, &handler, 0, "{}", cwd.as_path()).await },
            );
        tokio::time::timeout(Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("redirected descendant should start");

        hook_task.abort();
        assert!(
            hook_task
                .await
                .expect_err("aborted hook task should not complete")
                .is_cancelled()
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !escaped.exists(),
            "a redirected descendant survived hook cancellation"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn configured_hook_denies_explicit_windows_job_breakaway() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cwd = AbsolutePathBuf::try_from(temp_dir.path().to_path_buf()).expect("absolute cwd");
        let child_script = temp_dir.path().join("breakaway-child.ps1");
        let denied = temp_dir.path().join("breakaway-denied.txt");
        let launched = temp_dir.path().join("breakaway-launched.txt");
        let escaped = temp_dir.path().join("breakaway-escaped.txt");
        let quote = |path: &Path| path.to_string_lossy().replace('\'', "''");
        std::fs::write(
            &child_script,
            format!(
                "Start-Sleep -Seconds 2\nSet-Content -LiteralPath '{}' -Value escaped\n",
                quote(&escaped),
            ),
        )
        .expect("write breakaway child script");

        let test_executable = std::env::current_exe().expect("current test executable");
        let command = format!(
            "& '{}' --exact 'engine::command_runner::tests::windows_explicit_breakaway_attempt_helper' --nocapture",
            quote(&test_executable),
        );
        let mut handler = test_handler(command, 10, &cwd);
        handler.env.insert(
            "CODEX_HOOK_BREAKAWAY_CHILD_SCRIPT".to_string(),
            child_script.to_string_lossy().into_owned(),
        );
        handler.env.insert(
            "CODEX_HOOK_BREAKAWAY_DENIED_MARKER".to_string(),
            denied.to_string_lossy().into_owned(),
        );
        handler.env.insert(
            "CODEX_HOOK_BREAKAWAY_LAUNCHED_MARKER".to_string(),
            launched.to_string_lossy().into_owned(),
        );

        let result = run_command(&explicit_test_shell(), &handler, 0, "{}", cwd.as_path()).await;

        assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
        assert_eq!(result.error, None);
        assert!(
            denied.exists(),
            "the explicit breakaway attempt did not run"
        );
        assert!(
            !launched.exists(),
            "the explicit breakaway attempt succeeded"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !escaped.exists(),
            "an explicitly broken-away hook descendant survived"
        );
    }

    #[tokio::test]
    async fn drains_output_while_writing_stdin() {
        let cwd = AbsolutePathBuf::current_dir().expect("current directory");

        #[cfg(windows)]
        let command = concat!(
            "$stdout = [Console]::OpenStandardOutput(); ",
            "$bytes = New-Object byte[] (2 * 1024 * 1024); ",
            "$stdout.Write($bytes, 0, $bytes.Length); ",
            "$stdout.Flush(); ",
            "[Console]::In.ReadToEnd() | Out-Null"
        )
        .to_string();
        #[cfg(not(windows))]
        let command = "dd if=/dev/zero bs=65536 count=32 2>/dev/null; cat >/dev/null".to_string();

        let handler = test_handler(command, 5, &cwd);
        let input_json = "x".repeat(4 * 1024 * 1024);

        let result = tokio::time::timeout(
            Duration::from_secs(15),
            run_command(
                &explicit_test_shell(),
                &handler,
                0,
                &input_json,
                cwd.as_path(),
            ),
        )
        .await
        .expect("hook stdin and output should make progress concurrently");

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.error,
            Some(format!(
                "hook stdout exceeded the {HOOK_STREAM_CAPTURE_MAX_BYTES}-byte capture limit"
            ))
        );
    }

    #[tokio::test]
    async fn retains_output_when_hook_closes_stdin_early() {
        let cwd = AbsolutePathBuf::current_dir().expect("current directory");
        let handler = test_handler("echo retained-hook-output".to_string(), 5, &cwd);
        let shell = CommandShell {
            program: String::new(),
            args: Vec::new(),
        };
        let input_json = "x".repeat(4 * 1024 * 1024);

        let result = run_command(&shell, &handler, 0, &input_json, cwd.as_path()).await;

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim(), "retained-hook-output");
        assert_eq!(result.stderr, "");
        assert_eq!(result.error, None);
    }
}
