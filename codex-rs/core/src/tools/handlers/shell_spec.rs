use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::adaptive_output_budget_description;
use serde_json::Number;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

fn validation_context_schema() -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([(
            "covered_paths".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some(
                    "Non-empty repository-relative scopes attributed to this validation result."
                        .to_string(),
                ),
            ),
        )]),
        Some(vec!["covered_paths".to_string()]),
        Some(false.into()),
    );
    schema.description = Some("Optional validation scope metadata.".to_string());
    schema
}

const LEGACY_SHELL_SCRIPT_DESCRIPTION: &str = "Shell script to execute in the user's default shell. For a standalone native executable with known arguments, you may use `kind: \"argv\"` with `program` and `args`. Keep pipelines, redirection, shell expansion, compound statements, and builtins in script form. On Windows, also keep `.cmd`/`.bat` calls in script form; for complex PowerShell, prefer `kind: \"powershell_script\"`.";

const FORCE_FRESH_DESCRIPTION: &str = "Force execution instead of reusing equivalent evidence. By default, unchanged file reads, searches, and deterministic failures may reuse a prior result; reused results are labeled. Set true when external state changed or a fresh observation is required.";

fn bounded_integer(description: String, minimum: u64, maximum: u64) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum)),
        maximum: Some(Number::from(maximum.min((1_u64 << 53) - 1))),
        ..JsonSchema::integer(Some(description))
    }
}

fn command_parameters_schema(
    mut properties: BTreeMap<String, JsonSchema>,
    script_field: &str,
) -> JsonSchema {
    // The runtime decoder (`CommandInvocation::from_parts`) accepts the
    // historical untagged script string and infers `argv` or
    // `powershell_script` from their fields, so the advertised and
    // preflight-enforced schema must accept that surface. Require a command
    // field, while leaving conflicting combinations to the decoder's
    // prescriptive field-level messages.
    properties.insert(
        "kind".to_string(),
        JsonSchema::string_enum(
            vec![
                json!("script"),
                json!("argv"),
                json!("powershell_script"),
            ],
            Some(format!(
                "Canonical command encoding. `script` explicitly uses `{script_field}`; `argv` launches `program` directly with `args`; `powershell_script` runtime-encodes `script_body`. Legacy input remains supported by omitting `kind`; the runtime infers the branch from the single populated command field and normalizes it immediately."
            )),
        ),
    );
    JsonSchema {
        any_of: Some(
            [script_field, "program", "script_body"]
                .into_iter()
                .map(|field| JsonSchema {
                    required: Some(vec![field.to_string()]),
                    ..JsonSchema::object(BTreeMap::new(), None, None)
                })
                .collect(),
        ),
        ..JsonSchema::object(properties, /*required*/ None, Some(false.into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandToolOptions {
    pub allow_login_shell: bool,
    pub exec_permission_approvals_enabled: bool,
}

#[cfg(test)]
pub fn create_exec_command_tool(options: CommandToolOptions) -> ToolSpec {
    create_exec_command_tool_with_environment_id(
        options, /*include_environment_id*/ false, /*include_shell_parameter*/ true,
    )
}

#[cfg(test)]
pub(crate) fn create_exec_command_tool_with_environment_id(
    options: CommandToolOptions,
    include_environment_id: bool,
    include_shell_parameter: bool,
) -> ToolSpec {
    create_exec_command_tool_for_policy(
        options,
        include_environment_id,
        include_shell_parameter,
        /*allow_escalated_sandbox_permissions*/ true,
    )
}

pub(crate) fn create_exec_command_tool_for_policy(
    options: CommandToolOptions,
    include_environment_id: bool,
    include_shell_parameter: bool,
    allow_escalated_sandbox_permissions: bool,
) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "cmd".to_string(),
            JsonSchema::string(Some(LEGACY_SHELL_SCRIPT_DESCRIPTION.to_string())),
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
                "Plain PowerShell script for `kind: \"powershell_script\"`; Codex encodes it at runtime."
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
            "tty".to_string(),
            JsonSchema::boolean(Some(
                "True allocates a PTY for the command; false or omitted uses plain pipes."
                    .to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            bounded_integer(
                format!("Wait before yielding output. Defaults to 30000 ms for recognized validation commands and 2000 ms otherwise; explicit values use 250-{} ms. On Windows, waits are floored to {} ms only while the executor is not ready; commands that finish sooner return immediately. Nested calls may yield up to 2000 ms before their wrapper deadline to return a live session handle.", crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS, crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(format!(
                "Output token budget. {}; larger requests may be capped by policy. Zero requests a zero-token text budget; command lifecycle and recovery metadata are still returned.",
                adaptive_output_budget_description()
            ), 0, usize::MAX as u64),
        ),
        (
            "validation".to_string(),
            validation_context_schema(),
        ),
    ]);
    if include_shell_parameter {
        properties.insert(
            "shell".to_string(),
            JsonSchema::string(Some(
                "Shell binary to launch. Defaults to the user's default shell.".to_string(),
            )),
        );
    }
    if options.allow_login_shell {
        properties.insert(
            "login".to_string(),
            JsonSchema::boolean(Some(
                "True runs the shell with -l/-i semantics; false disables them. Defaults to true."
                    .to_string(),
            )),
        );
    }
    if include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Environment id from <environment_context>. Omit to use the primary environment."
                    .to_string(),
            )),
        );
    }
    properties.extend(create_approval_parameters(
        options.exec_permission_approvals_enabled,
        allow_escalated_sandbox_permissions,
    ));
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(FORCE_FRESH_DESCRIPTION.to_string())),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: "exec_command".to_string(),
        description: format!(
            "Runs a command, returning output or a session ID for ongoing interaction. Resume a returned session_id with write_stdin; do not restart the command while it is live or its effects are uncertain. For commands needing no shell interpretation, you may use program and args (kind: argv). Use kind: powershell_script with script_body for PowerShell semantics. Keep pipelines, redirections, and shell expansion in script form.\n\n{}\n\n{}",
            rg_search_admission_guidance(),
            filesystem_safety_guidance(),
        ),
        strict: false,
        defer_loading: None,
        parameters: command_parameters_schema(properties, "cmd"),
        output_schema: Some(unified_exec_output_schema()),
    })
}

