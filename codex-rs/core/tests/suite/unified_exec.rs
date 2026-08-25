use std::collections::HashMap;
use std::fs;
use std::sync::OnceLock;

use anyhow::Context;
use anyhow::Result;
use codex_exec_server::CreateDirectoryOptions;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::assert_regex_match;
use core_test_support::managed_network_requirements_loader;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_wine_exec;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use regex_lite::Regex;
use serde_json::Value;
use serde_json::json;
use tokio::time::Duration;

const UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

fn extract_output_text(item: &Value) -> Option<&str> {
    item.get("output").and_then(|value| match value {
        Value::String(text) => Some(text.as_str()),
        Value::Object(obj) => obj.get("content").and_then(Value::as_str),
        _ => None,
    })
}

#[allow(dead_code)]
#[derive(Debug)]
struct ParsedUnifiedExecOutput {
    chunk_id: Option<String>,
    wall_time_seconds: f64,
    process_id: Option<String>,
    exit_code: Option<i32>,
    original_token_count: Option<usize>,
    output: String,
}

fn parse_unified_exec_output(raw: &str) -> Result<ParsedUnifiedExecOutput> {
    static OUTPUT_REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = OUTPUT_REGEX.get_or_init(|| {
        Regex::new(concat!(
            r#"(?s)^(?:Warning: truncated output \(original token count: \d+\)\n)?(?:Total output lines: \d+\n\n)?"#,
            r#"(?:Chunk ID: (?P<chunk_id>[^\n]+)\n)?"#,
            r#"Wall time: (?P<wall_time>-?\d+(?:\.\d+)?) seconds\n"#,
            r#"(?:Process exited with code (?P<exit_code>-?\d+)\n)?"#,
            r#"(?:Process running with session ID (?P<process_id>-?\d+)\n)?"#,
            r#"(?:Original token count: (?P<original_token_count>\d+)\n)?"#,
            r#"(?:Command preflight applied one read-only equivalent repair \([^\n]+\) before execution\.\nOriginal: [^\n]*\nExecuted: [^\n]*\n)?"#,
            r#"(?:Raw output artifact(?: unavailable)?: [^\n]+\n)?"#,
            r#"Output:\n?(?P<output>.*)$"#,
        ))
        .expect("valid unified exec output regex")
    });

    let cleaned = raw.trim_matches('\r');
    let captures = regex
        .captures(cleaned)
        .ok_or_else(|| anyhow::anyhow!("missing Output section in unified exec output {raw}"))?;

    let chunk_id = captures
        .name("chunk_id")
        .map(|value| value.as_str().to_string());

    let wall_time_seconds = captures
        .name("wall_time")
        .expect("wall_time group present")
        .as_str()
        .parse::<f64>()
        .context("failed to parse wall time seconds")?;

    let exit_code = captures
        .name("exit_code")
        .map(|value| {
            value
                .as_str()
                .parse::<i32>()
                .context("failed to parse exit code from unified exec output")
        })
        .transpose()?;

    let process_id = captures
        .name("process_id")
        .map(|value| value.as_str().to_string());

    let original_token_count = captures
        .name("original_token_count")
        .map(|value| {
            value
                .as_str()
                .parse::<usize>()
                .context("failed to parse original token count from unified exec output")
        })
        .transpose()?;

    let output = captures
        .name("output")
        .expect("output group present")
        .as_str()
        .to_string();

    Ok(ParsedUnifiedExecOutput {
        chunk_id,
        wall_time_seconds,
        process_id,
        exit_code,
        original_token_count,
        output,
    })
}

fn collect_tool_outputs(bodies: &[Value]) -> Result<HashMap<String, ParsedUnifiedExecOutput>> {
    let mut outputs = HashMap::new();
    for body in bodies {
        if let Some(items) = body.get("input").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("function_call_output") {
                    continue;
                }
                if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                    let content = extract_output_text(item)
                        .ok_or_else(|| anyhow::anyhow!("missing tool output content"))?;
                    let trimmed = content.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let parsed = parse_unified_exec_output(content).with_context(|| {
                        format!("failed to parse unified exec output for {call_id}")
                    })?;
                    outputs.insert(call_id.to_string(), parsed);
                }
            }
        }
    }
    Ok(outputs)
}

