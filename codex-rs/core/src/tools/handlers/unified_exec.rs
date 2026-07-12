use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell;
use crate::shell::get_shell_by_model_provided_path;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::command_shape::CommandInvocation;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use codex_exec_server::Environment;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_tools::UnifiedExecShellMode;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use crate::tools::handlers::parse_arguments;

mod exec_command;
mod write_stdin;

pub use exec_command::ExecCommandHandler;
pub(crate) use exec_command::ExecCommandHandlerOptions;
pub use write_stdin::WriteStdinHandler;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ExecCommandArgs {
    #[serde(default)]
    pub(crate) cmd: Option<String>,
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
    #[serde(default = "default_exec_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
    #[serde(default)]
    additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
}

impl ExecCommandArgs {
    pub(crate) fn command_invocation(&self) -> Result<CommandInvocation, String> {
        CommandInvocation::from_parts(
            "exec_command",
            "cmd",
            self.cmd.as_deref(),
            self.kind.as_deref(),
            self.program.as_deref(),
            self.args.as_deref(),
            self.script_body.as_deref(),
        )
        .map_err(|err| err.to_string())
    }

    pub(crate) fn replace_command_invocation(&mut self, invocation: &CommandInvocation) {
        self.cmd = None;
        self.kind = None;
        self.program = None;
        self.args = None;
        self.script_body = None;

        match invocation {
            CommandInvocation::Script(script) => {
                self.kind = Some("script".to_string());
                self.cmd = Some(script.clone());
            }
            CommandInvocation::Argv { program, args } => {
                self.kind = Some("argv".to_string());
                self.program = Some(program.clone());
                self.args = Some(args.clone());
            }
            CommandInvocation::PowerShellScript(script_body) => {
                self.kind = Some("powershell_script".to_string());
                self.script_body = Some(script_body.clone());
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ExecCommandEnvironmentArgs {
    #[serde(default)]
    environment_id: Option<String>,
    // Keep this raw until after environment selection; relative paths must be
    // resolved against the selected environment cwd, not the process cwd.
    #[serde(default)]
    workdir: Option<String>,
}

fn default_exec_yield_time_ms() -> u64 {
    10_000
}

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
    pub(crate) preflight_shell_type: Option<ShellType>,
}

fn post_unified_exec_tool_use_payload(
    invocation: &ToolInvocation,
    result: &dyn ToolOutput,
) -> Option<PostToolUsePayload> {
    let ToolPayload::Function { .. } = &invocation.payload else {
        return None;
    };

    let tool_input = result.post_tool_use_input(&invocation.payload)?;
    let tool_use_id = result.post_tool_use_id(&invocation.call_id);
    let tool_response = result.post_tool_use_response(&tool_use_id, &invocation.payload)?;
    Some(PostToolUsePayload {
        tool_name: HookToolName::bash(),
        tool_use_id,
        tool_input,
        tool_response,
    })
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
    shell_mode: &UnifiedExecShellMode,
    allow_login_shell: bool,
    environment_is_remote: bool,
) -> Result<ResolvedCommand, String> {
    let use_login_shell = match args.login {
        Some(true) if !allow_login_shell => {
            return Err(
                "login shell is disabled by config; omit `login` or set it to false.".to_string(),
            );
        }
        Some(use_login_shell) => use_login_shell,
        None => allow_login_shell,
    };
    let invocation = args.command_invocation()?;

    if invocation.is_powershell_script() {
        let powershell = match args.shell.as_ref() {
            Some(shell) => get_shell_by_model_provided_path(&PathBuf::from(shell)),
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
            command: invocation.to_exec_args(&powershell, use_login_shell),
            safety_command: invocation.to_safety_args(&powershell, use_login_shell),
            shell_type: ShellType::PowerShell,
            preflight_shell_type: Some(ShellType::PowerShell),
        });
    }

    if invocation.is_argv() {
        if args.shell.is_some() {
            return Err(
                "`shell` is only valid for script commands; omit it when `kind` is `argv`."
                    .to_string(),
            );
        }
        let command = invocation.to_exec_args(session_shell.as_ref(), use_login_shell);
        return Ok(ResolvedCommand {
            safety_command: command.clone(),
            command,
            shell_type: session_shell.shell_type,
            preflight_shell_type: None,
        });
    }

    match shell_mode {
        UnifiedExecShellMode::Direct => {
            let model_shell = args
                .shell
                .as_ref()
                .map(|shell_str| get_shell_by_model_provided_path(&PathBuf::from(shell_str)));
            let shell = model_shell.as_ref().unwrap_or(session_shell.as_ref());
            let command = invocation.to_exec_args(shell, use_login_shell);
            Ok(ResolvedCommand {
                safety_command: command.clone(),
                command,
                shell_type: shell.shell_type,
                preflight_shell_type: Some(shell.shell_type),
            })
        }
        UnifiedExecShellMode::ZshFork(zsh_fork_config) => {
            if args.shell.is_some() {
                return Err(
                    "`shell` is not supported for local zsh-fork exec; omit `shell` to use zsh-fork, or target a remote environment where `shell` is supported.".to_string(),
                );
            }

            let command = vec![
                zsh_fork_config.shell_zsh_path.to_string_lossy().to_string(),
                if use_login_shell { "-lc" } else { "-c" }.to_string(),
                invocation.display_command(),
            ];
            Ok(ResolvedCommand {
                safety_command: command.clone(),
                command,
                shell_type: ShellType::Zsh,
                preflight_shell_type: Some(ShellType::Zsh),
            })
        }
    }
}

pub(crate) fn shell_mode_for_environment(
    turn_shell_mode: &UnifiedExecShellMode,
    environment: &Environment,
) -> UnifiedExecShellMode {
    if environment.is_remote() {
        UnifiedExecShellMode::Direct
    } else {
        turn_shell_mode.clone()
    }
}

#[cfg(test)]
#[path = "unified_exec_tests.rs"]
mod tests;