#[expect(
    clippy::expect_used,
    reason = "create_exec_command_tool_for_policy always constructs command properties containing cmd"
)]
pub(crate) fn create_foreign_shell_command_tool(
    options: CommandToolOptions,
    allow_escalated_sandbox_permissions: bool,
) -> ToolSpec {
    let ToolSpec::Function(mut tool) = create_exec_command_tool_for_policy(
        options,
        false,
        true,
        allow_escalated_sandbox_permissions,
    ) else {
        unreachable!("exec_command has a function schema")
    };
    tool.name = "shell_command".to_string();
    let mut properties = tool
        .parameters
        .properties
        .take()
        .expect("command properties");
    let command = properties.remove("cmd").expect("script command");
    properties.insert("command".to_string(), command);
    tool.parameters = command_parameters_schema(properties, "command");
    ToolSpec::Function(tool)
}

#[cfg(test)]
pub fn create_write_stdin_tool() -> ToolSpec {
    create_write_stdin_tool_with_max_timeout(
        crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS,
    )
}

pub(crate) fn create_write_stdin_tool_with_max_timeout(max_timeout_ms: u64) -> ToolSpec {
    let max_timeout_ms = max_timeout_ms.max(crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS);
    let default_timeout_ms =
        crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS.min(max_timeout_ms);
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
                "Bytes to write to stdin. Defaults to empty, which polls without writing."
                    .to_string(),
            )),
        ),
        (
            "yield_time_ms".to_string(),
            bounded_integer(
                format!(
                    "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms. Empty polls default to {default_timeout_ms} ms and cap at {max_timeout_ms} ms; explicit shorter waits are honored down to 250 ms. A wait deadline does not terminate the process."
                ),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                max_timeout_ms.max(crate::unified_exec::MAX_YIELD_TIME_MS),
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(
                format!(
                    "Output token budget. {}; larger requests may be capped by policy. Zero requests a zero-token text budget; command lifecycle and recovery metadata are still returned.",
                    adaptive_output_budget_description()
                ),
                0,
                usize::MAX as u64,
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "write_stdin".to_string(),
        description:
            "Writes characters to an existing unified exec session and returns recent output. Use only a session_id returned by exec_command or its shell_command compatibility route. Inspect session_capabilities first: send characters only with stdin=true, Ctrl-C by sending `chars: \"\\u0003\"` only when session_capabilities.interrupt=true, and empty input to poll with polling=true. `interrupt` is a returned capability, not an input parameter. cancellation describes explicit process cancellation through this tool; false means no such operation is exposed. Stop when no session_id is returned. Poll again only for an identified pending transition; do not restart the command while it is live or its effects are uncertain."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["session_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(unified_exec_output_schema()),
    })
}

#[cfg(test)]
pub fn create_shell_command_tool(options: CommandToolOptions) -> ToolSpec {
    create_shell_command_tool_for_policy(options, /*allow_escalated_sandbox_permissions*/ true)
}

pub(crate) fn create_shell_command_tool_for_policy(
    options: CommandToolOptions,
    allow_escalated_sandbox_permissions: bool,
) -> ToolSpec {
    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some(LEGACY_SHELL_SCRIPT_DESCRIPTION.to_string())),
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
                "Plain PowerShell script for `kind: \"powershell_script\"`; Codex encodes it at runtime."
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
                "Maximum command runtime. Defaults to 300000 ms for recognized validation commands and 10000 ms otherwise. This is a hard deadline; use exec_command for resumable long-running work. Zero sets an immediate deadline; it does not disable the timeout.".to_string(),
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
        (
            "validation".to_string(),
            validation_context_schema(),
        ),
    ]);
    if options.allow_login_shell {
        properties.insert(
            "login".to_string(),
            JsonSchema::boolean(Some(
                "True runs with login shell semantics; false disables them. Defaults to true."
                    .to_string(),
            )),
        );
    }
    properties.extend(create_approval_parameters(
        options.exec_permission_approvals_enabled,
        allow_escalated_sandbox_permissions,
    ));
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(FORCE_FRESH_DESCRIPTION.to_string())),
    );

    let description = format!(
        "Runs a command in the user's default shell and returns its output. The native route returns text with command status and output, or structured validation evidence, without a resumable session_id. Its output budget is policy-controlled; max_output_tokens is not accepted. Use syntax supported by that shell. For commands needing no shell interpretation, you may use program and args (kind: argv). Use kind: powershell_script with script_body for PowerShell semantics.\n\n{}\n\n{}",
        rg_search_admission_guidance(),
        filesystem_safety_guidance(),
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "shell_command".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: command_parameters_schema(properties, "command"),
        output_schema: None,
    })
}