async fn submit_unified_exec_turn(
    test: &TestCodex,
    prompt: &str,
    permission_profile: PermissionProfile,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(permission_profile, test.config.cwd.as_path());

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    Ok(())
}

#[allow(dead_code)]
async fn create_workspace_directory(
    test: &TestCodex,
    rel_path: impl AsRef<std::path::Path>,
) -> Result<std::path::PathBuf> {
    let abs_path = test.config.cwd.join(rel_path.as_ref());
    let abs_path_uri = PathUri::from_host_native_path(&abs_path)?;
    test.fs()
        .create_directory(
            &abs_path_uri,
            CreateDirectoryOptions { recursive: true },
            /*sandbox*/ None,
        )
        .await?;
    Ok(abs_path.into_path_buf())
}

fn controlled_lifecycle_exec_args(script_body: &str) -> Value {
    json!({
        "kind": "powershell_script",
        "script_body": script_body,
        "yield_time_ms": 10_000,
        "tty": false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_command_controlled_lifecycle_trio_completes_without_stale_processes() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let cases = [
        ("lifecycle-fast-success", "exit 0", 0, "success"),
        ("lifecycle-fast-failure", "exit 7", 7, "failure"),
        (
            "lifecycle-delayed-success",
            "Write-Output 'started'; Start-Sleep -Milliseconds 2250; exit 0",
            0,
            "timed_out",
        ),
    ];

    let delayed_poll_call_id = "lifecycle-delayed-poll";
    let delayed_poll_args = json!({
        "session_id": 1000,
        "chars": "",
        "yield_time_ms": 5_000,
    });

    let mut first_response = vec![ev_response_created("lifecycle-response")];
    for (call_id, script_body, _, _) in cases {
        let arguments = controlled_lifecycle_exec_args(script_body);
        first_response.push(ev_function_call(
            call_id,
            "exec_command",
            &serde_json::to_string(&arguments)?,
        ));
    }
    first_response.push(ev_completed("lifecycle-response"));
    mount_sse_sequence(
        &server,
        vec![
            sse(first_response),
            sse(vec![
                ev_response_created("lifecycle-poll"),
                ev_function_call(
                    delayed_poll_call_id,
                    "write_stdin",
                    &serde_json::to_string(&delayed_poll_args)?,
                ),
                ev_completed("lifecycle-poll"),
            ]),
            sse(vec![
                ev_response_created("lifecycle-final"),
                ev_assistant_message("lifecycle-message", "done"),
                ev_completed("lifecycle-final"),
            ]),
        ],
    )
    .await;

    submit_unified_exec_turn(
        &test,
        "run the controlled exec lifecycle cases",
        PermissionProfile::Disabled,
    )
    .await?;

    let expected_call_ids = [
        "lifecycle-fast-success",
        "lifecycle-fast-failure",
        "lifecycle-delayed-success",
        delayed_poll_call_id,
    ];
    let deadline = tokio::time::Instant::now() + UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT;
    let mut outputs = HashMap::new();
    let mut timing = None;
    while outputs.len() < expected_call_ids.len() || timing.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "controlled exec lifecycle exceeded 30 seconds; received outputs for {:?}",
            outputs.keys().collect::<Vec<_>>()
        );
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .context("controlled exec lifecycle exceeded 30 seconds")??;
        match event.msg {
            EventMsg::RawResponseItem(raw) => {
                if let ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } = raw.item
                    && expected_call_ids.contains(&call_id.as_str())
                    && let Some(content) = output.text_content()
                {
                    let parsed = parse_unified_exec_output(content).with_context(|| {
                        format!("failed to parse raw unified exec output for {call_id}")
                    })?;
                    outputs.insert(call_id, parsed);
                }
            }
            EventMsg::TurnComplete(event) => timing = event.timing,
            _ => {}
        }
    }
    let timing = timing.expect("turn completion should include timing");

    let lifecycle_by_id = timing
        .tool_calls
        .iter()
        .filter(|call| call.tool_name == "exec_command")
        .map(|call| (call.call_id.as_str(), call))
        .collect::<HashMap<_, _>>();

    for (call_id, _, expected_exit_code, expected_outcome) in cases {
        let output = outputs
            .get(call_id)
            .unwrap_or_else(|| panic!("missing output for {call_id}"));
        let background_expected = call_id == "lifecycle-delayed-success";
        if background_expected {
            assert_eq!(output.process_id.as_deref(), Some("1000"));
            assert_eq!(output.exit_code, None);
        } else {
            assert_eq!(
                output.exit_code,
                Some(expected_exit_code),
                "unexpected terminal status for {call_id}: {output:?}"
            );
            assert!(output.process_id.is_none(), "{call_id} must finish inline");
        }

        let lifecycle = lifecycle_by_id
            .get(call_id)
            .unwrap_or_else(|| panic!("missing lifecycle for {call_id}"));
        assert_eq!(lifecycle.outcome.as_deref(), Some(expected_outcome));
        assert!(lifecycle.first_poll_at_ms.is_some());
        assert!(lifecycle.parallel_gate_admitted_at_ms.is_some());
        assert!(lifecycle.handler_entry_at_ms.is_some());
        assert!(lifecycle.handler_exit_at_ms.is_some());
        assert!(lifecycle.process_spawned_at_ms.is_some());
        assert!(lifecycle.process_exited_at_ms.is_some());
        assert!(lifecycle.output_collected_at_ms.is_some());
        assert!(lifecycle.delivered_at_ms.is_some());
        assert!(lifecycle.output_model_visible_at_ms.is_some());
        assert!(lifecycle.model_resumed_at_ms.is_some());
        assert!(lifecycle.exec_cleanup_state_observed);
        assert_eq!(lifecycle.background_process_expected, background_expected);
        assert_eq!(lifecycle.running_process_after_cleanup, background_expected);
        assert_eq!(lifecycle.process_alive_at_delivery, background_expected);
    }

    let delayed_poll = outputs
        .get(delayed_poll_call_id)
        .expect("missing delayed lifecycle poll output");
    assert_eq!(delayed_poll.exit_code, Some(0));
    assert!(delayed_poll.process_id.is_none());

    let emitting_request = timing
        .model_requests
        .iter()
        .find(|request| request.model_emitted_tool_call_count > 0)
        .expect("model request owning the controlled tool calls");
    assert_eq!(emitting_request.model_emitted_tool_call_count, 3);
    assert_eq!(emitting_request.tool_call_count, 3);
    assert_eq!(emitting_request.executor_admitted_tool_call_count, 3);
    assert!(
        (1..=3).contains(&emitting_request.executor_max_concurrent_tool_calls),
        "executor peak must describe the admitted controlled calls"
    );

    let delayed = lifecycle_by_id
        .get("lifecycle-delayed-success")
        .copied()
        .expect("delayed lifecycle");
    let delayed_process_duration_ms = delayed
        .process_spawned_at_ms
        .zip(delayed.process_exited_at_ms)
        .map(|(spawned, exited)| exited.saturating_sub(spawned));
    assert!(
        delayed_process_duration_ms.is_some_and(|duration| duration >= 2_000),
        "delayed control must remain alive for at least two seconds: {delayed:?}"
    );

    Ok(())
}

