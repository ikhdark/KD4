use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn exec_command_guidance_description() -> String {
    format!(
        "\n\n{}\n\n{}",
        rg_search_admission_guidance(),
        filesystem_safety_guidance()
    )
}

fn shell_command_guidance_description() -> String {
    format!(
        "\n\n{}\n\n{}",
        rg_search_admission_guidance(),
        filesystem_safety_guidance()
    )
}

#[test]
fn token_efficiency_command_tools_recommend_narrow_rg_without_rejection() {
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
        let description =
            serde_json::to_value(tool).expect("serialize command tool")["description"]
                .as_str()
                .expect("command tool description")
                .to_string();
        assert!(description.contains("Read known files directly."));
        assert!(description.contains("Use `rg -l` when only matching filenames are needed"));
        assert!(description.contains("use scoped `rg -n` when matching content is needed"));
        assert!(
            description
                .contains("search the requested scope and preserve the complete matching set")
        );
        assert!(description.contains("without treating truncated results as complete"));
        assert!(!description.contains("then `rg -n"));
        assert!(!description.contains("is rejected"));
        assert!(description.contains(
            "Before editing, read the complete enclosing function, type, or configuration unit"
        ));
        assert!(
            description.contains(
                "apply when executing in a Windows environment, regardless of the host OS"
            )
        );
        assert!(description.contains(
            "resolve recursive delete or move targets inside the intended directory first"
        ));
    }
}

fn has_parameter(tool: &ToolSpec, parameter_name: &str) -> bool {
    let tool = serde_json::to_value(tool).expect("tool spec should serialize");
    tool.pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
}

#[test]
fn command_declarations_preserve_input_alternatives_and_return_contracts() {
    for spec in [
        create_exec_command_tool(CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        }),
        create_shell_command_tool(CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        }),
    ] {
        let definition = codex_tools::tool_spec_to_code_mode_tool_definition(&spec).unwrap();
        let (_, declaration) = definition
            .description
            .split_once("exec tool declaration:")
            .unwrap();
        eprintln!(
            "{}: description_bytes={}, declaration_bytes={}, untyped_object_unions={}",
            definition.name,
            definition.description.len(),
            declaration.len(),
            declaration
                .matches("string | number | boolean | null | unknown[]")
                .count()
        );
        let script_field = if definition.name == "exec_command" {
            "cmd"
        } else {
            "command"
        };
        for field in [script_field, "program", "script_body"] {
            assert!(declaration.contains(&format!("{field}: unknown;")));
        }
        assert!(declaration.contains("Promise<"));
        if definition.name == "exec_command" {
            assert!(declaration.contains("session_id?: number"));
            assert!(declaration.contains("execution_state?:"));
            assert!(declaration.contains("session_capabilities?:"));
        }
        assert!(
            !declaration.contains("string | number | boolean | null | unknown[]"),
            "an enclosing object constraint makes non-object command alternatives redundant"
        );
    }
}

#[test]
fn command_validation_context_is_lean_and_strict() {
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
        let validation = &tool["parameters"]["properties"]["validation"];
        assert_eq!(
            validation["properties"]
                .as_object()
                .expect("validation properties")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["covered_paths"]
        );
        assert_eq!(validation["required"], serde_json::json!(["covered_paths"]));
        assert_eq!(validation["additionalProperties"], serde_json::json!(false));
    }
}