pub fn create_request_permissions_tool(description: String) -> ToolSpec {
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

    ToolSpec::Function(ResponsesApiTool {
        name: "request_permissions".to_string(),
        description,
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["permissions".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(
            codex_protocol::request_permissions::RequestPermissionsResponse::output_schema(),
        ),
    })
}

pub fn request_permissions_tool_description() -> String {
    "Request additional filesystem or network permissions from the user and wait for the client to grant a subset of the requested permission profile. Use environment_id to target a specific attached environment; omit it to use the primary environment. Relative filesystem paths resolve against the selected environment cwd. Read the returned permissions and scope: a request can be partially granted or denied, and only the returned permissions are authorized. Granted permissions apply automatically to later shell-like commands in the current turn, or for the rest of the session if the client approves them at session scope."
        .to_string()
}

fn unified_exec_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_capabilities": {
                "type": "object",
                "properties": {
                    "stdin": {"type": "boolean"},
                    "interrupt": {"type": "boolean"},
                    "cancellation": {"type": "boolean"},
                    "polling": {"type": "boolean"}
                },
                "required": ["stdin", "interrupt", "cancellation", "polling"],
                "additionalProperties": false
            },
            "chunk_id": {
                "type": "string",
                "description": "Chunk identifier included when the response reports one."
            },
            "wall_time_seconds": {
                "type": "number",
                "description": "Elapsed wall time spent waiting for output in seconds."
            },
            "exit_code": {
                "type": ["integer", "null"],
                "description": "Terminal exit code; null when still running or when the exit code is unavailable. Consult execution_state."
            },
            "execution_state": {
                "type": "string",
                "enum": ["running", "exited", "unknown"],
                "description": "Command process state, independent of the exec cell. Unknown explicitly means no live handle or terminal outcome is available."
            },
            "process_exited": { "type": "boolean" },
            "output_complete": { "type": "boolean" },
            "output_reduced": { "type": "boolean" },
            "original_token_count_is_approximate": { "type": "boolean" },
            "raw_output_artifact_retention_limit_hit": { "type": "boolean" },
            "raw_output_artifact_retention_limit_reason": { "type": "string" },
            "output_decoding_notice": { "type": "string" },
            "validation": {
                "type": "object",
                "properties": {
                    "covered_paths": { "type": "array", "items": { "type": "string" } },
                    "coverage_status": { "const": "unverified" }
                },
                "required": ["covered_paths", "coverage_status"],
                "additionalProperties": false
            },
            "pending_deferred_completions": {
                "type": "array", "items": { "type": "string" }
            },
            "session_id": {
                "type": "integer",
                "description": "Continuation handle for write_stdin. If execution_state is exited, use it to drain pending output; it does not mean the process is still running."
            },
            "original_token_count": {
                "type": "integer",
                "description": "Approximate token count before output truncation."
            },
            "raw_output_artifact_id": {
                "type": "string",
                "description": "Opaque retained-output locator accepted verbatim as read_tool_output.artifact_id."
            },
            "raw_output_artifact_bytes": {
                "type": "integer",
                "description": "Cumulative bytes retained in the raw output artifact."
            },
            "raw_output_artifact_error": {
                "type": "string",
                "description": "Artifact persistence failure, when retention was unavailable."
            },
            "repair": {
                "type": "string",
                "description": "One pre-execution read-only equivalent repair applied to the command."
            },
            "output": {
                "type": "string",
                "description": "Command output text, possibly truncated."
            }
        },
        "required": ["wall_time_seconds", "output", "execution_state", "process_exited", "exit_code", "output_complete", "output_reduced", "raw_output_artifact_retention_limit_hit"],
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "execution_state": {"const": "running"},
                    "process_exited": {"const": false},
                    "exit_code": {"type": "null"},
                    "output_complete": {"const": false}
                },
                "required": ["session_id"]
            },
            {
                "type": "object",
                "properties": {
                    "execution_state": {"const": "exited"},
                    "process_exited": {"const": true}
                }
            },
            {
                "type": "object",
                "properties": {
                    "execution_state": {"const": "unknown"},
                    "process_exited": {"const": false},
                    "exit_code": {"type": "null"},
                    "output_complete": {"const": false}
                },
                "not": {"required": ["session_id"]}
            }
        ],
        "additionalProperties": false
    })
}

