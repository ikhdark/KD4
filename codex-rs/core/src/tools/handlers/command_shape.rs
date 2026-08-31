use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::FunctionCallError;
use crate::shell::Shell;
use crate::shell::ShellType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
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
                "command schema error in branch selection at `$.kind`: actual value `{other}` is unsupported; use canonical `script`, `argv`, or `powershell_script`. For the legacy form, omit `kind` and provide the script in the legacy command field."
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
        let script = script.and_then(non_blank);
        let program = program.and_then(non_blank);
        let script_body = script_body.and_then(non_blank);
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
                        "{tool_name} schema error in `script` branch at `$.program`/`$.args`: argv fields were supplied with `{script_field}` or `kind: \"script\"`; omit them, or use only `kind: \"argv\"`, `program`, and optional `args`."
                    )));
                }
                if has_powershell_script_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `script` branch at `$.script_body`: this field belongs to `kind: \"powershell_script\"`; omit it, or use only `kind: \"powershell_script\"` and `script_body`."
                    )));
                }
                let Some(script) = script else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `script` branch at `$.{script_field}`: the field is required and must be a non-blank string. Alternatively use `kind: \"argv\"` with `program`, or `kind: \"powershell_script\"` with `script_body`."
                    )));
                };
                Ok(Self::Script(script.to_string()))
            }
            CommandKind::Argv => {
                if script.is_some() {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `argv` branch at `$.{script_field}`: script text cannot be mixed with `kind: \"argv\"`; omit `$.{script_field}`."
                    )));
                }
                if has_powershell_script_fields {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `argv` branch at `$.script_body`: PowerShell script text cannot be mixed with `kind: \"argv\"`; omit `$.script_body`."
                    )));
                }
                let Some(program) = program else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `argv` branch at `$.program`: the field is required and must be a non-blank string."
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
                        "{tool_name} schema error in `powershell_script` branch at `$.{script_field}`/`$.program`/`$.args`: those fields cannot be mixed with this branch; use only `kind: \"powershell_script\"` and `script_body`."
                    )));
                }
                let Some(script_body) = script_body else {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{tool_name} schema error in `powershell_script` branch at `$.script_body`: the field is required and must be a non-blank string."
                    )));
                };
                Ok(Self::PowerShellScript(script_body.to_string()))
            }
        }
    }

    pub(crate) fn to_exec_args(
        &self,
        shell: &Shell,
        use_login_shell: bool,
    ) -> Result<Vec<String>, FunctionCallError> {
        match self {
            Self::Script(script) => shell
                .derive_exec_args(script, use_login_shell)
                .map_err(|error| FunctionCallError::RespondToModel(error.to_string())),
            Self::PowerShellScript(script_body) => {
                Ok(self.to_powershell_exec_args(shell, script_body, use_login_shell))
            }
            Self::Argv { program, args } => {
                let mut command = Vec::with_capacity(args.len() + 1);
                command.push(program.clone());
                command.extend(args.iter().cloned());
                Ok(command)
            }
        }
    }

    pub(crate) fn to_safety_args(
        &self,
        shell: &Shell,
        use_login_shell: bool,
    ) -> Result<Vec<String>, FunctionCallError> {
        match self {
            Self::PowerShellScript(script_body) => {
                Ok(self.to_powershell_safety_args(shell, script_body, use_login_shell))
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

    pub(crate) fn hook_input(&self) -> Value {
        match self {
            Self::Script(script) => json!({ "command": script }),
            Self::Argv { program, args } => json!({
                "command": self.display_command(),
                "kind": "argv",
                "program": program,
                "args": args,
            }),
            Self::PowerShellScript(script_body) => json!({
                "command": script_body,
                "kind": "powershell_script",
                "script_body": script_body,
            }),
        }
    }

    pub(crate) fn with_updated_hook_input(
        &self,
        tool_name: &str,
        updated_input: &Value,
    ) -> Result<Self, FunctionCallError> {
        let Value::Object(updated_input) = updated_input else {
            return Err(FunctionCallError::RespondToModel(
                "hook returned updatedInput that is not an object".to_string(),
            ));
        };
        let command = optional_string(updated_input, "command")?;
        let kind = optional_string(updated_input, "kind")?;
        let program = optional_string(updated_input, "program")?;
        let args = optional_string_array(updated_input, "args")?;
        let script_body = optional_string(updated_input, "script_body")?;

        match self {
            Self::Script(_) => {
                if !matches!(kind, None | Some("script"))
                    || program.is_some()
                    || args.is_some()
                    || script_body.is_some()
                {
                    return Err(shape_change_error(tool_name, "script"));
                }
                let command = command.ok_or_else(missing_updated_command)?;
                Ok(Self::Script(command.to_string()))
            }
            Self::PowerShellScript(_) => {
                if !matches!(kind, None | Some("powershell_script"))
                    || program.is_some()
                    || args.is_some()
                    || (script_body.is_some() && kind.is_none())
                {
                    return Err(shape_change_error(tool_name, "powershell_script"));
                }
                let updated_script = match (command, script_body) {
                    (Some(command), Some(script_body)) if command != script_body => {
                        return Err(FunctionCallError::RespondToModel(format!(
                            "{tool_name} hook returned conflicting `command` and `script_body` values for a PowerShell script."
                        )));
                    }
                    (Some(command), _) => command,
                    (None, Some(script_body)) => script_body,
                    (None, None) => {
                        return Err(FunctionCallError::RespondToModel(
                            "hook returned updatedInput without string field `command` or structured PowerShell field `script_body`".to_string(),
                        ));
                    }
                };
                Ok(Self::PowerShellScript(updated_script.to_string()))
            }
            Self::Argv { .. } => {
                if script_body.is_some() {
                    return Err(shape_change_error(tool_name, "argv"));
                }
                match kind {
                    None if program.is_none() && args.is_none() => {
                        let command = command.ok_or_else(missing_updated_command)?;
                        if command == self.display_command() {
                            Ok(self.clone())
                        } else {
                            Err(FunctionCallError::RespondToModel(format!(
                                "{tool_name} hook cannot rewrite a direct argv command as text because that would lose structured `program`/`args`; return structured `kind`, `program`, and `args`, return the original `command` value, or block the tool call instead."
                            )))
                        }
                    }
                    Some("argv") => {
                        let updated = Self::from_parts(
                            tool_name,
                            "command",
                            None,
                            Some("argv"),
                            program,
                            args.as_deref(),
                            None,
                        )?;
                        if command.is_some_and(|command| command != updated.display_command()) {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "{tool_name} hook returned a `command` value that does not match its structured `program`/`args` rewrite."
                            )));
                        }
                        Ok(updated)
                    }
                    _ => Err(shape_change_error(tool_name, "argv")),
                }
            }
        }
    }

    pub(crate) fn is_argv(&self) -> bool {
        matches!(self, Self::Argv { .. })
    }

    pub(crate) fn to_direct_argv(&self) -> Option<Vec<String>> {
        let Self::Argv { program, args } = self else {
            return None;
        };
        let mut command = Vec::with_capacity(args.len() + 1);
        command.push(program.clone());
        command.extend(args.iter().cloned());
        Some(command)
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
        let mut command = powershell_base_args(shell, use_login_shell);
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
        let mut command = powershell_base_args(shell, use_login_shell);
        command.push("-Command".to_string());
        command.push(script_body.to_string());
        command
    }
}

