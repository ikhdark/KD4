use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::Path;
use std::process::Stdio;
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

const HOOK_STREAM_CAPTURE_MAX_BYTES: usize = 1024 * 1024;
const HOOK_STREAM_READ_BUFFER_BYTES: usize = 16 * 1024;
const HOOK_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);

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
        codex_utils_pty::ManagedRootProcess::reserve_with_reclaim(),
    )
    .await
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
    let mut child =
        match run_windows_process_operation(spawn_timeout, move || command.spawn()).await {
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

    #[cfg(windows)]
    {
        let Some(process_id) = child.id() else {
            terminate_command_tree(&mut child, &managed).await;
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
        if let Err(err) = managed.attach_and_resume(process_id) {
            terminate_command_tree(&mut child, &managed).await;
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

    if tokio::time::Instant::now() >= timeout_deadline {
        terminate_command_tree(&mut child, &managed).await;
        return finish_timeout(started_at, started, handler.timeout_sec);
    }

    let stdin = child.stdin.take();
    let Some(stdout) = child.stdout.take() else {
        terminate_command_tree(&mut child, &managed).await;
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
        terminate_command_tree(&mut child, &managed).await;
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
    let wait_for_output = async {
        let ((), status, stdout, stderr) = tokio::try_join!(
            async { write_stdin.await.map_err(CommandRunError::Stdin) },
            async { child.wait().await.map_err(CommandRunError::Wait) },
            async { capture_output(stdout).await.map_err(CommandRunError::Wait) },
            async { capture_output(stderr).await.map_err(CommandRunError::Wait) },
        )?;
        Ok::<_, CommandRunError>((status, stdout, stderr))
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
            terminate_command_tree(&mut child, &managed).await;
            let (error, outcome) = match err {
                CommandRunError::Stdin(err) => {
                    (format!("failed to write hook stdin: {err}"), "stdin_error")
                }
                CommandRunError::Wait(err) => (err.to_string(), "wait_error"),
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
            terminate_command_tree(&mut child, &managed).await;
            finish_timeout(started_at, started, handler.timeout_sec)
        }
    }
}

async fn terminate_command_tree(
    child: &mut tokio::process::Child,
    managed: &codex_utils_pty::ManagedRootProcess,
) {
    #[cfg(windows)]
    if let Err(err) = managed.terminate() {
        tracing::warn!("failed to terminate hook process Job Object: {err:?}");
    }
    #[cfg(not(windows))]
    let _ = managed;

    if let Some(process_group_id) = child.id()
        && let Err(err) = codex_utils_pty::process_group::kill_process_group(process_group_id)
    {
        tracing::warn!("failed to kill hook process group {process_group_id}: {err:?}");
    }
    match terminate_with_timeout(HOOK_TERMINATION_TIMEOUT, child.kill()).await {
        Ok(Err(err))
            if err.kind() != io::ErrorKind::InvalidInput
                && err.kind() != io::ErrorKind::NotFound =>
        {
            tracing::warn!("failed to kill hook process: {err:?}");
        }
        Err(_) => tracing::warn!(
            "timed out after {:?} while killing hook process",
            HOOK_TERMINATION_TIMEOUT
        ),
        _ => {}
    }
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