#[allow(dead_code)]
async fn unified_exec_network_denial_test(
    server: &wiremock::MockServer,
) -> Result<(TestCodex, PermissionProfile)> {
    use codex_config::Constrained;
    use std::sync::Arc;
    use tempfile::TempDir;

    let home = Arc::new(TempDir::new()?);
    fs::write(
        home.path().join("config.toml"),
        r#"default_permissions = "workspace"

[permissions.workspace.filesystem]
":minimal" = "read"

[permissions.workspace.network]
enabled = true
mode = "limited"
allow_local_binding = true
"#,
    )?;
    let permission_profile_for_config = PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Enabled,
        /*exclude_tmpdir_env_var*/ false,
        /*exclude_slash_tmp*/ false,
    );
    let permission_profile = permission_profile_for_config.clone();
    let mut builder = test_codex()
        .with_home(home)
        .with_cloud_config_bundle(managed_network_requirements_loader())
        .with_config(move |config| {
            config.unified_exec_enabled = true;
            config
                .features
                .enable(Feature::UnifiedExec)
                .expect("test config should allow feature update");
            config.permissions.approval_policy = Constrained::allow_any(AskForApproval::Never);
            config
                .permissions
                .set_permission_profile(permission_profile_for_config)
                .expect("set permission profile");
        });
    let test = builder.build_with_auto_env(server).await?;
    assert!(
        test.config.permissions.network.is_some(),
        "expected managed network proxy config to be present"
    );

    Ok((test, permission_profile))
}

