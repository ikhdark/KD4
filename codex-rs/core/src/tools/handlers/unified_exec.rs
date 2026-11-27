use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell;
use crate::shell::get_shell_by_model_provided_path;
use crate::shell::resolve_use_login_shell;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::handlers::parse_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use codex_protocol::models::AdditionalPermissionProfile;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

mod exec_command;
mod write_stdin;

pub use exec_command::ExecCommandHandler;
pub(crate) use exec_command::ExecCommandHandlerOptions;
pub use write_stdin::WriteStdinHandler;

/// The manager gives back control with NESTED_POLL_MARGIN remaining. Use
/// only part of that headroom for optional handler work; leave the rest for
/// dispatch and transport to return the failure.
fn nested_failure_return_bound(
    deadline: Option<std::time::Instant>,
) -> Option<tokio::time::Instant> {
    let deadline = tokio::time::Instant::from_std(deadline?);
    Some(
        deadline
            .checked_sub(crate::unified_exec::NESTED_POLL_MARGIN / 2)
            .unwrap_or_else(tokio::time::Instant::now),
    )
}

/// Only for observations without a commit, cursor acknowledgement, or send.
async fn bounded_failure_read<T>(
    bound: Option<tokio::time::Instant>,
    read: impl std::future::Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    match bound {
        Some(bound) => tokio::time::timeout_at(bound, read).await,
        None => Ok(read.await),
    }
}

fn append_failure_notice(output: &mut ExecCommandToolOutput, notice: String) {
    output.repair_notice = Some(match output.repair_notice.take() {
        Some(previous) => format!("{previous}\n{notice}"),
        None => notice,
    });
}

/// Publish a separate, exact byte locator before storage can block. Do not
/// replace the process owner's cumulative log, rewrite its byte map, or
/// invent producer bytes from a failure notice. A durability failure can
/// also retain the observed receipt bytes, explicitly not a transcript.
fn retain_nested_failure_output(
    output: &mut ExecCommandToolOutput,
    session: &Arc<crate::session::Session>,
    turn: &crate::session::turn_context::TurnContext,
    call_id: &str,
    durability_failure: bool,
) {
    use crate::tools::command_output_artifact as artifacts;
    if output.raw_output_artifact.as_ref().is_some_and(|artifact| {
        !artifact.is_pending() && !matches!(artifact, artifacts::RawOutputArtifact::Empty)
    }) {
        return;
    }
    let snapshot = output.process_output.as_ref().filter(|snapshot| {
        output.process_exited
            && snapshot.output_closed
            && snapshot.aggregated_output_is_exact
    });
    let (bytes, scope) = if let Some(snapshot) = snapshot {
        (&snapshot.aggregated_output, "exact terminal output snapshot")
    } else if durability_failure || output.output_ranges.as_ref().is_some_and(|ranges| !ranges.range.is_empty()) {
        (&output.raw_output, "observed receipt bytes, not the complete producer transcript")
    } else {
        // There are no newly observed producer bytes to preserve.
        return;
    };
    if bytes.is_empty() {
        return;
    }

    let bytes = bytes.clone();
    let length = bytes.len();
    let call_id = call_id.to_owned();
    let id = artifacts::ToolOutputArtifactId::new();
    let codex_home = turn.config.codex_home.clone();
    let thread_id = session.thread_id.to_string();
    let owner = artifacts::reserve_canonical_retention(&codex_home, &thread_id, id);
    let tasks = session.terminal_tasks.clone();
    let session = Arc::clone(session);
    tasks.spawn(async move {
        let canonical = codex_tools::CanonicalToolResult::bytes(bytes);
        let artifact = artifacts::create_canonical_output_artifact_with_id(
            &codex_home, &thread_id, &canonical, id,
        )
        .await;
        owner.settle(&artifact);
        if artifact.complete && let Some(id) = artifact.artifact_id() {
            session
                .register_tool_artifact_origin(id, call_id, canonical.exact_bytes, canonical.sha256)
                .await;
        }
    });
    append_failure_notice(output, format!(
        "Recovery storage pending for {scope}; this is separate from the process log. Do not rerun or resend input. {}",
        serde_json::json!({
            "tool": "read_tool_output", "arguments": {
                "artifact_id": id.to_string(),
                "selectors": [{"kind": "bytes", "start": 0, "end": length}]
            }
        }),
    ));
}

