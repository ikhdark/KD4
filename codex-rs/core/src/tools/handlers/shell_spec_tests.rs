use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn windows_shell_guidance_description() -> String {
    format!("\n\n{}", windows_shell_guidance())
}

fn has_parameter(tool: &ToolSpec, parameter_name: &str) -> bool {
    let tool = serde_json::to_value(tool).expect("tool spec should serialize");
    tool.pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
        || tool
            .pointer("/parameters/oneOf")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|branches| {
                branches.iter().any(|branch| {
                    branch
                        .pointer(&format!("/properties/{parameter_name}"))
                        .is_some()
                })
            })
}

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = format!(
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction.{}",
        windows_shell_guidance_description()
    );

    let mut properties = BTreeMap::from([
        (
            "cmd".to_string(),
            JsonSchema::string(Some(
                "Legacy shell script to execute. Use this only when shell semantics are required, including PowerShell cmdlets, variables or interpolation, pipelines or redirection, here-docs, compound statements, shell builtins, and `.cmd`/`.bat` semantics. When a standalone native executable and separated arguments are already known, use `kind: \"argv\"` with `program` and `args` instead; do not serialize them into this string field. This includes Git (`git`), ripgrep (`rg`), Cargo (`cargo`), Node (`node`), Python (`python`), and KD4 helper executables such as `kds`. Examples: `git` with `[\"status\", \"--short\"]`; `rg` with `[\"--files\"]`; `cargo` with `[\"test\", \"-p\", \"codex-core\"]`; `node` with `[\"script.js\"]`; `python` with `[\"-m\", \"pytest\"]`; `kds` with `[\"--help\"]`. Arbitrary command strings remain shell scripts and must not be heuristically split. For complex PowerShell, prefer `kind: \"powershell_script\"`. If shell inspection is necessary, keep read-only PowerShell to direct cmdlet pipelines without variables, loops, or script blocks so it can remain outside the repository mutation lane."
                    .to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("legacy"),
                    json!("script"),
                    json!("argv"),
                    json!("powershell_script"),
                ],
                Some(
                    "Command encoding. `legacy` preserves the historical untagged `cmd` string; `script` explicitly uses `cmd`; `argv` launches `program` directly with `args`; `powershell_script` runtime-encodes `script_body`."
                        .to_string(),
                ),
            ),
        ),
        (
            "program".to_string(),
            JsonSchema::string(Some(
                "Executable to launch directly when `kind` is `argv`.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some("Arguments for direct argv mode, excluding the program name.".to_string()),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                "Plain PowerShell script for `kind: \"powershell_script\"`; Codex encodes it at runtime. Read-only PowerShell that uses variables, loops, or script blocks may require the repository mutation lane."
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
            bounded_integer(
                "Wait before yielding output. Defaults to 2000 ms; effective range is 250-30000 ms.".to_string(),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::MAX_YIELD_TIME_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(format!(
                "Output token budget. {}; larger requests may be capped by policy.",
                codex_utils_output_truncation::adaptive_output_budget_description()
            ), 0, usize::MAX as u64),
        ),
        (
            "login".to_string(),
            JsonSchema::boolean(Some(
                    "True runs the shell with -l/-i semantics; false disables them. Defaults to true.".to_string(),
                )),
        ),
        ("validation".to_string(), validation_context_schema()),
    ]);
    properties.extend(create_approval_parameters(
        /*exec_permission_approvals_enabled*/ false,
    ));
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(
            "Execute without reusing prior immutable evidence.".to_string(),
        )),
    );

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "exec_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: command_parameters_schema(properties, "cmd"),
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
            bounded_integer(
                "Identifier of the running unified exec session.".to_string(),
                0,
                u32::MAX as u64,
            ),
        ),
        (
            "chars".to_string(),
            JsonSchema::string(Some(
                "Bytes to write to stdin. Defaults to empty, which polls without writing.".to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            bounded_integer(
                "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms; empty polls default to one event-driven 60000 ms wait. A wait deadline does not terminate the process.".to_string(),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(format!(
                "Output token budget. {}; larger requests may be capped by policy.",
                codex_utils_output_truncation::adaptive_output_budget_description()
            ), 0, usize::MAX as u64),
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

    let description = r#"Runs a Powershell command (Windows) and returns its output.

Examples of valid command strings:

- ls -a (show hidden): "Get-ChildItem -Force"
- recursive find by name: "Get-ChildItem -Recurse -Filter *.py"
- recursive grep: "Get-ChildItem -Path C:\\myrepo -Recurse | Select-String -Pattern 'TODO' -CaseSensitive"
- ps aux | grep python: "Get-Process | Where-Object { $_.ProcessName -like '*python*' }"
- setting an env var: "$env:FOO='bar'; echo $env:FOO"
- running an inline Python script: "@'\\nprint('Hello, world!')\\n'@ | python -""#
            .to_string()
            + &windows_shell_guidance_description();

    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some(
                "Legacy shell script to execute. Use this only when shell semantics are required, including PowerShell cmdlets, variables or interpolation, pipelines or redirection, here-docs, compound statements, shell builtins, and `.cmd`/`.bat` semantics. When a standalone native executable and separated arguments are already known, use `kind: \"argv\"` with `program` and `args` instead; do not serialize them into this string field. This includes Git (`git`), ripgrep (`rg`), Cargo (`cargo`), Node (`node`), Python (`python`), and KD4 helper executables such as `kds`. Examples: `git` with `[\"status\", \"--short\"]`; `rg` with `[\"--files\"]`; `cargo` with `[\"test\", \"-p\", \"codex-core\"]`; `node` with `[\"script.js\"]`; `python` with `[\"-m\", \"pytest\"]`; `kds` with `[\"--help\"]`. Arbitrary command strings remain shell scripts and must not be heuristically split. For complex PowerShell, prefer `kind: \"powershell_script\"`. If shell inspection is necessary, keep read-only PowerShell to direct cmdlet pipelines without variables, loops, or script blocks so it can remain outside the repository mutation lane."
                    .to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![
                    json!("legacy"),
                    json!("script"),
                    json!("argv"),
                    json!("powershell_script"),
                ],
                Some(
                    "Command encoding. `legacy` preserves the historical untagged `command` string; `script` explicitly uses `command`; `argv` launches `program` directly with `args`; `powershell_script` runtime-encodes `script_body`."
                        .to_string(),
                ),
            ),
        ),
        (
            "program".to_string(),
            JsonSchema::string(Some(
                "Executable to launch directly when `kind` is `argv`.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some("Arguments for direct argv mode, excluding the program name.".to_string()),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                "Plain PowerShell script for `kind: \"powershell_script\"`; Codex encodes it at runtime. Read-only PowerShell that uses variables, loops, or script blocks may require the repository mutation lane."
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
            bounded_integer(
                "Maximum command runtime. Defaults to 10000 ms.".to_string(),
                0,
                u64::MAX,
            ),
        ),
        (
            "stall_timeout_ms".to_string(),
            bounded_integer(
                "Optional maximum time without stdout or stderr progress before cancellation. Omit or set zero to disable the stall deadline."
                    .to_string(),
                0,
                u64::MAX,
            ),
        ),
        ("validation".to_string(), validation_context_schema()),
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
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(
            "Execute without reusing prior immutable evidence.".to_string(),
        )),
    );

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "shell_command".to_string(),
            description,
            strict: false,
            defer_loading: None,
            parameters: command_parameters_schema(properties, "command"),
            output_schema: None,
        })
    );
}