#[allow(dead_code)]
async fn mount_unified_exec_network_denial_responses(
    server: &wiremock::MockServer,
    call_id: &str,
    args: &Value,
) -> Result<core_test_support::responses::ResponseMock> {
    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_assistant_message("msg-1", "finished"),
            ev_completed("resp-2"),
        ]),
    ];
    Ok(mount_sse_sequence(server, responses).await)
}

#[allow(dead_code)]
async fn wait_for_unified_exec_end(
    test: &TestCodex,
    call_id: &str,
    response_mock: &core_test_support::responses::ResponseMock,
) -> (codex_protocol::protocol::ExecCommandEndEvent, bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut observed_events = Vec::new();
    let mut turn_completed = false;
    let end_event = loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            panic!(
                "timed out waiting for network denial end event; observed {observed_events:?}; response requests: {}",
                response_mock.requests().len()
            );
        }
        let timeout_message = format!(
            "timed out waiting for network denial end event; observed {observed_events:?}; response requests: {}",
            response_mock.requests().len()
        );
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .expect(&timeout_message)
            .expect("event stream ended unexpectedly")
            .msg;
        turn_completed |= matches!(event, EventMsg::TurnComplete(_));
        observed_events.push(format!("{event:?}"));
        if let EventMsg::ExecCommandEnd(ev) = event
            && ev.call_id == call_id
        {
            break ev;
        }
    };
    (end_event, turn_completed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unified_exec_owner_wait_delivers_terminal_output_before_model_resumes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let open_args = json!({
        "kind": "powershell_script",
        // Windows enforces a two-second floor for the initial exec wait while the executor
        // warms up. Keep the process alive beyond that floor so `write_stdin` owns the
        // terminal wait that observes completion.
        "script_body": "Start-Sleep -Seconds 3; Write-Output POLL_DONE",
        "yield_time_ms": 10,
        // A PTY can emit bootstrap control bytes before the command's actual output, which is
        // valid progress and therefore ends an owner wait. A pipe keeps this fixture silent
        // until `POLL_DONE`, isolating the terminal-output delivery contract under test.
        "tty": false,
    });

    let server = start_mock_server().await;
    let poll_args = json!({
        "chars": "",
        "session_id": 1000,
        "yield_time_ms": 3_000,
    });
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("open"),
                ev_function_call(
                    "open-call",
                    "exec_command",
                    &serde_json::to_string(&open_args)?,
                ),
                ev_completed("open"),
            ]),
            sse(vec![
                ev_response_created("owner-wait"),
                ev_function_call(
                    "owner-wait-call",
                    "write_stdin",
                    &serde_json::to_string(&poll_args)?,
                ),
                ev_completed("owner-wait"),
            ]),
            sse(vec![
                ev_response_created("final"),
                ev_assistant_message("message", "complete"),
                ev_completed("final"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.unified_exec_enabled = true;
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let codex = builder.build_with_auto_env(&server).await?;
    submit_unified_exec_turn(
        &codex,
        "run the command and wait for it to finish",
        PermissionProfile::Disabled,
    )
    .await?;
    let completion = loop {
        if let EventMsg::TurnComplete(event) = wait_for_event(&codex.codex, |_| true).await {
            break event;
        }
    };

    assert_eq!(completion.last_agent_message.as_deref(), Some("complete"));
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    let final_request = requests[2].body_json();
    let owner_wait_output = final_request
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some("owner-wait-call")
            })
        })
        .and_then(extract_output_text)
        .expect("final model request should contain the owner-wait result");
    assert!(
        owner_wait_output.contains("POLL_DONE"),
        "the terminal command output must be present in the request that resumes the model; actual output: {owner_wait_output:?}",
    );

    let timing = completion.timing.expect("turn timing");
    assert_eq!(timing.counters.logical_generation_count, 3);
    assert_eq!(timing.counters.generations_by_reason.initial, 1);
    assert_eq!(timing.counters.generations_by_reason.tool_continuation, 2);
    assert_eq!(timing.counters.tool_call_count, 2);

    Ok(())
}

