use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

use crate::function_tool::FunctionCallError;
use crate::shell::Shell;
use crate::shell::ShellType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandKind {
    Script,
    Argv,
    PowerShellScript,
}

impl CommandKind {
    fn parse(value: &str) -> Result<Self, FunctionCallError> {
        match value {
            "script" => Ok(Self::Script),
            "argv" => Ok(Self::Argv),
            "powershell_script" => Ok(Self::PowerShellScript),
            other => Err(FunctionCallError::RespondToModel(format!(
                "unsupported command kind `{other}`; use `script`, `argv`, or `powershell_script`."
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandInvocation {
    Script(String),
    Argv { program: String, args: Vec<String> },
    PowerShellScript(String),
}

impl CommandInvocation {
    pub(crate) fn from_parts(
        tool_name: &str,
        script_field: &str,
        script: Option<&str>,
        kind: Option<&str>,
        program: Option<&str>,
        args: Option<&[String]>,
        script_body: Option<&str>,
    ) -> Result<Self, FunctionCallError> {
        let script = script.and_then(non_empty);
        let program = program.and_then(non_empty);
        let script_body = script_body.and_then(non_empty);
        let has_argv_fields = program.is_some() || args.is_some();
        let has_powershell_script_fields = script_body.is_some();
        let kind = match kind {
            Some(kind) => Some(CommandKind::parse(kind)?),
            None if script.is_none() && has_argv_fields => Some(CommandKind::Argv),
            None if script.is_none() && has_powershell_script_fields => {
                Some(CommandKind::PowerShellScript)
            }
            None => None,
        };

        match kind.unwrap_or(CommandKind::Script) {
            CommandKind::Script => {
                if has_argv_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} received argv fields in script mode; omit `program`/`args` or set `kind` to `argv`."
                    )));
                }
                if has_powershell_script_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} received `script_body` in script mode; omit `script_body` or set `kind` to `powershell_script`."
                    )));
                }
                let Some(script) = script else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} requires `{script_field}` for script mode, `kind: \"argv\"` with `program`, or `kind: \"powershell_script\"` with `script_body`."
                    )));
                };
                Ok(Self::Script(script.to_string()))
            }
            CommandKind::Argv => {
                if script.is_some() {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} received `{script_field}` with `kind: \"argv\"`; omit `{script_field}` for direct argv commands."
                    )));
                }
                if has_powershell_script_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} received `script_body` with `kind: \"argv\"`; omit `script_body` for direct argv commands."
                    )));
                }
                let Some(program) = program else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} requires `program` when `kind` is `argv`."
                    )));
                };
                Ok(Self::Argv {
                    program: program.to_string(),
                    args: args.map_or_else(Vec::new, ToOwned::to_owned),
                })
            }
            CommandKind::PowerShellScript => {
                if script.is_some() || has_argv_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} received legacy script or argv fields with `kind: \"powershell_script\"`; use only `script_body`."
                    )));
                }
                let Some(script_body) = script_body else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} requires `script_body` when `kind` is `powershell_script`."
                    )));
                };
                Ok(Self::PowerShellScript(script_body.to_string()))
            }
        }
    }

    pub(crate) fn to_exec_args(&self, shell: &Shell, use_login_shell: bool) -> Vec<String> {
        match self {
            Self::Script(script) => shell.derive_exec_args(script, use_login_shell),
            Self::PowerShellScript(script_body) => {
                self.to_powershell_exec_args(shell, script_body, use_login_shell)
            }
            Self::Argv { program, args } => {
                let mut command = Vec::with_capacity(args.len() + 1);
                command.push(program.clone());
                command.extend(args.iter().cloned());
                command
            }
        }
    }

    pub(crate) fn to_safety_args(&self, shell: &Shell, use_login_shell: bool) -> Vec<String> {
        match self {
            Self::PowerShellScript(script_body) => {
                self.to_powershell_safety_args(shell, script_body, use_login_shell)
            }
            _ => self.to_exec_args(shell, use_login_shell),
        }
    }

    pub(crate) fn display_command(&self) -> String {
        match self {
            Self::Script(script) => script.clone(),
            Self::PowerShellScript(script_body) => script_body.clone(),
            Self::Argv { program, args } => {
                let mut command = Vec::with_capacity(args.len() + 1);
                command.push(program.clone());
                command.extend(args.iter().cloned());
                codex_shell_command::parse_command::shlex_join(&command)
            }
        }
    }

    pub(crate) fn is_argv(&self) -> bool {
        matches!(self, Self::Argv { .. })
    }

    pub(crate) fn is_powershell_script(&self) -> bool {
        matches!(self, Self::PowerShellScript(_))
    }

    fn to_powershell_exec_args(
        &self,
        shell: &Shell,
        script_body: &str,
        use_login_shell: bool,
    ) -> Vec<String> {
        debug_assert_eq!(shell.shell_type, ShellType::PowerShell);
        let mut command = vec![shell.shell_path.to_string_lossy().to_string()];
        command.push("-NoLogo".to_string());
        if !use_login_shell {
            command.push("-NoProfile".to_string());
        }
        command.extend(encoded_command_args(&format!(
            "{}{}",
            codex_shell_command::powershell::UTF8_OUTPUT_PREFIX,
            script_body
        )));
        command
    }

    fn to_powershell_safety_args(
        &self,
        shell: &Shell,
        script_body: &str,
        use_login_shell: bool,
    ) -> Vec<String> {
        debug_assert_eq!(shell.shell_type, ShellType::PowerShell);
        let mut command = vec![shell.shell_path.to_string_lossy().to_string()];
        command.push("-NoLogo".to_string());
        if !use_login_shell {
            command.push("-NoProfile".to_string());
        }
        command.push("-Command".to_string());
        command.push(script_body.to_string());
        command
    }
}

fn encoded_command_args(script: &str) -> [String; 2] {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    ["-EncodedCommand".to_string(), BASE64_STANDARD.encode(utf16)]
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(crate) fn powershell_script_failure_advisory(
    shell_type: Option<ShellType>,
    exit_code: Option<i32>,
    output: &str,
) -> Option<&'static str> {
    if shell_type != Some(ShellType::PowerShell) || exit_code.is_none_or(|code| code == 0) {
        return None;
    }

    let lower = output.to_ascii_lowercase();
    let looks_like_measure_object_failure = lower.contains("measure-object")
        && (lower.contains("cannot bind")
            || lower.contains("parameter")
            || lower.contains("property")
            || lower.contains("scriptblock"));
    if looks_like_measure_object_failure {
        return Some(
            "Hint: PowerShell Measure-Object expects property names for -Property. For computed values, pipe numbers first, for example `... | ForEach-Object { <number> } | Measure-Object -Sum`; for real properties, use `Measure-Object -Property Count -Sum`.",
        );
    }

    let looks_like_parser_or_quoting_failure = [
        "parsererror",
        "unexpected token",
        "missing expression",
        "missing closing",
        "terminator",
        "positionalparameternotfound",
        "parameter cannot be processed",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    looks_like_parser_or_quoting_failure.then_some(
        "Hint: if this failed because of PowerShell quoting or parser handling, retry with `kind: \"powershell_script\"` and `script_body` so Codex encodes the script body instead of nesting quotes.",
    )
}

#[cfg(test)]
#[path = "command_shape_tests.rs"]
mod command_shape_tests;