async fn preserve_durability_failure_output(
    message: String,
    output: Option<Box<ExecCommandToolOutput>>,
    session: &Arc<crate::session::Session>,
    turn: &crate::session::turn_context::TurnContext,
    nested_call_id: Option<&str>,
) -> String {
    let Some(mut output) = output else {
        return message;
    };
    // A zero display budget must not discard the only observed result when
    // durable history failed. Keep this fatal diagnostic bounded and recoverable.
    output.max_output_tokens = Some(codex_code_mode::MAX_NESTED_COMMAND_OUTPUT_TOKENS);
    if let Some(call_id) = nested_call_id {
        retain_nested_failure_output(&mut output, session, turn, call_id, true);
    } else {
        output
            .prepare_recovery_artifact(turn.config.codex_home.as_path(), &session.thread_id.to_string())
            .await;
    }
    let observed = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".into(),
    });
    format!(
        "{message}\nCommand result observed before durability failure (not successful persistence; do not rerun):\n{observed}"
    )
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawExecCommandArgs")]
pub(crate) struct ExecCommandArgs {
    command: CommandInvocation,
    env: std::collections::HashMap<String, String>,
    shell: Option<String>,
    login: Option<bool>,
    tty: bool,
    pipe_stdin: bool,
    yield_time_ms: u64,
    /// Whether the caller chose `yield_time_ms` rather than its default.
    yield_time_requested: bool,
    stall_timeout_ms: Option<u64>,
    stall_timeout_requested: bool,
    max_output_tokens: Option<usize>,
    quiet_on_success: bool,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
    justification: Option<String>,
    prefix_rule: Option<Vec<String>>,
    force_fresh: bool,
    validation: Option<codex_protocol::validation::ValidationCommandContext>,
    argument_notices: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawExecCommandArgs {
    #[serde(default, rename = "output_encoding")]
    _output_encoding: Option<crate::tools::context::ExecOutputEncoding>,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    program: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    script_body: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    login: Option<bool>,
    #[serde(default = "default_tty")]
    tty: bool,
    #[serde(default)]
    pipe_stdin: bool,
    #[serde(default)]
    yield_time_ms: Option<ExecObservationWindow>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    quiet_on_success: bool,
    // Legacy sandbox hints from older callers are accepted and ignored.
    #[serde(default)]
    sandbox_permissions: Option<serde_json::Value>,
    #[serde(default)]
    additional_permissions: Option<serde_json::Value>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
    #[serde(default)]
    force_fresh: bool,
    #[serde(default)]
    validation: Option<codex_protocol::validation::ValidationCommandContext>,
    // Legacy hard deadlines are ignored, never used to kill a command.
    #[serde(default)]
    timeout_ms: Option<serde_json::Value>,
    #[serde(default)]
    stall_timeout_ms: Option<u64>,
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug)]
struct ExecObservationWindow {
    milliseconds: i128,
    repaired_decimal_string: bool,
}

impl<'de> Deserialize<'de> for ExecObservationWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let literal = raw.get();
        let repaired_decimal_string = literal.starts_with('"');
        let milliseconds = if repaired_decimal_string {
            let decoded: String = serde_json::from_str(literal).map_err(D::Error::custom)?;
            let digits = decoded.strip_prefix('-').unwrap_or(&decoded);
            if digits.is_empty()
                || !digits.bytes().all(|byte| byte.is_ascii_digit())
                || (digits.starts_with('0') && decoded != "0")
            {
                return Err(D::Error::custom("expected an integer millisecond observation window"));
            }
            decoded.parse::<i128>().map_err(D::Error::custom)?
        } else {
            serde_json::from_str(literal).map_err(D::Error::custom)?
        };
        Ok(Self { milliseconds, repaired_decimal_string })
    }
}

impl TryFrom<RawExecCommandArgs> for ExecCommandArgs {
    type Error = String;

    fn try_from(raw: RawExecCommandArgs) -> Result<Self, Self::Error> {
        if raw.tty && raw.pipe_stdin {
            return Err("pipe_stdin requires tty=false; terminal input is not a pipe".into());
        }
        let mut names = std::collections::BTreeSet::new();
        for (name, value) in &raw.env {
            if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
                return Err("Invalid command environment: names must be nonempty and contain neither '=' nor NUL; values must not contain NUL.".into());
            }
            if cfg!(windows) && !names.insert(name.to_ascii_uppercase()) {
                return Err(
                    "Command environment contains case-insensitive duplicate names.".into(),
                );
            }
        }
        let mut argument_notices = raw
            .extra
            .keys()
            .map(|field| format!("Ignored unsupported exec_command field `{field}`."))
            .collect::<Vec<_>>();
        if raw.timeout_ms.is_some() {
            argument_notices.push("Ignored `timeout_ms`; `yield_time_ms` controls observation, not a process deadline.".to_string());
        }