#[test]
fn command_tools_allow_batched_inspection_and_optional_validation_metadata() {
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
        let description = tool["description"]
            .as_str()
            .expect("command tool description");
        assert!(!description.contains("not recorded as proof"));
        assert!(!description.contains("Omit redundant defaults"));
        let script_field = if tool["name"] == "exec_command" {
            "cmd"
        } else {
            "command"
        };
        let script_description = tool["parameters"]["properties"][script_field]["description"]
            .as_str()
            .expect("script description");
        assert!(!script_description.contains("Issue independent read-only commands"));
        assert!(script_description.contains("you may use `kind: \"argv\"`"));
        assert_eq!(
            tool["parameters"]["properties"]["validation"]["description"],
            "Optional validation scope metadata."
        );
        let validator =
            jsonschema::validator_for(&tool["parameters"]).expect("command schema should compile");
        let mut arguments = json!({});
        arguments[script_field] = json!("python -m unittest -q");
        assert!(validator.is_valid(&arguments));
        arguments["validation"] = json!({"covered_paths": ["src"]});
        assert!(validator.is_valid(&arguments));
    }
}

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = format!(
        "Runs a command, returning output or a session ID for ongoing interaction. Resume a returned session_id with write_stdin; do not restart the command while it is live or its effects are uncertain. For commands needing no shell interpretation, you may use program and args (kind: argv). Use kind: powershell_script with script_body for PowerShell semantics. Keep pipelines, redirections, and shell expansion in script form.{}",
        exec_command_guidance_description()
    );

    let mut properties = BTreeMap::from([
        (
            "cmd".to_string(),
            JsonSchema::string(Some(
                "Shell script to execute in the user's default shell. For a standalone native executable with known arguments, you may use `kind: \"argv\"` with `program` and `args`. Keep pipelines, redirection, shell expansion, compound statements, and builtins in script form. On Windows, also keep `.cmd`/`.bat` calls in script form; for complex PowerShell, prefer `kind: \"powershell_script\"`."
                    .to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![json!("script"), json!("argv"), json!("powershell_script")],
                Some(
                    "Canonical command encoding. `script` explicitly uses `cmd`; `argv` launches `program` directly with `args`; `powershell_script` runtime-encodes `script_body`. Legacy input remains supported by omitting `kind`; the runtime infers the branch from the single populated command field and normalizes it immediately."
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
                "Wait before yielding output. Defaults to 30000 ms for recognized validation commands and 2000 ms otherwise; explicit values use 250-30000 ms. Windows initial waits are floored to 2000 ms.".to_string(),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::MAX_YIELD_TIME_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(format!(
                "Output token budget. {}; larger requests may be capped by policy. Zero requests a zero-token text budget; command lifecycle and recovery metadata are still returned.",
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
        /*allow_escalated_sandbox_permissions*/ true,
    ));
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(
            "Force execution instead of reusing equivalent evidence. By default, unchanged file reads, searches, and deterministic failures may reuse a prior result; reused results are labeled. Set true when external state changed or a fresh observation is required.".to_string(),
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
fn command_tools_only_advertise_escalation_when_the_policy_can_request_it() {
    let options = CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    };
    let unavailable = [
        create_exec_command_tool_for_policy(
            options, /*include_environment_id*/ false, /*include_shell_parameter*/ true,
            /*allow_escalated_sandbox_permissions*/ false,
        ),
        create_shell_command_tool_for_policy(
            options, /*allow_escalated_sandbox_permissions*/ false,
        ),
    ];
    for tool in unavailable {
        let tool = serde_json::to_value(tool).expect("serialize command tool");
        let properties = &tool["parameters"]["properties"];
        assert_eq!(
            properties["sandbox_permissions"]["enum"],
            json!(["use_default"])
        );
        assert!(properties.get("justification").is_none());
        assert!(properties.get("prefix_rule").is_none());
    }

    let available = serde_json::to_value(create_exec_command_tool_for_policy(
        options, /*include_environment_id*/ false, /*include_shell_parameter*/ true,
        /*allow_escalated_sandbox_permissions*/ true,
    ))
    .expect("serialize command tool");
    assert_eq!(
        available["parameters"]["properties"]["sandbox_permissions"]["enum"],
        json!(["use_default", "require_escalated"])
    );
    assert!(
        available["parameters"]["properties"]
            .get("justification")
            .is_some()
    );
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
                "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms. Empty polls default to 60000 ms; explicit shorter waits are honored down to 250 ms. A wait deadline does not terminate the process.".to_string(),
                crate::unified_exec::MIN_YIELD_TIME_MS,
                crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS,
            ),
        ),
        (
            "max_output_tokens".to_string(),
            bounded_integer(format!(
                "Output token budget. {}; larger requests may be capped by policy. Zero requests a zero-token text budget; command lifecycle and recovery metadata are still returned.",
                codex_utils_output_truncation::adaptive_output_budget_description()
            ), 0, usize::MAX as u64),
        ),
    ]);

    assert_eq!(
        tool,
        ToolSpec::Function(ResponsesApiTool {
            name: "write_stdin".to_string(),
            description:
                "Writes characters to an existing unified exec session and returns recent output. Use only a session_id returned by exec_command or its shell_command compatibility route. Inspect session_capabilities first: send characters only with stdin=true, Ctrl-C only with interrupt=true, and empty input to poll with polling=true. cancellation describes explicit process cancellation through this tool; false means no such operation is exposed. Stop when no session_id is returned. Poll again only for an identified pending transition; do not restart the command while it is live or its effects are uncertain."
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
            // The caller must read which permissions were actually granted, so
            // the response shape is published with the tool.
            output_schema: Some(
                codex_protocol::request_permissions::RequestPermissionsResponse::output_schema()
            ),
        })
    );
}

#[test]
fn shell_command_tool_matches_expected_spec() {
    let tool = create_shell_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = "Runs a command in the user's default shell and returns its output. The native route returns text with command status and output, or structured validation evidence, without a resumable session_id. Its output budget is policy-controlled; max_output_tokens is not accepted. Use syntax supported by that shell. For commands needing no shell interpretation, you may use program and args (kind: argv). Use kind: powershell_script with script_body for PowerShell semantics.".to_string()
        + &shell_command_guidance_description();

    let mut properties = BTreeMap::from([
        (
            "command".to_string(),
            JsonSchema::string(Some(
                "Shell script to execute in the user's default shell. For a standalone native executable with known arguments, you may use `kind: \"argv\"` with `program` and `args`. Keep pipelines, redirection, shell expansion, compound statements, and builtins in script form. On Windows, also keep `.cmd`/`.bat` calls in script form; for complex PowerShell, prefer `kind: \"powershell_script\"`."
                    .to_string(),
            )),
        ),
        (
            "kind".to_string(),
            JsonSchema::string_enum(
                vec![json!("script"), json!("argv"), json!("powershell_script")],
                Some(
                    "Canonical command encoding. `script` explicitly uses `command`; `argv` launches `program` directly with `args`; `powershell_script` runtime-encodes `script_body`. Legacy input remains supported by omitting `kind`; the runtime infers the branch from the single populated command field and normalizes it immediately."
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
                "Maximum command runtime. Defaults to 10000 ms. Zero sets an immediate deadline; it does not disable the timeout.".to_string(),
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
        /*allow_escalated_sandbox_permissions*/ true,
    ));
    properties.insert(
        "force_fresh".to_string(),
        JsonSchema::boolean(Some(
            "Force execution instead of reusing equivalent evidence. By default, unchanged file reads, searches, and deterministic failures may reuse a prior result; reused results are labeled. Set true when external state changed or a fresh observation is required.".to_string(),
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
fn command_tools_accept_legacy_untagged_and_canonical_kind_forms() {
    for (tool, script_field) in [
        (
            create_exec_command_tool(CommandToolOptions {
                allow_login_shell: true,
                exec_permission_approvals_enabled: false,
            }),
            "cmd",
        ),
        (
            create_shell_command_tool(CommandToolOptions {
                allow_login_shell: true,
                exec_permission_approvals_enabled: false,
            }),
            "command",
        ),
    ] {
        let tool = serde_json::to_value(tool).expect("serialize command tool");
        let parameters = &tool["parameters"];
        // The advertised schema must match the runtime decoder's surface: one
        // flat object whose `kind` is optional and canonical. Field
        // combination rules belong to `CommandInvocation::from_parts`, which
        // reports violations with prescriptive field-level messages.
        assert!(parameters.get("oneOf").is_none());
        assert!(parameters.get("$defs").is_none());
        assert_eq!(parameters["required"], serde_json::Value::Null);
        assert_eq!(
            parameters["properties"]["kind"]["enum"],
            json!(["script", "argv", "powershell_script"])
        );

        let validator =
            jsonschema::validator_for(parameters).expect("command schema should compile");
        let mut legacy_untagged = json!({});
        legacy_untagged[script_field] = json!("git status --short");
        let mut explicit_script = json!({
            "kind": "script",
            "workdir": "repo",
            "validation": {"covered_paths": ["src"]}
        });
        explicit_script[script_field] = json!("git status --short");
        let accepted = [
            legacy_untagged,
            explicit_script,
            json!({
                "kind": "argv",
                "program": "git",
                "args": ["status", "--short"],
                "force_fresh": true
            }),
            json!({"program": "git", "args": ["status"]}),
            json!({"kind": "powershell_script", "script_body": "Get-ChildItem"}),
        ];
        for arguments in accepted {
            assert!(
                validator.is_valid(&arguments),
                "command schema should accept decoder-supported shape {arguments}"
            );
        }

        let rejected = [
            json!({}),
            json!({"kind": "script"}),
            json!({"args": ["status"]}),
            json!({"kind": "argv", "program": "git", "unknown": true}),
            json!({"kind": "argv", "program": "git", "workdir": 42}),
            json!({
                "kind": "argv",
                "program": "git",
                "validation": {"covered_paths": "src"}
            }),
        ];
        for arguments in rejected {
            assert!(
                !validator.is_valid(&arguments),
                "command schema should reject malformed arguments {arguments}"
            );
        }
    }
}

#[test]
fn command_schema_reports_specific_bounds_violations() {
    let tool = serde_json::to_value(create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    }))
    .expect("serialize exec tool");
    let validator =
        jsonschema::validator_for(&tool["parameters"]).expect("command schema should compile");

    let arguments = json!({"cmd": "Get-Content README.md", "yield_time_ms": 50});
    let errors = validator
        .iter_errors(&arguments)
        .map(|error| format!("{}: {error}", error.instance_path().as_str()))
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("/yield_time_ms"), "{errors:?}");
    assert!(errors[0].contains("250"), "{errors:?}");
}

#[test]
fn integer_arguments_expose_destination_types_and_runtime_bounds() {
    let exec = serde_json::to_value(create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    }))
    .expect("serialize exec tool");
    let exec_yield = &exec["parameters"]["properties"]["yield_time_ms"];
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
        write["parameters"]["properties"]["yield_time_ms"]["maximum"],
        crate::unified_exec::DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS
    );
}

#[test]
fn command_schema_rejects_numbers_javascript_cannot_represent_exactly() {
    let tool = serde_json::to_value(create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    }))
    .expect("serialize command tool");
    let validator = jsonschema::validator_for(&tool["parameters"]).expect("command schema");
    assert!(
        validator
            .is_valid(&json!({"cmd": "echo ok", "max_output_tokens": 9_007_199_254_740_991_u64}))
    );
    assert!(
        !validator
            .is_valid(&json!({"cmd": "echo ok", "max_output_tokens": 9_007_199_254_740_992_u64}))
    );
}

#[test]
fn command_output_schemas_require_integral_counters_but_allow_fractional_time() {
    for tool in [
        create_exec_command_tool(CommandToolOptions {
            allow_login_shell: true,
            exec_permission_approvals_enabled: false,
        }),
        create_write_stdin_tool(),
    ] {
        let ToolSpec::Function(tool) = tool else {
            panic!("expected command function tool");
        };
        let schema = tool.output_schema.expect("command output schema");
        let validator = jsonschema::validator_for(&schema).expect("valid output schema");
        let output = json!({
            "wall_time_seconds": 0.125,
            "output": "done",
            "exit_code": -1,
            "execution_state": "exited",
            "process_exited": true,
            "output_complete": false,
            "output_reduced": false,
            "raw_output_artifact_retention_limit_hit": false,
            "session_id": 42,
            "original_token_count": 100,
            "raw_output_artifact_bytes": 400
        });
        assert!(validator.is_valid(&output));
        for field in [
            "exit_code",
            "session_id",
            "original_token_count",
            "raw_output_artifact_bytes",
        ] {
            let mut invalid_output = output.clone();
            invalid_output[field] = json!(1.5);
            assert!(
                !validator.is_valid(&invalid_output),
                "{} must reject fractional {field}",
                tool.name
            );
        }
    }
}

#[test]
fn command_output_schema_rejects_ambiguous_lifecycle_and_accepts_runtime_results() {
    use crate::tools::context::ExecCommandToolOutput;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use codex_utils_output_truncation::TruncationPolicy;

    let schema = unified_exec_output_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();
    assert!(!validator.is_valid(&json!({"wall_time_seconds": 1.0, "output": "partial"})));
    for (process_id, exit_code, process_exited, expected) in [
        (Some(42), None, false, "running"),
        (None, Some(0), true, "exited"),
        (None, Some(1), true, "exited"),
        (Some(42), Some(0), true, "exited"), // exited with output left to drain
        (None, None, true, "exited"),        // terminal, exit code unavailable
        (None, None, false, "unknown"),
    ] {
        let output = ExecCommandToolOutput {
            validation: None,
            event_call_id: "lifecycle".into(),
            chunk_id: "chunk".into(),
            wall_time: std::time::Duration::from_millis(125),
            raw_output: b"evidence".to_vec(),
            truncation_policy: TruncationPolicy::Tokens(10_000),
            max_output_tokens: Some(0),
            process_id,
            session_capabilities: None,
            exit_code,
            process_exited,
            search_no_match: false,
            original_token_count: Some(2),
            hook_command: None,
            raw_output_artifact: None,
            raw_output_reduction_notice: None,
            repair_notice: None,
            pending_deferred_completions: Vec::new(),
        };
        let result = output.code_mode_result(&ToolPayload::Function {
            arguments: "{}".into(),
        });
        assert_eq!(result["execution_state"], expected);
        assert_eq!(result["exit_code"], json!(exit_code));
        assert!(validator.is_valid(&result), "{result}");
        assert_eq!(
            output.projection_metadata().unwrap().essential_inline["execution_state"],
            expected
        );
        for field in ["execution_state", "process_exited", "exit_code"] {
            let mut invalid = result.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(!validator.is_valid(&invalid), "missing {field}: {invalid}");
        }
        if expected == "running" {
            let mut invalid = result.clone();
            invalid.as_object_mut().unwrap().remove("session_id");
            assert!(!validator.is_valid(&invalid));
            let mut invalid = result;
            invalid["exit_code"] = json!(0);
            assert!(!validator.is_valid(&invalid));
        }
    }
}
