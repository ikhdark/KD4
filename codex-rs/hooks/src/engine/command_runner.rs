use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::Span;

use super::CommandShell;
use super::ConfiguredHandler;
use super::dispatcher::hook_event_name_label;
use super::dispatcher::hook_execution_mode_label;
use super::dispatcher::hook_handler_type_label;
use super::dispatcher::hook_scope_label;
use super::dispatcher::hook_source_label;
use super::dispatcher::scope_for_event;
use codex_protocol::protocol::HookExecutionMode;
use codex_protocol::protocol::HookHandlerType;

const HOOK_STREAM_CAPTURE_MAX_BYTES: usize = 1024 * 1024;
const HOOK_STREAM_READ_BUFFER_BYTES: usize = 16 * 1024;

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
        hook.event_name = hook_event_name_label(handler.event_name),
        hook.handler_type = hook_handler_type_label(HookHandlerType::Command),
        hook.execution_mode = hook_execution_mode_label(HookExecutionMode::Sync),
        hook.scope = hook_scope_label(scope_for_event(handler.event_name)),
        hook.source = hook_source_label(handler.source),
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
    let started_at = chrono::Utc::now().timestamp();
    let started = Instant::now();

    let mut command = build_command(shell, handler);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

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

    if let Some(mut stdin) = child.stdin.take()
        && let Err(err) = stdin.write_all(input_json.as_bytes()).await
    {
        let _ = child.kill().await;
        return finish_command_run(
            started_at,
            started,
            CommandRunCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to write hook stdin: {err}")),
                outcome: "stdin_error",
            },
        );
    }

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
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
        let _ = child.kill().await;
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

    let timeout_duration = Duration::from_secs(handler.timeout_sec);
    let wait_for_output =
        async { tokio::try_join!(child.wait(), capture_output(stdout), capture_output(stderr)) };
    match timeout(timeout_duration, wait_for_output).await {
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
            let _ = child.kill().await;
            finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                    outcome: "wait_error",
                },
            )
        }
        Err(_) => {
            let _ = child.kill().await;
            finish_command_run(
                started_at,
                started,
                CommandRunCompletion {
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("hook timed out after {}s", handler.timeout_sec)),
                    outcome: "timeout",
                },
            )
        }
    }
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

fn build_command(shell: &CommandShell, handler: &ConfiguredHandler) -> Command {
    let mut command = if shell.program.is_empty() {
        default_shell_command()
    } else {
        Command::new(&shell.program)
    };
    if shell.program.is_empty() {
        command.arg(&handler.command);
    } else {
        command.args(&shell.args);
        command.arg(&handler.command);
    }
    command.envs(&handler.env);
    command
}

fn default_shell_command() -> Command {
    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        let mut command = Command::new(comspec);
        command.arg("/C");
        command
    }

    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut command = Command::new(shell);
        command.arg("-lc");
        command
    }
}
