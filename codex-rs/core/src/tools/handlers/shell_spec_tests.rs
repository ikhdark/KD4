use super::*;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn exec_command_guidance_description() -> String {
    format!(
        "\n\n{}\n\n{}\n\n{}",
        KD4_VALIDATION_COMMAND_GUIDANCE,
        rg_search_admission_guidance(),
        filesystem_safety_guidance()
    )
}

fn shell_command_guidance_description() -> String {
    format!(
        "\n\n{}\n\n{}\n\n{}",
        KD4_VALIDATION_COMMAND_GUIDANCE,
        rg_search_admission_guidance(),
        windows_shell_guidance()
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
        assert!(description.contains("Start repository `rg` searches in a likely owning path"));
        assert!(description.contains("genuinely requires a repository-wide inventory"));
        assert!(!description.contains("is rejected"));
    }
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

fn resolved_command_parameter_schema<'a>(
    tool: &'a serde_json::Value,
    branch: &'a serde_json::Value,
    parameter_name: &str,
) -> &'a serde_json::Value {
    let schema = &branch["properties"][parameter_name];
    let Some(schema_ref) = schema["$ref"].as_str() else {
        return schema;
    };
    let pointer = schema_ref
        .strip_prefix('#')
        .expect("local command parameter schema reference");
    tool["parameters"]
        .pointer(pointer)
        .expect("referenced command parameter schema")
}

fn inline_common_command_parameter_schemas(parameters: &serde_json::Value) -> serde_json::Value {
    let mut inline = parameters.clone();
    for branch in inline["oneOf"]
        .as_array_mut()
        .expect("command form branches")
    {
        for schema in branch["properties"]
            .as_object_mut()
            .expect("command form properties")
            .values_mut()
        {
            let Some(schema_ref) = schema["$ref"].as_str() else {
                continue;
            };
            let pointer = schema_ref
                .strip_prefix('#')
                .expect("local command parameter schema reference");
            *schema = parameters
                .pointer(pointer)
                .expect("referenced command parameter schema")
                .clone();
        }
    }
    inline
        .as_object_mut()
        .expect("command parameters object")
        .remove("$defs");
    inline
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
        let branches = tool["parameters"]["oneOf"]
            .as_array()
            .expect("command variants");
        for branch in branches {
            let validation = resolved_command_parameter_schema(&tool, branch, "validation");
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
}

#[test]
fn token_efficiency_command_tools_explain_validation_proof_metadata() {
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
        assert!(description.contains("For direct validation proof"));
        assert!(description.contains("`kind: \"argv\"`"));
        assert!(description.contains("`validation.covered_paths`"));
        assert!(description.contains("may run, but is not recorded as proof"));

        for branch in tool["parameters"]["oneOf"]
            .as_array()
            .expect("command variants")
        {
            assert_eq!(
                resolved_command_parameter_schema(&tool, branch, "validation")["description"],
                KD4_VALIDATION_COMMAND_GUIDANCE
            );
        }
    }
}

#[test]
fn exec_command_tool_matches_expected_spec() {
    let tool = create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    });

    let description = format!(
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction.{}",
        exec_command_guidance_description()
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
        /*allow_escalated_sandbox_permissions*/ true,
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
            + &shell_command_guidance_description();

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
        /*allow_escalated_sandbox_permissions*/ true,
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
        assert!(branches.iter().all(|branch| {
            branch["properties"]["kind"]["description"]
                .as_str()
                .is_some_and(|description| {
                    description.contains("Explicit command encoding")
                        && !description.contains("legacy")
                })
        }));
    }
}

#[test]
fn command_tools_define_common_parameters_once_and_reference_them_from_each_form() {
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
        let parameters = &tool["parameters"];
        let definitions = parameters["$defs"]
            .as_object()
            .expect("common parameter definitions");
        let branches = parameters["oneOf"].as_array().expect("command forms");
        assert!(!definitions.is_empty());

        for branch in branches {
            let properties = branch["properties"]
                .as_object()
                .expect("command form properties");
            for name in definitions.keys() {
                assert_eq!(
                    properties[name],
                    json!({"$ref": format!("#/$defs/{name}")}),
                    "common parameter `{name}` should reference its single definition"
                );
            }
        }
    }
}

#[test]
fn command_tool_schema_deduplication_preserves_validation_and_reduces_bytes() {
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
        let inline = inline_common_command_parameter_schemas(parameters);
        assert!(
            serde_json::to_vec(parameters)
                .expect("serialize referenced command schema")
                .len()
                < serde_json::to_vec(&inline)
                    .expect("serialize inline command schema")
                    .len()
        );

        let referenced_validator = jsonschema::validator_for(parameters)
            .expect("referenced command schema should compile");
        let inline_validator =
            jsonschema::validator_for(&inline).expect("inline command schema should compile");

        let mut script = json!({
            "kind": "script",
            "workdir": "repo",
            "validation": {"covered_paths": ["src"]}
        });
        script[script_field] = json!("git status --short");
        let mut mixed_form = script.clone();
        mixed_form["program"] = json!("git");
        let cases = [
            (script, true),
            (
                json!({
                    "kind": "argv",
                    "program": "git",
                    "args": ["status", "--short"],
                    "force_fresh": true
                }),
                true,
            ),
            (
                json!({"kind": "powershell_script", "script_body": "Get-ChildItem"}),
                true,
            ),
            (json!({"program": "git", "args": ["status"]}), false),
            (mixed_form, false),
            (
                json!({"kind": "argv", "program": "git", "unknown": true}),
                false,
            ),
            (
                json!({"kind": "argv", "program": "git", "workdir": 42}),
                false,
            ),
            (
                json!({
                    "kind": "argv",
                    "program": "git",
                    "validation": {"covered_paths": "src"}
                }),
                false,
            ),
        ];

        for (arguments, expected) in cases {
            assert_eq!(
                referenced_validator.is_valid(&arguments),
                inline_validator.is_valid(&arguments),
                "referenced and inline schemas disagree for {arguments}"
            );
            assert_eq!(
                referenced_validator.is_valid(&arguments),
                expected,
                "unexpected command-schema result for {arguments}"
            );
        }
    }
}

#[test]
fn integer_arguments_expose_destination_types_and_runtime_bounds() {
    let exec = serde_json::to_value(create_exec_command_tool(CommandToolOptions {
        allow_login_shell: true,
        exec_permission_approvals_enabled: false,
    }))
    .expect("serialize exec tool");
    let exec_yield =
        resolved_command_parameter_schema(&exec, &exec["parameters"]["oneOf"][0], "yield_time_ms");
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