#[test]
fn command_tools_advertise_only_mutually_exclusive_explicit_forms() {
    for tool in [
        create_exec_command_tool(CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        }),
        create_shell_command_tool(CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        }),
    ] {
        let tool = serde_json::to_value(tool).expect("serialize command tool");
        let branches = tool
            .pointer("/parameters/oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("command form branches");
        assert_eq!(branches.len(), 3);
        let kinds = branches
            .iter()
            .map(|branch| branch["properties"]["kind"]["enum"][0].clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![json!("script"), json!("argv"), json!("powershell_script")]
        );
        assert!(
            branches
                .iter()
                .all(|branch| branch["additionalProperties"] == false)
        );
        assert!(branches.iter().all(|branch| {
            branch["required"]
                .as_array()
                .is_some_and(|required| required.contains(&json!("kind")))
        }));
        assert!(
            branches
                .iter()
                .all(|branch| branch["properties"]["kind"]["enum"] != json!(["legacy"]))
        );
    }
}

#[test]
fn integer_arguments_expose_destination_types_and_runtime_bounds() {
    let exec = serde_json::to_value(create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    }))
    .expect("serialize exec tool");
    let exec_yield = &exec["parameters"]["oneOf"][0]["properties"]["yield_time_ms"];
    assert_eq!(exec_yield["type"], "integer");
    assert_eq!(
        exec_yield["minimum"],
        crate::unified_exec::MIN_YIELD_TIME_MS
    );
    assert_eq!(
        exec_yield["maximum"],
        crate::unified_exec::MAX_YIELD_TIME_MS
    );

    let write = serde_json::to_value(create_write_stdin_tool()).expect("serialize write tool");
    assert_eq!(
        write["parameters"]["properties"]["session_id"]["type"],
        "integer"
    );
    assert_eq!(
        write["parameters"]["properties"]["session_id"]["maximum"],
        u32::MAX
    );
    assert_eq!(
        write["parameters"]["properties"]["yield-time_ms"]["maximum"],
        crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS
    );
}