fn powershell_base_args(shell: &Shell, use_login_shell: bool) -> Vec<String> {
    let mut command = vec![shell.shell_path.to_string_lossy().to_string()];
    command.push("-NoLogo".to_string());
    if !use_login_shell {
        command.push("-NoProfile".to_string());
    }
    command
}

fn encoded_command_args(script: &str) -> [String; 2] {
    let mut utf16 = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    ["-EncodedCommand".to_string(), BASE64_STANDARD.encode(utf16)]
}

fn non_blank(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn optional_string<'a>(
    input: &'a Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, FunctionCallError> {
    match input.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(FunctionCallError::RespondToModel(format!(
            "hook returned updatedInput with non-string field `{field}`"
        ))),
    }
}

fn optional_string_array(
    input: &Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<String>>, FunctionCallError> {
    match input.get(field) {
        None => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
                    FunctionCallError::RespondToModel(format!(
                        "hook returned updatedInput with non-string item in `{field}`"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(FunctionCallError::RespondToModel(format!(
            "hook returned updatedInput with non-array field `{field}`"
        ))),
    }
}

fn missing_updated_command() -> FunctionCallError {
    FunctionCallError::RespondToModel(
        "hook returned updatedInput without string field `command`".to_string(),
    )
}

fn shape_change_error(tool_name: &str, original_kind: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "{tool_name} hook cannot change command shape from `{original_kind}`; return updatedInput with the same command kind or block the tool call instead."
    ))
}

pub(crate) fn powershell_script_failure_advisory(
    shell_type: Option<ShellType>,
    exit_code: Option<i32>,
    is_powershell_script: bool,
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

    (!is_powershell_script && looks_like_parser_or_quoting_failure).then_some(
        "Hint: if this failed because of PowerShell quoting or parser handling, retry with `kind: \"powershell_script\"` and `script_body` so Codex encodes the script body instead of nesting quotes.",
    )
}

#[cfg(test)]
#[path = "command_shape_tests.rs"]
mod command_shape_tests;
