use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

fn windows_shell_guidance_description() -> String {
    format!("\n\n{}", windows_shell_guidance())
}

fn has_parameter(tool: &ToolSpec, parameter_name: &str) -> bool {
    serde_json::to_value(tool)
        .expect("tool spec should serialize")
        .pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
}

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = if cfg!(windows) {
        format!(
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction.\n\nExamples:\n- Simple executable: `kind: \"argv\"`, `program: \"rg\"`, `args: [\"--files\"]`.\n- Complex PowerShell: `kind: \"powershell_script\"` with plain `script_body`; do not hand-build `-EncodedCommand`.\n\n{}\n\n{VALIDATION_COMMAND_GUIDANCE}",
            windows_shell_guidance(),
        )
    } else {
        format!(
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction.\n\nExamples:\n- Simple executable: `kind: \"argv\"`, `program: \"rg\"`, `args: [\"--files\"]`.\n- Complex PowerShell: `kind: \"powershell_script\"` with plain `script_body`; do not hand-build `-EncodedCommand`.\n\n{VALIDATION_COMMAND_GUIDANCE}",
        )
    };

    let mut properties = BTreeMap::from([
        (
            "cmd".to_string(),
            JsonSchema::string(Some(
                    "Legacy shell script to execute. Use this for pipes, redirects, variable expansion, here-docs, compound shell syntax, and PowerShell cmdlets. For simple standalone executables, prefer `kind: \"argv\"` with `program` and `args`; for complex PowerShell, prefer `kind: \"powershell_script\"` with `script_body`."
                        .to_string(),
                )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![json!("script"), json!("argv"), json!("powershell_script")],
                Some(
                        "Command encoding. `script` uses the legacy `cmd` shell string; `argv` launches `program` directly with `args`; `powershell_script` launches PowerShell with a runtime-encoded `script_body` to avoid nested quoting."
                            .to_string(),
                    ),
            ),
        ),
        (
            "program".to_string(),
            JsonSchema::string(Some(
                    "Program to launch directly when `kind` is `argv`, for example `rg` or `git`. PowerShell cmdlets are not standalone executables; run them with `kind: \"script\"` or through PowerShell."
                        .to_string(),
                )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some(
                        "Argument vector for direct argv mode. Do not include the program name."
                            .to_string(),
                    ),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                    "Plain PowerShell script text for `kind: \"powershell_script\"`; the runtime encodes it for PowerShell instead of asking the model to quote or base64-encode it."
                        .to_string(),
                )),
        ),
        (
            "workdir".to_string(),
            JsonSchema::string(Some(
                    "Working directory for the command. Defaults to the turn cwd."
                        .to_string(),
                )),
        ),
        (
            "shell".to_string(),
            JsonSchema::string(Some(
                    "Shell binary to launch. Defaults to the user's default shell.".to_string(),
                )),
        ),
        (
            "tty".to_string(),
            JsonSchema::boolean(Some(
                    "True allocates a PTY for the command; false or omitted uses plain pipes."
                        .to_string(),
                )),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                    "Wait before yielding output. Defaults to 10000 ms; effective range is 250-30000 ms.".to_string(),
                )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some(
                    "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy.".to_string(),
                )),
        ),
        (
            "login".to_string(),
            JsonSchema::boolean(Some(
                    "True runs the shell with -l/-i semantics; false disables them. Defaults to true.".to_string(),
                )),
        ),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "exec_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
            output_schema: Some(unified_exec_output_schema()),
        })
    );
}

#[test]
fn exec_command_tool_can_hide_shell_parameter() {
    let tool = create_exec_command_tool_with_environment_id(
        CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        },
        /*include_environment_id*/ false,
        /*include_shell_parameter*/ false,
    );

    assert!(!has_parameter(&tool, "shell"));
    assert!(has_parameter(&tool, "cmd"));
}