        let command = CommandInvocation::from_parts(
            "exec_command",
            "cmd",
            raw.cmd.as_deref(),
            raw.kind.as_deref(),
            raw.program.as_deref(),
            raw.args.as_deref(),
            raw.script_body.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        if command.is_argv() && (raw.shell.is_some() || raw.login == Some(true)) {
            return Err(
                "`shell` and `login=true` require a script command; omit `shell` and omit `login` or set it to false with program/args."
                    .to_string(),
            );
        }
        // Environment selection and workdir resolution intentionally happen
        // in a separate boundary pass before the command arguments are
        // decoded. Reading these fields here keeps this decoder permissive for
        // that shared surface without retaining either value internally.
        let _compatibility_only_fields = (
            &raw.environment_id,
            &raw.workdir,
            &raw.sandbox_permissions,
            &raw.additional_permissions,
        );

        // Native PowerShell parsing can wait on IPC. Deserialization also runs
        // during synchronous Code Mode preflight, so only decode structure here.
        let requested_yield_time_ms = raw
            .yield_time_ms
            .map_or_else(|| i128::from(default_exec_yield_time_ms()), |window| window.milliseconds);
        if raw.yield_time_ms.is_some_and(|window| window.repaired_decimal_string) {
            argument_notices.push("Interpreted decimal string `yield_time_ms` as integer milliseconds.".to_string());
        }
        let yield_time_ms = requested_yield_time_ms.clamp(
            i128::from(crate::unified_exec::MIN_YIELD_TIME_MS),
            i128::from(crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS),
        ) as u64;
        if raw
            .yield_time_ms
            .is_some_and(|window| window.milliseconds != i128::from(yield_time_ms))
        {
            argument_notices.push(format!("Adjusted `yield_time_ms` to {yield_time_ms}."));
        }
        Ok(Self {
            command,
            env: raw.env,
            shell: raw.shell,
            login: raw.login,
            tty: raw.tty,
            pipe_stdin: raw.pipe_stdin,
            yield_time_ms,
            yield_time_requested: raw.yield_time_ms.is_some(),
            stall_timeout_ms: raw
                .stall_timeout_ms
                .or(Some(crate::exec::DEFAULT_COMMAND_STALL_TIMEOUT_MS))
                .filter(|timeout| *timeout != 0),
            stall_timeout_requested: raw.stall_timeout_ms.is_some(),
            max_output_tokens: raw.max_output_tokens,
            quiet_on_success: raw.quiet_on_success,
            // KD4 has no sandbox: legacy per-command overrides and additional
            // permission requests are accepted but never refused or prompted,
            // because every command already runs with full access.
            sandbox_permissions: SandboxPermissions::UseDefault,
            additional_permissions: None,
            justification: raw.justification,
            prefix_rule: raw.prefix_rule,
            force_fresh: raw.force_fresh,
            validation: raw.validation,
            argument_notices,
        })
    }
}

impl ExecCommandArgs {
    pub(super) fn apply_validation_observation_policy(&mut self, validation: bool) {
        if validation {
            if !self.yield_time_requested {
                self.yield_time_ms = crate::unified_exec::MAX_YIELD_TIME_MS;
            }
            if !self.stall_timeout_requested {
                self.stall_timeout_ms = None;
            }
        }
    }

    pub(crate) fn command_invocation(&self) -> &CommandInvocation {
        &self.command
    }

    pub(crate) fn replace_command_invocation(&mut self, invocation: &CommandInvocation) {
        self.command = invocation.clone();
    }

    fn observation_yield_time_ms(&self, nested: bool) -> u64 {
        if !nested || self.yield_time_requested {
            return self.yield_time_ms;
        }
        if self.tty {
            self.yield_time_ms.max(NESTED_EXEC_YIELD_TIME_MS)
        } else {
            NESTED_NONINTERACTIVE_EXEC_YIELD_TIME_MS
        }
    }
}