fn create_approval_parameters(
    exec_permission_approvals_enabled: bool,
    allow_escalated_sandbox_permissions: bool,
) -> BTreeMap<String, JsonSchema> {
    let mut sandbox_permission_values = vec![json!("use_default")];
    if exec_permission_approvals_enabled {
        sandbox_permission_values.push(json!("with_additional_permissions"));
    }
    if allow_escalated_sandbox_permissions {
        sandbox_permission_values.push(json!("require_escalated"));
    }
    let sandbox_permissions_description = match (
        exec_permission_approvals_enabled,
        allow_escalated_sandbox_permissions,
    ) {
        (true, true) => {
            "Per-command sandbox override. Defaults to `use_default`; use `with_additional_permissions` with `additional_permissions`, or `require_escalated` for unsandboxed execution."
        }
        (true, false) => {
            "Per-command sandbox override. Defaults to `use_default`; use `with_additional_permissions` with `additional_permissions`."
        }
        (false, true) => {
            "Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for unsandboxed execution."
        }
        (false, false) => "Per-command sandbox override. Defaults to `use_default`.",
    };

    let mut properties = BTreeMap::from([(
        "sandbox_permissions".to_string(),
        JsonSchema::string_enum(
            sandbox_permission_values,
            Some(sandbox_permissions_description.to_string()),
        ),
    )]);

    if allow_escalated_sandbox_permissions {
        properties.extend([
            (
            "justification".to_string(),
            JsonSchema::string(Some(
                "User-facing approval question for `require_escalated`; omit otherwise.".to_string(),
            )),
            ),
            (
                "prefix_rule".to_string(),
                JsonSchema::array(JsonSchema::string(/*description*/ None), Some(
                    r#"Reusable approval prefix for `cmd`, only with `sandbox_permissions: "require_escalated"`; for example ["git", "pull"]."#.to_string(),
                )),
            ),
        ]);
    }

    if exec_permission_approvals_enabled {
        let mut additional_permissions = permission_profile_schema();
        additional_permissions.description = Some(
            "Sandboxed filesystem or network access for this command; only with `sandbox_permissions: \"with_additional_permissions\"`."
                .to_string(),
        );
        properties.insert("additional_permissions".to_string(), additional_permissions);
    }

    properties
}

