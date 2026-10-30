use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::adaptive_output_budget_description;
use serde_json::Number;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

const LEGACY_SHELL_SCRIPT_DESCRIPTION: &str = "Shell script to execute in the user's default shell. Prefer this for every command, including pipelines, redirection, shell expansion, compound statements, and builtins.";

fn bounded_integer(description: String, minimum: u64, maximum: u64) -> JsonSchema {
    JsonSchema {
        minimum: Some(Number::from(minimum)),
        maximum: Some(Number::from(maximum.min((1_u64 << 53) - 1))),
        ..JsonSchema::integer(Some(description))
    }
}

fn command_parameters_schema(
    properties: BTreeMap<String, JsonSchema>,
    script_field: &str,
) -> JsonSchema {
    // The runtime decoder (`CommandInvocation::from_parts`) infers the branch
    // from whichever command field is populated, so `kind` only ever restates
    // what the arguments already say. Advertising it made every call carry a
    // discriminator and a choice the caller does not need to make; the decoder
    // still accepts it from existing callers. Require a command field, and
    // leave conflicting combinations to the decoder's field-level messages.
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

fn stall_timeout_schema() -> JsonSchema {
    bounded_integer(
        format!(
            "Maximum time without stdout or stderr before reporting the command as stalled and yielding its live session, including between polls. Silence never terminates the command. Defaults to {} ms; exec_command disables this default for recognized long-running validation, build, install, and discovery commands. Set zero to disable this observation.",
            crate::exec::DEFAULT_COMMAND_STALL_TIMEOUT_MS,
        ),
        0,
        u64::MAX,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandToolOptions {
    pub allow_login_shell: bool,
    pub exec_permission_approvals_enabled: bool,
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
                "Executable to launch directly, bypassing the shell. Use only when `cmd` cannot express the command.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some("Arguments for `program`, excluding the program name.".to_string()),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                "Plain PowerShell script that Codex encodes at runtime. Use only when `cmd` quoting cannot express the script."
                    .to_string(),
            )),
        ),
        (
            "env".to_string(),
            JsonSchema {
                description: Some("Environment overrides scoped to this command only; values are literal strings, never shell-expanded. Use with standalone program/args when shell assignments prevent command admission. Does not change the session or global environment.".to_string()),
                ..JsonSchema::object(BTreeMap::new(), None, Some(JsonSchema::string(None).into()))
            },
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
        ("stall_timeout_ms".to_string(), stall_timeout_schema()),
        (
            "yield_time_ms".to_string(),
            bounded_integer(
                format!("Wait before yielding output. Inside `exec`, noninteractive commands default to {} ms so the host can finish the awaited command without model polling; to overlap it with other work, start it alongside the other calls with Promise.all in the same cell, and shorten the wait only when the command must outlive the cell. Otherwise defaults to 30000 ms for recognized validation, build, install, and discovery commands (including command chains), and 2000 ms for other commands ({} ms for interactive commands inside `exec`). Explicit values are clamped to 250-{} ms. On Windows, waits are floored to {} ms only while the executor is not ready; commands that finish sooner return immediately. Nested calls may yield up to 2000 ms before their wrapper deadline to return a live session handle.", super::unified_exec::NESTED_NONINTERACTIVE_EXEC_YIELD_TIME_MS, super::unified_exec::NESTED_EXEC_YIELD_TIME_MS, crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS, crate::unified_exec::WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::MAX_INITIAL_YIELD_TIME_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(
                "Display cap for this result. Omit it: inside exec each result is already bounded by the cell limit with envelope reserve, independently of earlier prints, and direct calls are capped at 10000 tokens. Set it only to deliberately shrink a display. Zero returns only execution controls.".to_string(),
                0, usize::MAX as u64),
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
    ToolSpec::Function(ResponsesApiTool {
        name: "exec_command".to_string(),
        description: format!(
            "Runs a command through `cmd`, returning output or a session ID for ongoing interaction. Resume a returned session_id with write_stdin; do not restart the command while it is live or its effects are uncertain.\n\n{}\n\n{}\n\n{}",
            rg_search_admission_guidance(),
            filesystem_safety_guidance(),
            include_str!("../../../templates/command_admission_checkpoint.md").trim(),
        ),
        strict: false,
        defer_loading: None,
        parameters: command_parameters_schema(properties, "cmd"),
        output_schema: Some(unified_exec_output_schema().into()),
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
    tool.description = tool.description.replace("`cmd`", "`command`");
    let mut properties = tool
        .parameters
        .properties
        .take()
        .expect("command properties");
    let command = properties.remove("cmd").expect("script command");
    properties.insert("command".to_string(), command);
    for property in properties.values_mut() {
        if let Some(description) = &mut property.description {
            *description = description.replace("`cmd`", "`command`");
        }
    }
    tool.parameters = command_parameters_schema(properties, "command");
    ToolSpec::Function(tool)
}


pub(crate) fn create_write_stdin_tool_with_max_timeout(max_timeout_ms: u64) -> ToolSpec {
    let max_timeout_ms = max_timeout_ms.max(crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS);
    let default_timeout_ms =
        crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS.min(max_timeout_ms);
    let properties = BTreeMap::from([
        (
            "event_filter".to_string(),
            JsonSchema { description: Some("Conditional process wait for readiness/error literals or exit. Observation-only: omit other controls/output budgets. Replays retained producer bytes unless after is supplied, then waits through irrelevant output until a match or terminal event. Each call closes on return or cancellation; no persistent watcher. Exit/failure are always included, even without a literal match.".into()), ..JsonSchema::object(BTreeMap::from([
                ("contains".into(), JsonSchema::array(JsonSchema::string(Some("Nonempty literal, at most 256 UTF-8 bytes. At most 16 literals; duplicates match once per call across selected streams. Empty array waits only for exit/failure.".into())), None)),
                ("stream".into(), JsonSchema::string_enum(vec!["any".into(), "stdout".into(), "stderr".into()], Some("Default any. PTY output is stdout.".into()))),
                ("timeout_ms".into(), bounded_integer("Required subscription lifetime; nested Code Mode deadlines may shorten it. The first match, exit/failure, output gap, timeout or cancellation closes this call's subscription. Cancellation never terminates the process.".into(), 1, max_timeout_ms)),
                ("after".into(), JsonSchema { description: Some("Reuse output_cursor from this same session/incarnation to skip previously examined bytes. Omit to include output emitted before subscription. Matches must end after the cursor; split literals still match. Unselected streams and exit-only waits keep the previous position. Cursor is runtime-only; it does not consume ordinary output.".into()), ..JsonSchema::object(BTreeMap::from([
                    ("stdout".into(), bounded_integer("Previously examined stdout byte position.".into(), 0, usize::MAX as u64)),
                    ("stderr".into(), bounded_integer("Previously examined stderr byte position.".into(), 0, usize::MAX as u64)),
                ]), Some(vec!["stdout".into(), "stderr".into()]), Some(false.into())) }),
            ]), Some(vec!["contains".into(), "timeout_ms".into()]), Some(false.into())) },
        ),
        (
            "terminate".to_string(),
            JsonSchema::boolean(Some("True confirms process termination before collecting final output. Requires empty chars. Use only with session_capabilities.cancellation=true.".to_string())),
        ),
        (
            "incarnation".to_string(),
            JsonSchema::string(Some("Exact creation identity from the originating session_capabilities.incarnation. Required with session_id for polling, input, and termination; legacy numeric-only handles are rejected.".into())),
        ),
        (
            "wait_for_output".to_string(),
            JsonSchema::boolean(Some("With empty chars, wait until new output or process exit rather than returning empty periodic polls. Defaults to true when chars is empty, yield_time_ms is omitted, and terminate is false; false preserves bounded polling. Cancellation and new user input remain responsive. Code mode applies no default nested deadline for this passive wait; an explicit timeout_ms still bounds it.".to_string())),
        ),
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
                    "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms. Bounded empty polls default to {default_timeout_ms} ms and cap at {max_timeout_ms} ms; waits below 5000 ms are honored down to 250 ms only when new output is pending, otherwise a 5000 ms floor applies. Omitting this on an empty poll defaults to waiting for output or exit. A wait deadline does not terminate the process."
                ),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                // The request is an upper bound, not permission to exceed the
                // owner's configured cap. Accept the standard helper budget;
                // the process manager still clamps it to max_timeout_ms.
                max_timeout_ms.max(crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS),
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
            "Writes characters to an existing unified exec session and returns recent output. Use only a session_id returned by exec_command or its shell_command compatibility route. Inspect session_capabilities first: send characters only with stdin=true, Ctrl-C by sending `chars: \"\\u0003\"` only when session_capabilities.interrupt=true, and empty input to poll with polling=true. `interrupt` is a returned capability, not an input parameter. Use terminate=true with empty chars to stop a process when session_capabilities.cancellation=true, including non-PTY Windows sessions. Stop when no session_id is returned. Poll again only for an identified pending transition; do not restart the command while it is live or its effects are uncertain."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["session_id".to_string(), "incarnation".to_string()]),
            Some(false.into()),
        ),
        output_schema: Some(json!({"anyOf":[unified_exec_output_schema(), process_event_output_schema()]}).into()),
    })
}

fn process_event_output_schema() -> Value {
    json!({
        "type":"object",
        "description":"Read-only conditional observation, not command completion collection. Retained stdout/stderr are replayed unless event_filter.after is supplied, then only new bytes are examined. Reuse output_cursor as after to avoid old matches across waits; a split literal ending after the cursor still matches. No normal output is consumed. Exit/failure always end the wait, even without a match; a literal wait first drains output still open at exit, up to the timeout. history_complete=false means bytes a literal wait would examine were lost, or output was still open. At most 20 events and 16 literals (256 UTF-8 bytes each); failure messages cap at 1024 Unicode characters. Collect final output with ordinary write_stdin.",
        "properties":{
            "session_id":{"type":"integer"}, "incarnation":{"type":"string"},
            "subscription_closed":{"const":true},
            "subscription_end":{"enum":["events","timeout"]},
            "history_complete":{"type":"boolean"},
            "output_cursor":{"type":"object","properties":{
                "stdout":{"type":"integer","minimum":0},
                "stderr":{"type":"integer","minimum":0}
            },"required":["stdout","stderr"],"additionalProperties":false},
            "events":{"type":"array","maxItems":20,"items":{
                "type":"object","properties":{
                    "kind":{"enum":["output_match","output_gap","exited","failed"]},
                    "stream":{"enum":["stdout","stderr","unknown"]},
                    "contains":{"type":"string"}, "exit_code":{"type":["integer","null"]},
                    "message":{"type":"string"}, "message_truncated":{"type":"boolean"}
                },"required":["kind"],"additionalProperties":false
            }}
        },
        "required":["session_id","incarnation","subscription_closed","subscription_end","history_complete","output_cursor","events"],
        "additionalProperties":false
    })
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
                "Executable to launch directly, bypassing the shell. Use only when `command` cannot express the command.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema::array(
                JsonSchema::string(/*description*/ None),
                Some("Arguments for `program`, excluding the program name.".to_string()),
            ),
        ),
        (
            "script_body".to_string(),
            JsonSchema::string(Some(
                "Plain PowerShell script that Codex encodes at runtime. Use only when `command` quoting cannot express the script."
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
                format!(
                    "Maximum time without stdout or stderr before cancelling the command. Defaults to {} ms for ordinary commands; recognized validation commands disable this default. Set zero to disable this silence timeout; timeout_ms still applies. This native route does not return a resumable session; use exec_command for a non-terminating stall observation.",
                    crate::exec::DEFAULT_COMMAND_STALL_TIMEOUT_MS,
                ),
                0,
                u64::MAX,
            ),
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

    let description = format!(
        "Runs a command in the user's default shell and returns its output. The native route returns text with command status and output, or structured validation evidence, without a resumable session_id. Use max_output_tokens to request a smaller output budget; larger requests are capped by policy. Use syntax supported by that shell.\n\n{}\n\n{}",
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
            codex_protocol::request_permissions::RequestPermissionsResponse::output_schema().into(),
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
            "kind": {"enum": ["workspace_gate_timeout"]},
            "did_run": {
                "type": "boolean",
                "description": "False on a workspace admission refusal: the command was never started."
            },
            "error": {
                "type": ["string", "null"],
                "description": "Command failure, including watchdog or cleanup failure. Inspect execution_state and session_id; do not replay uncertain effects."
            },
            "stdout": {
                "type": "string",
                "description": "Exact whole-command UTF-8 stdout, available to scripts only when streams_complete is true. Independent of the display token budget; stdout and stderr together are limited to 1 MiB."
            },
            "stderr": {
                "type": "string",
                "description": "Exact whole-command UTF-8 stderr when streams_complete is true."
            },
            "streams_complete": {
                "type": "boolean",
                "description": "True only when both entire streams are available separately without loss, including pipe-backed exec-server commands. False means merged (for example PTY), partial, oversized, or non-UTF-8 streams; never parse output as a substitute for exact stdout."
            },
            "session_capabilities": {
                "type": "object",
                "properties": {
                    "incarnation": {"type":"string"},
                    "stdin": {"type": "boolean"},
                    "interrupt": {"type": "boolean"},
                    "cancellation": {"type": "boolean"},
                    "polling": {"type": "boolean"},
                    "observation": {
                        "type": "object",
                        "properties": {
                            "silent_for_ms": {"type": "integer", "minimum": 0},
                            "reason": {"enum": ["no_output_observed"]},
                            "process_exited": {"type": "boolean"},
                            "termination_requested": {"type": "boolean"}
                        },
                        "required": ["silent_for_ms", "reason", "process_exited", "termination_requested"],
                        "additionalProperties": false
                    }
                },
                "required": ["stdin", "interrupt", "cancellation", "polling", "incarnation"],
                "additionalProperties": false
            },
            "chunk_id": {
                "type": "string",
                "description": "Chunk identifier included when the response reports one."
            },
            "wall_time_seconds": {
                "type": "number",
                "description": "Diagnostic elapsed time; retained in session logs and omitted from script-visible results."
            },
            "exit_code": {
                "type": ["integer", "null"],
                "description": "Terminal exit code; null when still running or when the exit code is unavailable. Consult execution_state."
            },
            "execution_state": {
                "type": "string",
                "enum": ["running", "exited", "unknown", "not_started"],
                "description": "Command process state, independent of the exec cell. Unknown means no live handle or terminal outcome is available; not_started confirms workspace admission refused before launch."
            },
            "process_exited": { "type": "boolean" },
            "search_no_match": { "type": "boolean" },
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
            "recovery_selector": {
                "type": "object",
                "description": "First omitted source line range, when its coordinates are known in raw_output_artifact_id.",
                "properties": {
                    "kind": {"type": "string", "enum": ["lines", "bytes"]},
                    "start": {"type": "integer", "minimum": 0},
                    "end": {"type": "integer", "minimum": 0}
                },
                "required": ["kind", "start", "end"],
                "additionalProperties": false
            },
            "raw_output_artifact_error": {
                "type": "string",
                "description": "Artifact persistence failure, when retention was unavailable."
            },
            "recovery": {
                "type": "object",
                "description": "read_tool_output call for the retained output; a running command's artifact is readable after it exits.",
                "properties": {"tool":{"const":"read_tool_output"}, "arguments":{"type":"object"}},
                "required": ["tool", "arguments"]
            },
            "completion_recovered": {"type":"boolean"},
            "success": {
                "type": "boolean",
                "description": "Present on a recovered terminal receipt; true only when retained terminal facts establish success. Missing terminal facts are not a successful execution receipt."
            },
            "terminal_facts_available": {
                "type": "boolean",
                "description": "Whether a recovered terminal receipt retains outcome details beyond its exit code."
            },
            "notice": {"type":"string"},
            "repair": {
                "type": "string",
                "description": "One pre-execution read-only equivalent repair applied to the command."
            },
            "output": {
                "type": "string",
                "description": "Command output text, possibly truncated."
            },
            "current_chunk_display_complete": {
                "type": "boolean",
                "description": "True when this poll's displayed output chunk was not reduced; the inverse of output_reduced."
            },
            "cumulative_streams_complete": {
                "type": "boolean",
                "description": "Same as streams_complete: the whole stdout and stderr are available exactly."
            },
            "retained_artifact_complete": {
                "type": "boolean",
                "description": "Present with raw_output_artifact_id; true only when the process exited and the artifact retains its complete output."
            }
        },
        "required": ["output", "execution_state", "process_exited", "exit_code", "output_complete", "output_reduced", "raw_output_artifact_retention_limit_hit"],
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "kind": {"const": "workspace_gate_timeout"},
                    "did_run": {"const": false},
                    "execution_state": {"const": "not_started"},
                    "process_exited": {"const": false},
                    "exit_code": {"type": "null"},
                    "output_complete": {"const": false}
                },
                "required": ["kind", "did_run"],
                "not": {"required": ["session_id"]}
            },
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

/// Eager code-mode projection of the command result fields cells commonly read.
/// The named shape replaces an `unknown` result in every exec prompt; the
/// registered specs keep the complete lifecycle contract above for resolve_tool.
pub(crate) fn unified_exec_eager_output_schema() -> Value {
    json!({
        "$ref": "#/$defs/ExecCommandResult",
        "$defs": {
            "ExecCommandResult": {
                "type": "object",
                "properties": {
                    "output": {
                        "type": "string",
                        "description": "Combined display text; may be truncated."
                    },
                    "exit_code": {
                        "type": ["integer", "null"],
                        "description": "Exit status; null while running or when unavailable."
                    },
                    "stdout": {
                        "type": "string",
                        "description": "Exact stdout, present when streams_complete is true."
                    },
                    "stderr": {
                        "type": "string",
                        "description": "Exact stderr, present when streams_complete is true."
                    },
                    "streams_complete": {"type": "boolean"},
                    "session_id": {
                        "type": "integer",
                        "description": "Present while the process runs, or after exit while output remains unread: pass the whole result to await_command, or use it with write_stdin."
                    },
                    "recovery": {
                        "type": "object",
                        "description": "Ready read_tool_output call for omitted output, present when output was truncated.",
                        "properties": {"tool": {"const": "read_tool_output"}, "arguments": {"type": "object"}},
                        "required": ["tool", "arguments"]
                    }
                },
                "required": ["output", "exit_code"]
            }
        }
    })
}

fn create_approval_parameters(
    exec_permission_approvals_enabled: bool,
    allow_escalated_sandbox_permissions: bool,
) -> BTreeMap<String, JsonSchema> {
    if !exec_permission_approvals_enabled && !allow_escalated_sandbox_permissions {
        return BTreeMap::new();
    }
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
                    r#"Reusable approval prefix for the command, only with `sandbox_permissions: "require_escalated"`; for example ["git", "pull"]."#.to_string(),
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
- PowerShell and cmd do not expand wildcard paths for native `rg`. Search a literal parent directory with `--glob` instead, for example `rg -n "default_prompt:" skills --glob "**/agents/openai.yaml"`.
- PowerShell `foreach` statements cannot feed a pipeline directly. Collect first: `$rows = foreach ($item in $items) { $item }; $rows | Out-String`.
- Required PowerShell cmdlets must use `-ErrorAction Stop` (or a script-scoped stop policy); capture and check `$LASTEXITCODE` immediately after each required native command, before dependent work or success output. A final zero exit does not prove earlier steps succeeded. Guard genuinely optional paths explicitly rather than ignoring required failures.
- For multiline Python or other interpreter code, prefer a literal PowerShell here-string piped to the selected interpreter's stdin over nested `-c` quoting. Keep diagnostic projections in named steps rather than deeply nested expressions.
- For deletes and moves prefer `Remove-Item` / `Move-Item` with `-LiteralPath`; do not hand PowerShell-enumerated paths to `cmd /c` or another shell.
- When using `Start-Process` to launch a background helper or service, pass `-WindowStyle Hidden` unless the user explicitly asked for a visible interactive window."#
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
- Discover filenames before reading/searching them: first use `rg --files <literal-root> --glob '<pattern>'`, for example `rg --files codex-rs --glob '*remote*'`, then use the returned paths. Never pass guessed filenames or wildcard paths directly to `rg`.
- Use `rg -l` when only matching filenames are needed; use scoped `rg -n` when matching content is needed. `rg` skips hidden directories and ignored files by default, with or without matches; add `--hidden` when the scope can include dot-directories such as `.config` or `.github`.
- Keep separate searches and reads in separate tool calls, optionally batched; do not semicolon-chain `rg` searches and file reads into one shell call. Inspect each result: `rg` exit 0 means matches, 1 means no matches, and 2 means the search failed. Correct only the failed search; retain successful sibling results and do not rerun the batch.
- Prefer native `read_file` with batched selectors over `Get-Content | Select-Object` paging when available. Keep shell reads for required encoding, streaming, or transformations.
- Omit `max_output_tokens` unless deliberately requesting a smaller display; the host enforces output caps. When output is truncated, recover only missing evidence from the retained artifact."#
}

