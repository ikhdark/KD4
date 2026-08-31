use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::adaptive_output_budget_description;
use serde_json::Number;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

const KD4_VALIDATION_COMMAND_GUIDANCE: &str = "For direct validation proof, use `kind: \"argv\"` and include non-empty repository-relative `validation.covered_paths`. A recognized validation command without this metadata may run, but is not recorded as proof.";

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
    schema.description = Some(KD4_VALIDATION_COMMAND_GUIDANCE.to_string());
    schema
}

const LEGACY_SHELL_SCRIPT_DESCRIPTION: &str = "Legacy shell script to execute. Use this only when shell semantics are required, including PowerShell cmdlets, variables or interpolation, pipelines or redirection, here-docs, compound statements, shell builtins, and `.cmd`/`.bat` semantics. When a standalone native executable and separated arguments are already known, use `kind: \"argv\"` with `program` and `args` instead; do not serialize them into this string field. This includes Git (`git`), ripgrep (`rg`), Cargo (`cargo`), Node (`node`), Python (`python`), and KD4 helper executables such as `kds`. Examples: `git` with `[\"status\", \"--short\"]`; `rg` with `[\"--files\"]`; `cargo` with `[\"test\", \"-p\", \"codex-core\"]`; `node` with `[\"script.js\"]`; `python` with `[\"-m\", \"pytest\"]`; `kds` with `[\"--help\"]`. Arbitrary command strings remain shell scripts and must not be heuristically split. For complex PowerShell, prefer `kind: \"powershell_script\"`. If shell inspection is necessary, keep read-only PowerShell to direct cmdlet pipelines without variables, loops, or script blocks so it can remain outside the repository mutation lane.";

fn bounded_integer(description: String, minimum: u64, maximum: u64) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum)),
        maximum: Some(Number::from(maximum)),
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
    // preflight-enforced schema must accept exactly that surface. Field
    // combination rules stay in the decoder, which reports violations with
    // prescriptive field-level messages; a stricter schema here rejects
    // shapes the runtime supports and buries the reason in an opaque
    // validation error.
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
    JsonSchema::object(properties, /*required*/ None, Some(false.into()))
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
        JsonSchema::boolean(Some(
            "Execute without reusing prior immutable evidence.".to_string(),
        )),
    );
    ToolSpec::Function(ResponsesApiTool {
        name: "exec_command".to_string(),
        description: format!(
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction.\n\n{}\n\n{}\n\n{}",
            KD4_VALIDATION_COMMAND_GUIDANCE,
            rg_search_admission_guidance(),
            filesystem_safety_guidance(),
        ),
        strict: false,
        defer_loading: None,
        parameters: command_parameters_schema(properties, "cmd"),
        output_schema: Some(unified_exec_output_schema()),
    })
}

pub fn create_write_stdin_tool() -> ToolSpec {
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
                adaptive_output_budget_description()
            ), 0, usize::MAX as u64),
        ),
    ]);

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
        JsonSchema::boolean(Some(
            "Execute without reusing prior immutable evidence.".to_string(),
        )),
    );

    let description = format!(
        r#"Runs a Powershell command (Windows) and returns its output.

Examples of valid command strings:

- ls -a (show hidden): "Get-ChildItem -Force"
- recursive find by name: "Get-ChildItem -Recurse -Filter *.py"
- recursive grep: "Get-ChildItem -Path C:\\myrepo -Recurse | Select-String -Pattern 'TODO' -CaseSensitive"
- ps aux | grep python: "Get-Process | Where-Object {{ $_.ProcessName -like '*python*' }}"
- setting an env var: "$env:FOO='bar'; echo $env:FOO"
- running an inline Python script: "@'\\nprint('Hello, world!')\\n'@ | python -"

{}

{}

{}"#,
        KD4_VALIDATION_COMMAND_GUIDANCE,
        rg_search_admission_guidance(),
        windows_shell_guidance(),
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
        output_schema: None,
    })
}

pub fn request_permissions_tool_description() -> String {
    "Request additional filesystem or network permissions from the user and wait for the client to grant a subset of the requested permission profile. Use environment_id to target a specific attached environment; omit it to use the primary environment. Relative filesystem paths resolve against the selected environment cwd. Granted permissions apply automatically to later shell-like commands in the current turn, or for the rest of the session if the client approves them at session scope."
        .to_string()
}

fn unified_exec_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "chunk_id": {
                "type": "string",
                "description": "Chunk identifier included when the response reports one."
            },
            "wall_time_seconds": {
                "type": "number",
                "description": "Elapsed wall time spent waiting for output in seconds."
            },
            "exit_code": {
                "type": "number",
                "description": "Process exit code when the command finished during this call."
            },
            "session_id": {
                "type": "number",
                "description": "Session identifier to pass to write_stdin when the process is still running."
            },
            "original_token_count": {
                "type": "number",
                "description": "Approximate token count before output truncation."
            },
            "raw_output_artifact": {
                "type": "string",
                "description": "Path to output retained before model summarization."
            },
            "raw_output_artifact_bytes": {
                "type": "number",
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
        "required": ["wall_time_seconds", "output"],
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
    r#"Windows safety rules:
- Do not compose destructive filesystem commands across shells. Do not enumerate paths in PowerShell and then pass them to `cmd /c`, batch builtins, or another shell for deletion or moving. Use one shell end-to-end, prefer native PowerShell cmdlets such as `Remove-Item` / `Move-Item` with `-LiteralPath`, and avoid string-built shell commands for file operations.
- Before any recursive delete or move on Windows, verify the resolved absolute target paths stay within the intended workspace or explicitly named target directory. Never issue a recursive delete or move against a computed path if the final target has not been checked.
- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle Hidden` unless the user explicitly asked for a visible interactive window. Use visible windows only for interactive tools the user needs to see or control."#
}

fn filesystem_safety_guidance() -> &'static str {
    "Filesystem safety: keep destructive operations in one shell, resolve recursive delete or move targets inside the intended directory first, and avoid unresolved variables or globs."
}

fn rg_search_admission_guidance() -> &'static str {
    r#"Search guidance:
- Start repository `rg` searches in a likely owning path. Expand after a miss or when the request genuinely requires a repository-wide inventory."#
}

#[cfg(test)]
#[path = "shell_spec_tests.rs"]
mod tests;