fn permission_profile_schema() -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            ("network".to_string(), network_permissions_schema()),
            ("file_system".to_string(), file_system_permissions_schema()),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = Some("Filesystem or network access request.".to_string());
    schema
}

fn network_permissions_schema() -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([(
            "enabled".to_string(),
            JsonSchema::boolean(Some(
                "True requests network access; false or omitted requests none.".to_string(),
            )),
        )]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = Some("Network access request.".to_string());
    schema
}

fn file_system_permissions_schema() -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "read".to_string(),
                JsonSchema::array(
                    JsonSchema::string(/*description*/ None),
                    Some(
                        "Absolute paths to grant read access; omit when none are needed."
                            .to_string(),
                    ),
                ),
            ),
            (
                "write".to_string(),
                JsonSchema::array(
                    JsonSchema::string(/*description*/ None),
                    Some(
                        "Absolute paths to grant write access; omit when none are needed."
                            .to_string(),
                    ),
                ),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = Some("Filesystem access request.".to_string());
    schema
}

fn windows_shell_guidance() -> &'static str {
    r#"Filesystem safety: keep destructive operations in one shell, resolve recursive delete or move targets inside the intended directory first, and avoid unresolved variables or globs.

Windows safety rules (apply when executing in a Windows environment, regardless of the host OS):
- Do not compose destructive filesystem commands across shells. Do not enumerate paths in PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / `Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.
- Before any recursive delete or move on Windows, verify the resolved absolute target paths stay within the intended workspace or explicitly named target directory. Never issue a recursive delete or move against a computed path if the final target has not been checked.
- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle Hidden` unless the user explicitly asked for a visible interactive window. Use visible windows only for interactive tools the user needs to see or control."#
}

/// Keep the platform-independent rule while omitting Windows-only instructions
/// when every selectable execution environment has a known POSIX cwd.
pub(crate) fn omit_windows_shell_guidance(spec: &mut ToolSpec) {
    if let ToolSpec::Function(tool) = spec
        && let Some(start) = tool.description.find("\n\nWindows safety rules")
    {
        tool.description.truncate(start);
    }
}

fn filesystem_safety_guidance() -> &'static str {
    windows_shell_guidance()
}

fn rg_search_admission_guidance() -> &'static str {
    r#"Search guidance:
- Read known files directly. Use `rg -l` when only matching filenames are needed; use scoped `rg -n` when matching content is needed. Start unknown-location searches in a likely owning path and expand after a miss. For repository-wide inventories, search the requested scope and preserve the complete matching set; bound displayed evidence without treating truncated results as complete. Exclude the repository's build and dependency output directories when they are outside the requested scope.
- Search windows locate code. Before editing, read the complete enclosing function, type, or configuration unit and refresh it after intervening writes.
- Output above the token budget is truncated. For a known source file, request enough `max_output_tokens` to read the needed range in one call."#
}

#[cfg(test)]
#[path = "shell_spec_tests.rs"]
mod tests;