#[test]
fn write_stdin_tool_matches_expected_spec() {
    let tool = create_write_stdin_tool();

    let properties = BTreeMap::from([
        (
            "session_id".to_string(),
            JsonSchema::number(Some(
                "Identifier of the running unified exec session.".to_string(),
            )),
        ),
        (
            "chars".to_string(),
            JsonSchema::string(Some(
                "Bytes to write to stdin. Defaults to empty, which polls without writing.".to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number(Some(
                "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms; empty polls wait 5000-300000 ms by default.".to_string(),
            )),
        ),
        (
            "max_output_tokens".to_string(),
            JsonSchema::number(Some(
                "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy.".to_string(),
            )),
        ),
    ]);

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "write_stdin".to_string(),
            description:
                "Writes characters to an existing unified exec session and returns recent output."
                    .to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["session_id".to_string()]),
                Some(false.into())
            ),
            output_schema: Some(unified_exec_output_schema()),
        })
    );
}

#[test]
fn request_permissions_tool_includes_full_permission_schema() {
    let tool =
        create_request_permissions_tool("Request extra permissions for this turn.".to_string());

    let properties = BTreeMap::from([
        (
            "reason".to_string(),
            JsonSchema::string(Some(
                "Optional short explanation for why additional permissions are needed.".to_string(),
            )),
        ),
        (
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>. Omit to use the primary environment."
                    .to_string(),
            )),
        ),
        ("permissions".to_string(), permission_profile_schema()),
    ]);

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "request_permissions".to_string(),
            description: "Request extra permissions for this turn.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["permissions".to_string()]),
                Some(false.into())
            ),
            output_schema: None,
        })
    );
}

#[test]
fn shell_command_tool_matches_expected_spec() {
    let tool = create_shell_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = if cfg!(windows) {
        r#"Runs a Powershell command (Windows) and returns its output.

Examples of valid command strings:

- ls -a (show hidden): "Get-ChildItem -Force"
- recursive find by name: "Get-ChildItem -Recurse -Filter *.py"
- recursive grep: "Get-ChildItem -Path C:\\myrepo -Recurse | Select-String -Pattern 'TODO' -CaseSensitive"
- ps aux | grep python: "Get-Process | Where-Object { $_.ProcessName -like '*python*' }"
- setting an env var: "$env:FOO='bar'; echo $env:FOO"
- running an inline Python script: "@'\\nprint('Hello, world!')\\n'@ | python -""#
            .to_string()
            + &windows_shell_guidance_description()
    } else {
        r#"Runs a shell command and returns its output.
- Always set the `workdir` param when using the shell_command function. Do not use `cd` unless absolutely necessary."#
            .to_string()
    };

    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some(
                "Legacy shell script to run in the user's default shell. Use this for pipes, redirects, variable expansion, here-docs, compound shell syntax, and PowerShell cmdlets. For simple standalone executables, prefer `kind: \"argv\"` with `program` and `args`; for complex PowerShell, prefer `kind: \"powershell_script\"` with `script_body`."
                    .to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![json!("script"), json!("argv"), json!("powershell_script")],
                Some(
                    "Command encoding. `script` uses the legacy `command` shell string; `argv` launches `program` directly with `args`; `powershell_script` launches PowerShell with a runtime-encoded `script_body` to avoid nested quoting."
                        .to_string(),
                ),
            ),
        ),
        (
            "program".to_string(),
            JsonSchema::string(Some(
                "Program to launch directly when `kind` is `argv`, for example `rg` or `git`. PowerShell cmdlets are not standalone executables; run them with `kind: \"script\"` or through PowerShell."
                    .to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some(
                    "Argument vector for direct argv mode. Do not include the program name."
                        .to_string(),
                ),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                "Plain PowerShell script text for `kind: \"powershell_script\"`; the runtime encodes it for PowerShell instead of asking the model to quote or base64-encode it."
                    .to_string(),
            )),
        ),
        (
            "workdir".to_string(),
            JsonSchema::string(Some(
                "Working directory for the command. Defaults to the turn cwd.".to_string(),
            )),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::number(Some(
                "Maximum command runtime. Defaults to 10000 ms.".to_string(),
            )),
        ),
        (
            "login".to_string(),
            JsonSchema::boolean(Some(
                "True runs with login shell semantics; false disables them. Defaults to true."
                    .to_string(),
            )),
        ),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "shell_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(properties, None, Some(false.into())),
            output_schema: None,
        })
    );
}
