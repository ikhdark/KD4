use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::io::Write;

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
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnTimingToolCallSource;
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
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
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
    fn parse_wall_time(value: &str) -> Result<f64> {
        value
            .strip_suffix(" seconds")
            .context("missing wall-time unit")?
            .parse::<f64>()
            .context("failed to parse wall time seconds")
    }

    let cleaned = raw.replace("\r\n", "\n");
    let (headers, output) = cleaned
        .trim_matches('\r')
        .split_once("\nOutput:")
        .ok_or_else(|| anyhow::anyhow!("missing Output section in unified exec output {raw}"))?;
    let output = output.strip_prefix('\n').unwrap_or(output).to_string();

    let mut chunk_id = None;
    let mut wall_time_seconds = None;
    let mut process_id = None;
    let mut exit_code = None;
    let mut original_token_count = None;

    for line in headers.lines() {
        if let Some(value) = line.strip_prefix("Chunk ID: ") {
            chunk_id = Some(value.to_string());
            continue;
        }
        if let Some(value) = line.strip_prefix("Original token count: ") {
            original_token_count = Some(
                value
                    .parse::<usize>()
                    .context("failed to parse original token count from unified exec output")?,
            );
            continue;
        }
        if let Some(value) = line.strip_prefix("Process exited with code ") {
            if let Some((code, wall_time)) = value.split_once("; wall time: ") {
                exit_code = Some(
                    code.parse::<i32>()
                        .context("failed to parse exit code from unified exec output")?,
                );
                wall_time_seconds = Some(parse_wall_time(wall_time)?);
            } else {
                exit_code = Some(
                    value
                        .parse::<i32>()
                        .context("failed to parse exit code from unified exec output")?,
                );
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("Process running with session ID ") {
            if let Some((id, wall_time)) = value.split_once("; wall time: ") {
                process_id = Some(id.to_string());
                wall_time_seconds = Some(parse_wall_time(wall_time)?);
            } else {
                process_id = Some(value.to_string());
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("Wall time: ") {
            wall_time_seconds = Some(parse_wall_time(value)?);
        }
    }

    let wall_time_seconds =
        wall_time_seconds.context("missing wall time in unified exec output")?;

    Ok(ParsedUnifiedExecOutput {
        chunk_id,
        wall_time_seconds,
        process_id,
        exit_code,
        original_token_count,
        output,
    })
}

#[test]
fn token_efficiency_unified_exec_parser_accepts_compact_and_legacy_headers() {
    let compact = parse_unified_exec_output(
        "Process exited with code 0; wall time: 1.2500 seconds\nOutput:\ndone",
    )
    .expect("compact unified exec output should parse");
    assert_eq!(compact.exit_code, Some(0));
    assert_eq!(compact.wall_time_seconds, 1.25);
    assert_eq!(compact.chunk_id, None);
    assert_eq!(compact.original_token_count, None);
    assert_eq!(compact.output, "done");

    let legacy = parse_unified_exec_output(
        "Chunk ID: chunk-42\nWall time: 0.5000 seconds\nProcess running with session ID 1000\nOriginal token count: 12\nOutput:\nwaiting",
    )
    .expect("legacy unified exec output should remain parseable");
    assert_eq!(legacy.wall_time_seconds, 0.5);
    assert_eq!(legacy.process_id.as_deref(), Some("1000"));
    assert_eq!(legacy.chunk_id.as_deref(), Some("chunk-42"));
    assert_eq!(legacy.original_token_count, Some(12));
    assert_eq!(legacy.output, "waiting");
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

#[test]
#[ignore = "subprocess fixture for retained unified-exec lifecycle coverage"]
fn retained_process_child_fixture() {
    fn wait_for_control(reader: &mut impl Read, expected: &[u8]) {
        let mut received = Vec::new();
        loop {
            let mut chunk = [0_u8; 64];
            let count = reader
                .read(&mut chunk)
                .expect("read retained-process control input");
            assert_ne!(count, 0, "retained-process control stdin closed");
            received.extend_from_slice(&chunk[..count]);
            if received
                .windows(expected.len())
                .any(|window| window == expected)
            {
                return;
            }
        }
    }

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    writeln!(writer, "__KD4_RETAINED_READY__").expect("write retained ready marker");
    writer.flush().expect("flush retained ready marker");
    wait_for_control(&mut reader, b"poll");

    writeln!(writer, "__KD4_RETAINED_POLL_ACK__").expect("write retained poll marker");
    writer.flush().expect("flush retained poll marker");
    wait_for_control(&mut reader, b"finish");

    writeln!(writer, "__KD4_RETAINED_FINISHED__").expect("write retained finished marker");
    writer.flush().expect("flush retained finished marker");
}

fn retained_process_exec_args(program: &std::path::Path, yield_time_ms: u64) -> Value {
    json!({
        "kind": "argv",
        "program": program,
        "args": [
            "--ignored",
            "--exact",
            "suite::unified_exec::retained_process_child_fixture",
            "--nocapture"
        ],
        "yield_time_ms": yield_time_ms,
        "tty": true,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_command_retained_session_lifecycle_completes_without_stale_processes() -> Result<()> {
    skip_if_no_network!(Ok(()));
    if cfg!(target_arch = "aarch64") {
        return Ok(());
    }
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;

    let fixture_program = std::env::current_exe().context("resolve unified-exec test binary")?;
    let cases = [("lifecycle-delayed-success", 0, "yielded")];

    let delayed_live_poll_call_id = "lifecycle-delayed-live-poll";
    let delayed_live_poll_args = json!({
        "session_id": 1000,
        "chars": "poll\n",
        "yield_time_ms": 250,
    });
    let delayed_terminal_poll_call_id = "lifecycle-delayed-terminal-poll";
    let delayed_terminal_poll_args = json!({
        "session_id": 1000,
        "chars": "finish\n",
        "yield_time_ms": 10_000,
    });

    let mut first_response = vec![ev_response_created("lifecycle-response")];
    for (call_id, _, _) in cases {
        let arguments = retained_process_exec_args(&fixture_program, 10);
        first_response.push(ev_function_call(
            call_id,
            "exec_command",
            &serde_json::to_string(&arguments)?,
        ));
    }
    first_response.push(ev_completed("lifecycle-response"));
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(first_response),
            sse(vec![
                ev_response_created("lifecycle-live-poll"),
                ev_function_call(
                    delayed_live_poll_call_id,
                    "write_stdin",
                    &serde_json::to_string(&delayed_live_poll_args)?,
                ),
                ev_completed("lifecycle-live-poll"),
            ]),
            sse(vec![
                ev_response_created("lifecycle-terminal-poll"),
                ev_function_call(
                    delayed_terminal_poll_call_id,
                    "write_stdin",
                    &serde_json::to_string(&delayed_terminal_poll_args)?,
                ),
                ev_completed("lifecycle-terminal-poll"),
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
        "lifecycle-delayed-success",
        delayed_live_poll_call_id,
        delayed_terminal_poll_call_id,
    ];
    let deadline = tokio::time::Instant::now() + UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT;
    let mut outputs = HashMap::new();
    let mut raw_outputs = HashMap::new();
    let mut timing = None;
    while outputs.len() < expected_call_ids.len() || timing.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "controlled exec lifecycle exceeded 30 seconds; parsed outputs: {outputs:#?}; raw outputs: {raw_outputs:#?}; provider requests: {}; captured request outputs: {:#?}",
            response_mock.requests().len(),
            expected_call_ids
                .iter()
                .map(|call_id| (*call_id, response_mock.function_call_output_text(call_id)))
                .collect::<Vec<_>>(),
        );
        let event = match tokio::time::timeout(remaining, test.codex.next_event()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!(
                "controlled exec event stream failed: {err:#}; parsed outputs: {outputs:#?}; raw outputs: {raw_outputs:#?}; provider requests: {}; captured request outputs: {:#?}",
                response_mock.requests().len(),
                expected_call_ids
                    .iter()
                    .map(|call_id| (*call_id, response_mock.function_call_output_text(call_id)))
                    .collect::<Vec<_>>(),
            ),
            Err(err) => panic!(
                "timed out waiting for controlled exec lifecycle: {err}; parsed outputs: {outputs:#?}; raw outputs: {raw_outputs:#?}; provider requests: {}; captured request outputs: {:#?}",
                response_mock.requests().len(),
                expected_call_ids
                    .iter()
                    .map(|call_id| (*call_id, response_mock.function_call_output_text(call_id)))
                    .collect::<Vec<_>>(),
            ),
        };
        match event.msg {
            EventMsg::RawResponseItem(raw) => {
                if let ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } = raw.item
                    && expected_call_ids.contains(&call_id.as_str())
                    && let Some(content) = output.text_content()
                {
                    raw_outputs.insert(call_id.clone(), content.to_string());
                    let parsed = parse_unified_exec_output(content).unwrap_or_else(|err| {
                        panic!(
                            "failed to parse raw unified exec output for {call_id}: {err:#}; raw output: {content:?}; provider requests: {}; captured request outputs: {:#?}",
                            response_mock.requests().len(),
                            expected_call_ids
                                .iter()
                                .map(|expected_call_id| (
                                    *expected_call_id,
                                    response_mock.function_call_output_text(expected_call_id),
                                ))
                                .collect::<Vec<_>>(),
                        )
                    });
                    outputs.insert(call_id, parsed);
                }
            }
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    outputs.len(),
                    expected_call_ids.len(),
                    "turn completed before every retained-process call produced output; parsed outputs: {outputs:#?}; raw outputs: {raw_outputs:#?}; provider requests: {}; captured request outputs: {:#?}",
                    response_mock.requests().len(),
                    expected_call_ids
                        .iter()
                        .map(|call_id| (*call_id, response_mock.function_call_output_text(call_id)))
                        .collect::<Vec<_>>(),
                );
                timing = event.timing;
            }
            EventMsg::Error(event) => panic!(
                "unexpected retained-process turn error: {}; parsed outputs: {outputs:#?}; raw outputs: {raw_outputs:#?}; provider requests: {}; captured request outputs: {:#?}",
                event.message,
                response_mock.requests().len(),
                expected_call_ids
                    .iter()
                    .map(|call_id| (*call_id, response_mock.function_call_output_text(call_id)))
                    .collect::<Vec<_>>(),
            ),
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

    for (call_id, expected_exit_code, expected_outcome) in cases {
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

    let delayed_live_poll = outputs
        .get(delayed_live_poll_call_id)
        .expect("missing delayed live lifecycle poll output");
    assert_eq!(delayed_live_poll.process_id.as_deref(), Some("1000"));
    assert_eq!(delayed_live_poll.exit_code, None);
    assert!(
        delayed_live_poll
            .output
            .contains("__KD4_RETAINED_POLL_ACK__"),
        "live poll must causally acknowledge the first write: {delayed_live_poll:?}"
    );
    let delayed_terminal_poll = outputs
        .get(delayed_terminal_poll_call_id)
        .expect("missing delayed terminal lifecycle poll output");
    assert_eq!(delayed_terminal_poll.exit_code, Some(0));
    assert!(delayed_terminal_poll.process_id.is_none());
    assert!(
        delayed_terminal_poll
            .output
            .contains("__KD4_RETAINED_FINISHED__"),
        "terminal poll must causally acknowledge the final write: {delayed_terminal_poll:?}"
    );

    let initial_output = outputs
        .get("lifecycle-delayed-success")
        .expect("missing retained-process initial output");
    assert!(
        initial_output.output.contains("__KD4_RETAINED_READY__"),
        "initial yield must observe the child readiness marker: {initial_output:?}"
    );

    let requests = response_mock.requests();
    assert_eq!(
        requests.len(),
        4,
        "one initial request plus three continuations"
    );
    for (request_index, call_id, expected_marker) in [
        (1, "lifecycle-delayed-success", "__KD4_RETAINED_READY__"),
        (2, delayed_live_poll_call_id, "__KD4_RETAINED_POLL_ACK__"),
        (
            3,
            delayed_terminal_poll_call_id,
            "__KD4_RETAINED_FINISHED__",
        ),
    ] {
        let output = requests[request_index]
            .function_call_output_text(call_id)
            .unwrap_or_else(|| panic!("request {request_index} missing output for {call_id}"));
        assert!(
            output.contains(expected_marker),
            "request {request_index} must contain {expected_marker}: {output}"
        );
    }

    assert_eq!(timing.tool_closure.accepted_count, 3);
    assert_eq!(timing.tool_closure.timing_paired_count, 3);
    assert_eq!(timing.tool_closure.terminal_count, 3);
    assert_eq!(timing.tool_closure.persisted_count, 3);
    assert!(timing.tool_closure.complete);
    assert_eq!(timing.counters.logical_generation_count, 4);
    assert_eq!(timing.counters.generations_by_reason.initial, 1);
    assert_eq!(timing.counters.generations_by_reason.tool_continuation, 3);

    let emitting_request = timing
        .model_requests
        .iter()
        .find(|request| request.model_emitted_tool_call_count > 0)
        .expect("model request owning the controlled tool calls");
    assert_eq!(emitting_request.model_emitted_tool_call_count, 1);
    assert_eq!(emitting_request.tool_call_count, 1);
    assert_eq!(emitting_request.executor_admitted_tool_call_count, 1);
    assert_eq!(emitting_request.executor_max_concurrent_tool_calls, 1);

    Ok(())
}

#[test]
#[ignore]
fn fast_success_child_fixture() {}

#[test]
#[ignore]
fn fast_failure_child_fixture() {
    std::process::exit(7);
}

fn fast_lifecycle_exec_args(program: &std::path::Path, fixture: &str) -> Value {
    json!({
        "kind": "argv",
        "program": program,
        "args": ["--ignored", "--exact", fixture, "--nocapture"],
        "yield_time_ms": 10_000,
        "tty": false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_command_fast_success_and_failure_lifecycles_finish_inline() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let fixture_program = std::env::current_exe().context("resolve unified-exec test binary")?;

    let cases = [
        (
            "lifecycle-fast-success",
            "suite::unified_exec::fast_success_child_fixture",
            0,
            "success",
        ),
        (
            "lifecycle-fast-failure",
            "suite::unified_exec::fast_failure_child_fixture",
            7,
            "failure",
        ),
    ];
    let mut first_response = vec![ev_response_created("fast-lifecycle-response")];
    for (call_id, fixture, _, _) in cases {
        let arguments = fast_lifecycle_exec_args(&fixture_program, fixture);
        first_response.push(ev_function_call(
            call_id,
            "exec_command",
            &serde_json::to_string(&arguments)?,
        ));
    }
    first_response.push(ev_completed("fast-lifecycle-response"));
    let response_mock = mount_sse_sequence(&server, vec![sse(first_response)]).await;

    submit_unified_exec_turn(
        &test,
        "run the fast exec lifecycle cases",
        PermissionProfile::Disabled,
    )
    .await?;

    let expected_call_ids = ["lifecycle-fast-success", "lifecycle-fast-failure"];
    let deadline = tokio::time::Instant::now() + UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT;
    let mut outputs = HashMap::new();
    let mut completed = None;
    let mut errors = Vec::new();
    while outputs.len() < expected_call_ids.len() || completed.is_none() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "fast exec lifecycle exceeded 30 seconds; received outputs for {:?}",
            outputs.keys().collect::<Vec<_>>()
        );
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .context("fast exec lifecycle exceeded 30 seconds")??;
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
            EventMsg::TurnComplete(event) => completed = Some(event),
            EventMsg::Error(event) => errors.push(event),
            EventMsg::TurnAborted(event) => {
                anyhow::bail!(
                    "fast lifecycle turn aborted unexpectedly: {:?}",
                    event.reason
                )
            }
            _ => {}
        }
    }
    let completed = completed.expect("required command failure should complete the turn");
    assert_eq!(
        response_mock.requests().len(),
        1,
        "required command failure must stop before another provider request"
    );
    assert_eq!(errors.len(), 1, "required command failure emits one error");
    let error_message = errors[0].message.as_str();
    assert_eq!(
        completed.error.as_ref().map(|error| error.message.as_str()),
        Some(error_message)
    );
    let timing = completed
        .timing
        .expect("turn completion should include timing");
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
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
        assert_eq!(
            output.exit_code,
            Some(expected_exit_code),
            "unexpected terminal status for {call_id}: {output:?}"
        );
        assert!(output.process_id.is_none(), "{call_id} must finish inline");

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
        assert!(
            lifecycle.model_resumed_at_ms.is_none(),
            "terminal failure must stop before model resume"
        );
        assert!(lifecycle.exec_cleanup_state_observed);
        assert!(!lifecycle.background_process_expected);
        assert!(!lifecycle.running_process_after_cleanup);
        assert!(!lifecycle.process_alive_at_delivery);
    }

    let emitting_request = timing
        .model_requests
        .iter()
        .find(|request| request.model_emitted_tool_call_count > 0)
        .expect("model request owning the fast lifecycle calls");
    assert_eq!(emitting_request.model_emitted_tool_call_count, 2);
    assert_eq!(emitting_request.tool_call_count, 2);
    assert_eq!(emitting_request.executor_admitted_tool_call_count, 2);
    assert!(
        (1..=2).contains(&emitting_request.executor_max_concurrent_tool_calls),
        "executor peak must describe the admitted fast lifecycle calls"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_command_interrupt_closes_unpublished_retained_process_before_turn_aborted()
-> Result<()> {
    skip_if_no_network!(Ok(()));
    if cfg!(target_arch = "aarch64") {
        return Ok(());
    }

    const CALL_ID: &str = "retained-interrupt-exec";
    const READY_MARKER: &str = "__KD4_RETAINED_READY__";

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::UnifiedExec)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let fixture_program = std::env::current_exe().context("resolve unified-exec test binary")?;
    let arguments = retained_process_exec_args(&fixture_program, 30_000);
    let response_mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("retained-interrupt-response"),
            ev_function_call(CALL_ID, "exec_command", &serde_json::to_string(&arguments)?),
            ev_completed("retained-interrupt-response"),
        ])],
    )
    .await;

    submit_unified_exec_turn(
        &test,
        "start the controlled retained process and wait",
        PermissionProfile::Disabled,
    )
    .await?;

    let barrier_deadline = tokio::time::Instant::now() + UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT;
    let mut turn_id = None;
    let mut process_id = None;
    let mut streamed_output = Vec::new();
    loop {
        let remaining = barrier_deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for retained-process interrupt barrier"
        );
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .context("timed out waiting for retained-process interrupt barrier")??;
        match event.msg {
            EventMsg::ExecCommandBegin(event) if event.call_id == CALL_ID => {
                turn_id = Some(event.turn_id);
                process_id = event.process_id;
            }
            EventMsg::ExecCommandOutputDelta(event) if event.call_id == CALL_ID => {
                streamed_output.extend_from_slice(&event.chunk);
                if String::from_utf8_lossy(&streamed_output).contains(READY_MARKER) {
                    break;
                }
            }
            EventMsg::Error(event) => {
                anyhow::bail!("turn failed before interrupt: {}", event.message)
            }
            EventMsg::TurnComplete(_) => {
                anyhow::bail!("retained-process turn completed before explicit interrupt")
            }
            EventMsg::TurnAborted(event) => anyhow::bail!(
                "retained-process turn aborted before explicit interrupt: {:?}",
                event.reason
            ),
            _ => {}
        }
    }

    let turn_id = turn_id.context("exec begin event must precede retained child output")?;
    let process_id = process_id.context("retained exec begin must expose its process identity")?;
    let retained_before_interrupt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let terminals = test.codex.list_background_terminals().await;
            if terminals
                .iter()
                .any(|terminal| terminal.item_id == CALL_ID && terminal.process_id == process_id)
            {
                break terminals;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("retained process was never registered before interrupt")?;
    assert_eq!(
        retained_before_interrupt.len(),
        1,
        "the controlled fixture must be the only retained process"
    );

    test.codex.submit(Op::Interrupt).await?;

    let abort_deadline = tokio::time::Instant::now() + UNIFIED_EXEC_LAGGED_OUTPUT_TIMEOUT;
    let mut persisted_outputs = Vec::new();
    let aborted = loop {
        let remaining = abort_deadline.saturating_duration_since(tokio::time::Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "timed out waiting for TurnAborted after retained-process interrupt"
        );
        let event = tokio::time::timeout(remaining, test.codex.next_event())
            .await
            .context("timed out waiting for retained-process TurnAborted")??;
        match event.msg {
            EventMsg::RawResponseItem(raw) => {
                if let ResponseItem::FunctionCallOutput {
                    call_id, output, ..
                } = raw.item
                    && call_id == CALL_ID
                    && let Some(content) = output.text_content()
                {
                    persisted_outputs.push(content.to_string());
                }
            }
            EventMsg::TurnAborted(event) if event.turn_id.as_deref() == Some(turn_id.as_str()) => {
                break event;
            }
            EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                anyhow::bail!("interrupted retained-process turn emitted TurnComplete")
            }
            EventMsg::Error(event) => {
                anyhow::bail!(
                    "turn failed while interrupting retained process: {}",
                    event.message
                )
            }
            _ => {}
        }
    };

    assert_eq!(aborted.reason, TurnAbortReason::Interrupted);
    assert_eq!(
        persisted_outputs.len(),
        1,
        "the interrupted originating call must be persisted exactly once"
    );
    assert!(
        persisted_outputs[0].contains("aborted"),
        "the durable interrupted output must expose the cancellation: {:?}",
        persisted_outputs[0]
    );
    assert!(
        test.codex.list_background_terminals().await.is_empty(),
        "the unpublished retained process must be terminated and removed before TurnAborted"
    );

    let timing = aborted
        .timing
        .expect("TurnAborted must carry the closed timing profile");
    assert!(timing.profile_valid);
    assert!(timing.classification_complete);
    assert_eq!(timing.exclusive.unclassified_ns, 0);
    assert_eq!(timing.terminalization.unclassified_ns, 0);
    assert_eq!(timing.counters.invalid_transition_count, 0);
    assert_eq!(timing.counters.clock_regression_count, 0);
    assert_eq!(timing.counters.saturation_count, 0);
    assert_eq!(timing.counters.model_request_count, 1);
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.tool_call_count, 1);
    assert_eq!(timing.tool_call_timing_overflow, 0);
    assert_eq!(timing.tool_calls.len(), 1);

    let lifecycle = &timing.tool_calls[0];
    assert_eq!(lifecycle.call_id, CALL_ID);
    assert_eq!(lifecycle.tool_name, "exec_command");
    assert_eq!(lifecycle.source, TurnTimingToolCallSource::Direct);
    assert!(lifecycle.outcome.is_some());
    assert!(lifecycle.process_spawned_at_ms.is_some());
    assert!(lifecycle.process_exited_at_ms.is_some());
    assert!(
        lifecycle.process_spawned_at_ms <= lifecycle.process_exited_at_ms,
        "process exit must not precede process spawn: {lifecycle:?}"
    );
    assert!(
        lifecycle.process_exited_at_ms <= lifecycle.handler_exit_at_ms,
        "the handler must not return before retained-process exit: {lifecycle:?}"
    );
    assert!(lifecycle.output_collected_at_ms.is_some());
    assert!(
        lifecycle.output_projection_ms.is_some(),
        "the synthesized abort result must record its model-visible projection"
    );
    assert!(lifecycle.output_model_visible_at_ms.is_some());
    assert!(lifecycle.model_resumed_at_ms.is_none());
    assert!(!lifecycle.background_process_expected);

    let closure = &timing.tool_closure;
    assert_eq!(closure.accepted_count, 1);
    assert_eq!(closure.timing_paired_count, 1);
    assert_eq!(closure.terminal_count, 1);
    assert_eq!(closure.persisted_count, 1);
    assert_eq!(closure.duplicate_call_id_count, 0);
    assert_eq!(closure.duplicate_acceptance_count, 0);
    assert_eq!(closure.duplicate_timing_count, 0);
    assert_eq!(closure.duplicate_persistence_count, 0);
    assert_eq!(closure.orphan_timing_count, 0);
    assert_eq!(closure.orphan_persistence_count, 0);
    assert_eq!(closure.overflow_count, 0);
    assert!(closure.unresolved_calls.is_empty());
    assert!(closure.orphan_calls.is_empty());
    assert!(closure.complete);

    assert_eq!(
        response_mock.requests().len(),
        1,
        "interrupt must not resume the model after the originating exec call"
    );

    let no_completion_deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    loop {
        let remaining =
            no_completion_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, test.codex.next_event()).await {
            Ok(Ok(event)) => match event.msg {
                EventMsg::TurnComplete(event) if event.turn_id == turn_id => {
                    anyhow::bail!("interrupted retained-process turn forged a late TurnComplete")
                }
                EventMsg::Error(event) => {
                    anyhow::bail!("late retained-process error: {}", event.message)
                }
                _ => {}
            },
            Ok(Err(_)) | Err(_) => break,
        }
    }

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
async fn unified_exec_runs_on_windows() -> Result<()> {
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
        "cmd": "Write-Output 'hello windows'",
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

    assert_regex_match(".*hello windows.*", &output.output);

    Ok(())
}