#[allow(dead_code)]
async fn assert_write_stdin_ctrl_c_interrupts_non_tty_session(
    test_name: &str,
    command: &str,
    expected_exit_code: i32,
    expected_interrupt_output: Option<&str>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = format!("uexec-non-tty-interrupt-{test_name}-start");
    let interrupt_call_id = format!("uexec-non-tty-interrupt-{test_name}");

    let start_args = serde_json::json!({
        "cmd": command,
        "yield_time_ms": 250,
        "tty": false,
    });
    let interrupt_args = serde_json::json!({
        "chars": "\u{3}",
        "session_id": 1000,
        "yield_time_ms": 1000,
    });

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(
                &start_call_id,
                "exec_command",
                &serde_json::to_string(&start_args)?,
            ),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_function_call(
                &interrupt_call_id,
                "write_stdin",
                &serde_json::to_string(&interrupt_args)?,
            ),
            ev_completed("resp-2"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-3"),
        ]),
    ];
    let request_log = mount_sse_sequence(&server, responses).await;

    submit_unified_exec_turn(
        &test,
        "interrupt non-tty unified exec",
        PermissionProfile::Disabled,
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = request_log.requests();
    assert!(!requests.is_empty(), "expected at least one POST request");
    let bodies = requests
        .into_iter()
        .map(|request| request.body_json())
        .collect::<Vec<_>>();

    let outputs = collect_tool_outputs(&bodies)?;

    let start_output = outputs
        .get(&start_call_id)
        .with_context(|| format!("missing start output for exec_command {start_call_id}"))?;
    assert_eq!(
        start_output.process_id.as_deref(),
        Some("1000"),
        "exec_command should leave a running non-TTY session"
    );
    assert!(
        start_output.exit_code.is_none(),
        "initial exec_command should not include exit_code while session is running"
    );
    assert!(
        start_output.output.contains("READY"),
        "start output should include command readiness marker, got {:?}",
        start_output.output
    );

    let interrupt_output = outputs
        .get(&interrupt_call_id)
        .with_context(|| format!("missing interrupt output for write_stdin {interrupt_call_id}"))?;
    assert!(
        interrupt_output.process_id.is_none(),
        "interrupted process should be cleared from the session map"
    );
    assert_eq!(
        interrupt_output.exit_code,
        Some(expected_exit_code),
        "interrupt should preserve the process-reported exit code"
    );
    if let Some(expected_interrupt_output) = expected_interrupt_output {
        assert!(
            interrupt_output.output.contains(expected_interrupt_output),
            "interrupt should drain output from the signal handler, got {:?}",
            interrupt_output.output
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]

async fn write_stdin_ctrl_c_reports_unsupported_interrupt_to_model_on_windows() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let start_call_id = "uexec-windows-interrupt-start";
    let interrupt_call_id = "uexec-windows-interrupt";

    let start_args = serde_json::json!({
        "shell": "cmd",
        "cmd": "echo READY && ping -n 30 127.0.0.1 >NUL",
        "yield_time_ms": 250,
        "tty": false,
    });
    let interrupt_args = serde_json::json!({
        "chars": "\u{3}",
        "session_id": 1000,
        "yield_time_ms": 1000,
    });

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(
                start_call_id,
                "exec_command",
                &serde_json::to_string(&start_args)?,
            ),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_response_created("resp-2"),
            ev_function_call(
                interrupt_call_id,
                "write_stdin",
                &serde_json::to_string(&interrupt_args)?,
            ),
            ev_completed("resp-2"),
        ]),
        sse(vec![
            ev_response_created("resp-3"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-3"),
        ]),
    ];
    let request_log = mount_sse_sequence(&server, responses).await;

    submit_unified_exec_turn(
        &test,
        "interrupt non-tty unified exec on Windows",
        PermissionProfile::Disabled,
    )
    .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let start_output = request_log
        .function_call_output_text(start_call_id)
        .expect("missing start output for exec_command");
    let start_output = parse_unified_exec_output(&start_output)?;
    assert_eq!(
        start_output.process_id.as_deref(),
        Some("1000"),
        "exec_command should leave a running non-TTY session"
    );
    assert!(
        start_output.output.contains("READY"),
        "start output should include command readiness marker, got {:?}",
        start_output.output
    );

    let interrupt_output = request_log
        .function_call_output_text(interrupt_call_id)
        .expect("missing interrupt output for write_stdin");
    assert!(
        interrupt_output.contains("write_stdin failed"),
        "model-visible write_stdin output should report failure, got {interrupt_output:?}"
    );
    assert!(
        interrupt_output.contains("process interrupt is not supported by this process backend"),
        "model-visible write_stdin output should explain unsupported interrupt, got {interrupt_output:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// Skipped on arm because the ctor logic to handle arg0 doesn't work on ARM
async fn unified_exec_runs_on_all_platforms() -> Result<()> {
    // TODO(anp): Remove after PowerShell execution passes through Wine exec.
    skip_if_wine_exec!(
        Ok(()),
        "basic PowerShell execution through Wine exec is not passing yet"
    );
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let call_id = "uexec";
    let args = serde_json::json!({
        "cmd": "echo 'hello crossplat'",
    });

    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "exec_command", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    ];
    let request_log = mount_sse_sequence(&server, responses).await;

    submit_unified_exec_turn(&test, "summarize large output", PermissionProfile::Disabled).await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = request_log.requests();
    assert!(!requests.is_empty(), "expected at least one POST request");
    let bodies = requests
        .into_iter()
        .map(|request| request.body_json())
        .collect::<Vec<_>>();

    let outputs = collect_tool_outputs(&bodies)?;
    let output = outputs.get(call_id).expect("missing output");

    // TODO: Weaker match because windows produces control characters
    assert_regex_match(".*hello crossplat.*", &output.output);

    Ok(())
}

#[allow(dead_code)]
fn assert_command(command: &[String], expected_args: &str, expected_cmd: &str) {
    assert_eq!(command.len(), 3);
    let shell_path = &command[0];
    assert!(
        shell_path == "/bin/bash"
            || shell_path == "/usr/bin/bash"
            || shell_path == "/usr/local/bin/bash"
            || shell_path.ends_with("/bash"),
        "unexpected bash path: {shell_path}"
    );
    assert_eq!(command[1], expected_args);
    assert_eq!(command[2], expected_cmd);
}