pub(crate) fn validate_exec_command_arguments(arguments: &str) -> Result<(), String> {
    parse_arguments::<ExecCommandArgs>(arguments)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[derive(Debug, Deserialize)]
struct ExecCommandEnvironmentArgs {
    #[serde(default)]
    output_encoding: Option<crate::tools::context::ExecOutputEncoding>,
    #[serde(default)]
    environment_id: Option<String>,
    // Keep this raw until after environment selection; relative paths must be
    // resolved against the selected environment cwd, not the process cwd.
    #[serde(default)]
    workdir: Option<String>,
}

fn default_exec_yield_time_ms() -> u64 {
    2_000
}

/// Interactive nested commands retain their short observation window.
pub(super) const NESTED_EXEC_YIELD_TIME_MS: u64 = 10_000;

/// An awaiting cell already owns noninteractive work. Use the same bounded
/// window as its terminal polls instead of requiring the model to write a
/// polling loop. Explicit yields still allow deliberate background work.
pub(super) const NESTED_NONINTERACTIVE_EXEC_YIELD_TIME_MS: u64 =
    crate::tools::code_mode::NESTED_DEFAULT_POLL.as_millis() as u64;

fn default_write_stdin_yield_time_ms() -> u64 {
    250
}

fn default_tty() -> bool {
    false
}

#[derive(Debug)]
pub(crate) struct ResolvedCommand {
    pub(crate) command: Vec<String>,
    pub(crate) safety_command: Vec<String>,
    pub(crate) shell_type: ShellType,
    pub(crate) use_login_shell: bool,
    pub(crate) preflight_shell_type: Option<ShellType>,
}

fn post_unified_exec_tool_use_payload(
    invocation: &ToolInvocation,
    result: &dyn ToolOutput,
) -> Option<PostToolUsePayload> {
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return None;
    };

    let tool_input = exec_command::exec_command_hook_input(arguments)
        .or_else(|| result.post_tool_use_input(&invocation.payload))?;
    let tool_use_id = result.post_tool_use_id(&invocation.call_id);
    let tool_response = result.post_tool_use_response(&tool_use_id, &invocation.payload)?;
    Some(PostToolUsePayload {
        tool_name: HookToolName::exec_command(),
        tool_use_id,
        tool_input,
        tool_response,
    })
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
    allow_login_shell: bool,
    environment_is_remote: bool,
) -> Result<ResolvedCommand, String> {
    let use_login_shell = resolve_use_login_shell(args.login, allow_login_shell)?;
    let invocation = args.command_invocation();

    if invocation.is_powershell_script() {
        let powershell = match args.shell.as_ref() {
            Some(shell) => get_shell_by_model_provided_path(&PathBuf::from(shell))
                .map_err(|err| err.to_string())?,
            None if session_shell.shell_type == ShellType::PowerShell => {
                session_shell.as_ref().clone()
            }
            None if environment_is_remote => {
                return Err(
                    "`kind: \"powershell_script\"` requires the selected remote environment to report PowerShell."
                        .to_string(),
                );
            }
            None => get_shell(ShellType::PowerShell, /*path*/ None).ok_or_else(|| {
                "`kind: \"powershell_script\"` requires PowerShell in this environment; use `kind: \"script\"` with an available shell instead."
                    .to_string()
            })?,
        };
        if powershell.shell_type != ShellType::PowerShell {
            return Err(format!(
                "`kind: \"powershell_script\"` requires PowerShell; `{}` was selected.",
                powershell.name()
            ));
        }
        return Ok(ResolvedCommand {
            command: invocation
                .to_exec_args(&powershell, use_login_shell)
                .map_err(|error| error.to_string())?,
            safety_command: invocation
                .to_safety_args(&powershell, use_login_shell)
                .map_err(|error| error.to_string())?,
            shell_type: ShellType::PowerShell,
            use_login_shell,
            preflight_shell_type: Some(ShellType::PowerShell),
        });
    }

    if invocation.is_argv() {
        if args.shell.is_some() || args.login == Some(true) {
            return Err(
                "`shell` and `login=true` require a script command; omit `shell` and omit `login` or set it to false with program/args."
                    .to_string(),
            );
        }
        let command = invocation
            .to_exec_args(session_shell.as_ref(), use_login_shell)
            .map_err(|error| error.to_string())?;
        return Ok(ResolvedCommand {
            safety_command: command.clone(),
            command,
            shell_type: session_shell.shell_type,
            use_login_shell,
            preflight_shell_type: None,
        });
    }

    let model_shell = args
        .shell
        .as_ref()
        .map(|shell_str| get_shell_by_model_provided_path(&PathBuf::from(shell_str)))
        .transpose()
        .map_err(|err| err.to_string())?;
    let shell = model_shell.as_ref().unwrap_or(session_shell.as_ref());
    if shell.shell_type == ShellType::Cmd && args.login == Some(true) {
        return Err(
            "cmd.exe does not support login/profile loading; omit `login` or set it to false."
                .into(),
        );
    }
    let command = invocation
        .to_exec_args(shell, use_login_shell)
        .map_err(|error| error.to_string())?;
    Ok(ResolvedCommand {
        safety_command: command.clone(),
        command,
        shell_type: shell.shell_type,
        use_login_shell,
        preflight_shell_type: Some(shell.shell_type),
    })
}
