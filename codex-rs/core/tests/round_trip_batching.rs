#![allow(clippy::expect_used)]

// Scripted protocol/accounting regression tests. Request-count differences
// reflect the mock workflows, not model behavior or fork/upstream performance.

use codex_protocol::protocol::TurnTiming;
use core_test_support::require_network;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use wiremock::MockServer;

const EDIT_ARGS: &str = r#"{"sleep_before_ms":1}"#;
const VALIDATE_TEST_ARGS: &str = r#"{"sleep_before_ms":2}"#;
const VALIDATE_FORMAT_ARGS: &str = r#"{"sleep_before_ms":3}"#;
const GIT_DIFF_ARGS: &str = r#"{"sleep_before_ms":4}"#;
#[cfg(windows)]
const FAILING_CHECK_ARGS: &str = r#"{"cmd":"echo controlled-check-diagnostic & exit /b 17","yield_time_ms":10000,"max_output_tokens":1024}"#;
#[cfg(not(windows))]
const FAILING_CHECK_ARGS: &str = r#"{"cmd":"printf 'controlled-check-diagnostic\n'; exit 17","yield_time_ms":10000,"max_output_tokens":1024}"#;
const PARALLEL_ARGS: &str =
    r#"{"barrier":{"id":"round-trip-overlap","participants":3,"timeout_ms":5000}}"#;
const DIAGNOSE_ARGS: &str = r#"{"sleep_after_ms":2}"#;
const TARGETED_VALIDATION_ARGS: &str = r#"{"sleep_after_ms":3}"#;

fn function_call_output_ids(input: &[Value]) -> Vec<&str> {
    input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .collect()
}

type FunctionCall = (String, String, String);
type FunctionCallOutput = (String, String);

fn request_tool_transcript(
    request: &wiremock::Request,
) -> Option<(Vec<FunctionCall>, Vec<FunctionCallOutput>)> {
    let body: Value = serde_json::from_slice(&request.body).ok()?;
    let input = body.get("input")?.as_array()?;
    let calls = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .map(|item| {
            Some((
                item.get("call_id")?.as_str()?.to_string(),
                item.get("name")?.as_str()?.to_string(),
                item.get("arguments")?.as_str()?.to_string(),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let outputs = input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .map(|item| {
            Some((
                item.get("call_id")?.as_str()?.to_string(),
                item.get("output")?.as_str()?.to_string(),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((calls, outputs))
}

async fn mount_semantically_gated_sequence(
    server: &MockServer,
    steps: Vec<(Vec<ExpectedCall>, String)>,
) -> Vec<ResponseMock> {
    let mut mocks = Vec::with_capacity(steps.len());
    for (expected_calls, response) in steps {
        mocks.push(
            mount_sse_once_match(
                server,
                move |request: &wiremock::Request| {
                    let Some((calls, outputs)) = request_tool_transcript(request) else {
                        return false;
                    };
                    calls.len() == expected_calls.len()
                        && outputs.len() == expected_calls.len()
                        && expected_calls.iter().zip(calls).zip(outputs).all(
                            |((expected, call), output)| {
                                call == (
                                    expected.id.to_string(),
                                    expected.tool.to_string(),
                                    expected.arguments.to_string(),
                                ) && output.0 == expected.id
                                    && expected.output.iter().all(|part| output.1.contains(part))
                                    && (expected.tool != "test_sync_tool" || output.1 == "ok")
                            },
                        )
                },
                response,
            )
            .await,
        );
    }
    mocks
}

struct ExpectedCall {
    id: &'static str,
    tool: &'static str,
    arguments: &'static str,
    output: Vec<&'static str>,
}

fn calls(entries: &[(&'static str, &'static str)]) -> Vec<ExpectedCall> {
    entries
        .iter()
        .map(|&(id, arguments)| ExpectedCall {
            id,
            arguments,
            tool: "test_sync_tool",
            output: vec!["ok"],
        })
        .collect()
}

fn diagnosis_calls(entries: &[(&'static str, &'static str)]) -> Vec<ExpectedCall> {
    let mut expected = calls(entries);
    let failed = expected
        .first_mut()
        .expect("diagnostic sequence starts with failed check");
    failed.tool = "exec_command";
    failed.output = vec!["Process exited with code 17", "controlled-check-diagnostic"];
    expected
}

fn requests_for_sequence(
    mocks: &[ResponseMock],
) -> Vec<core_test_support::responses::ResponsesRequest> {
    mocks.iter().flat_map(ResponseMock::requests).collect()
}

fn assert_timing_reconciles(timing: &TurnTiming) {
    let counters = &timing.counters;
    assert_eq!(
        timing.model_requests.len(),
        counters.model_request_count as usize
    );
    assert_eq!(
        counters.model_request_count,
        counters.attempts_by_kind.primary
            + counters.attempts_by_kind.retry
            + counters.attempts_by_kind.fallback
    );
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.model_stream_wait_ns)
            .sum::<u64>(),
        timing.unions.model_stream_wait_union_ns
    );
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.tool_call_count)
            .sum::<u32>(),
        counters.tool_call_count
    );
}

fn print_timing_breakdown(workflow: &str, baseline_requests: u32, timing: &TurnTiming) {
    eprintln!(
        "workflow={workflow} baseline_requests={baseline_requests} logical_generations={} \
         physical_attempts={} tool_continuations={} primary_attempts={} retry_attempts={} \
         fallback_attempts={} tool_calls={} model_stream_wait_ns={}",
        timing.counters.logical_generation_count,
        timing.counters.model_request_count,
        timing.counters.generations_by_reason.tool_continuation,
        timing.counters.attempts_by_kind.primary,
        timing.counters.attempts_by_kind.retry,
        timing.counters.attempts_by_kind.fallback,
        timing.counters.tool_call_count,
        timing.unions.model_stream_wait_union_ns,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_edit_batches_validation_and_git_into_three_model_requests() -> anyhow::Result<()> {
    require_network!();

    let baseline_server = start_mock_server().await;
    let baseline_responses = mount_semantically_gated_sequence(
        &baseline_server,
        vec![
            (
                vec![],
                sse(vec![
                    ev_response_created("edit"),
                    ev_function_call("edit", "test_sync_tool", EDIT_ARGS),
                    ev_completed("edit"),
                ]),
            ),
            (
                calls(&[("edit", EDIT_ARGS)]),
                sse(vec![
                    ev_response_created("validate-test"),
                    ev_function_call("validate-test", "test_sync_tool", VALIDATE_TEST_ARGS),
                    ev_completed("validate-test"),
                ]),
            ),
            (
                calls(&[("edit", EDIT_ARGS), ("validate-test", VALIDATE_TEST_ARGS)]),
                sse(vec![
                    ev_response_created("validate-format"),
                    ev_function_call("validate-format", "test_sync_tool", VALIDATE_FORMAT_ARGS),
                    ev_completed("validate-format"),
                ]),
            ),
            (
                calls(&[
                    ("edit", EDIT_ARGS),
                    ("validate-test", VALIDATE_TEST_ARGS),
                    ("validate-format", VALIDATE_FORMAT_ARGS),
                ]),
                sse(vec![
                    ev_response_created("git-diff"),
                    ev_function_call("git-diff", "test_sync_tool", GIT_DIFF_ARGS),
                    ev_completed("git-diff"),
                ]),
            ),
            (
                calls(&[
                    ("edit", EDIT_ARGS),
                    ("validate-test", VALIDATE_TEST_ARGS),
                    ("validate-format", VALIDATE_FORMAT_ARGS),
                    ("git-diff", GIT_DIFF_ARGS),
                ]),
                sse(vec![
                    ev_assistant_message("done", "done"),
                    ev_completed("complete"),
                ]),
            ),
        ],
    )
    .await;
    let baseline_test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .build(&baseline_server)
        .await?;

    let baseline_completion = baseline_test
        .submit_turn_and_capture_completion(
            "make the edit, then validate it and inspect the Git diff",
        )
        .await?;

    let server = start_mock_server().await;
    let responses = mount_semantically_gated_sequence(
        &server,
        vec![
            (
                vec![],
                sse(vec![
                    ev_response_created("edit"),
                    ev_function_call("edit", "test_sync_tool", EDIT_ARGS),
                    ev_completed("edit"),
                ]),
            ),
            (
                calls(&[("edit", EDIT_ARGS)]),
                sse(vec![
                    ev_response_created("post-edit"),
                    ev_function_call("validate-test", "test_sync_tool", VALIDATE_TEST_ARGS),
                    ev_function_call("validate-format", "test_sync_tool", VALIDATE_FORMAT_ARGS),
                    ev_function_call("git-diff", "test_sync_tool", GIT_DIFF_ARGS),
                    ev_completed("post-edit"),
                ]),
            ),
            (
                calls(&[
                    ("edit", EDIT_ARGS),
                    ("validate-test", VALIDATE_TEST_ARGS),
                    ("validate-format", VALIDATE_FORMAT_ARGS),
                    ("git-diff", GIT_DIFF_ARGS),
                ]),
                sse(vec![
                    ev_assistant_message("done", "done"),
                    ev_completed("complete"),
                ]),
            ),
        ],
    )
    .await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .build(&server)
        .await?;

    let completion = test
        .submit_turn_and_capture_completion(
            "make the edit, then validate it and inspect the Git diff",
        )
        .await?;

    let baseline_requests = requests_for_sequence(&baseline_responses);
    let requests = requests_for_sequence(&responses);
    assert_eq!(
        baseline_completion.last_agent_message.as_deref(),
        completion.last_agent_message.as_deref()
    );
    let baseline_timing = baseline_completion.timing.expect("baseline turn timing");
    let timing = completion.timing.expect("turn timing");
    assert_timing_reconciles(&baseline_timing);
    assert_timing_reconciles(&timing);
    let counters = &timing.counters;
    let serial_request_count = baseline_timing.counters.logical_generation_count;
    assert_eq!(serial_request_count, 5);
    assert_eq!(baseline_timing.counters.tool_call_count, 4);
    assert_eq!(counters.model_request_count, 3);
    assert_eq!(counters.logical_generation_count, 3);
    assert_eq!(counters.generations_by_reason.initial, 1);
    assert_eq!(counters.generations_by_reason.tool_continuation, 2);
    assert_eq!(counters.attempts_by_kind.primary, 3);
    assert!(counters.model_request_count < serial_request_count);
    assert_eq!(counters.tool_call_count, 4);
    assert_eq!(requests.len(), counters.model_request_count as usize);
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.tool_call_count)
            .collect::<Vec<_>>(),
        [1, 3, 0]
    );
    assert_eq!(function_call_output_ids(&requests[1].input()), ["edit"]);
    assert_eq!(
        function_call_output_ids(&requests[2].input()),
        ["edit", "validate-test", "validate-format", "git-diff"]
    );
    assert_eq!(
        function_call_output_ids(&baseline_requests[4].input()),
        function_call_output_ids(&requests[2].input())
    );
    print_timing_breakdown("post_edit_checks", serial_request_count, &timing);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_parallel_tool_calls_reach_immediate_continuation() -> anyhow::Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("parallel"),
                ev_function_call("parallel-1", "test_sync_tool", PARALLEL_ARGS),
                ev_function_call("parallel-2", "test_sync_tool", PARALLEL_ARGS),
                ev_function_call("parallel-3", "test_sync_tool", PARALLEL_ARGS),
                ev_completed("parallel"),
            ]),
            sse(vec![
                ev_assistant_message("done", "done"),
                ev_completed("complete"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .build(&server)
        .await?;

    let completion = test
        .submit_turn_and_capture_completion("run the three independent checks")
        .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let timing = completion.timing.expect("turn timing");
    assert_timing_reconciles(&timing);
    assert_eq!(timing.counters.logical_generation_count, 2);
    assert_eq!(timing.counters.model_request_count, 2);
    assert_eq!(timing.counters.tool_call_count, 3);
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.tool_call_count)
            .collect::<Vec<_>>(),
        [3, 0]
    );

    let continuation = &requests[1];
    assert_eq!(
        function_call_output_ids(&continuation.input()),
        ["parallel-1", "parallel-2", "parallel-3"]
    );
    assert_eq!(
        [
            continuation.function_call_output_text("parallel-1"),
            continuation.function_call_output_text("parallel-2"),
            continuation.function_call_output_text("parallel-3"),
        ],
        [
            Some("ok".to_string()),
            Some("ok".to_string()),
            Some("ok".to_string()),
        ]
    );
    let calls = continuation
        .inputs_of_type("function_call")
        .into_iter()
        .map(|item| {
            (
                item["call_id"].as_str().expect("call id").to_string(),
                item["name"].as_str().expect("tool name").to_string(),
                item["arguments"].as_str().expect("arguments").to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        vec![
            (
                "parallel-1".to_string(),
                "test_sync_tool".to_string(),
                PARALLEL_ARGS.to_string(),
            ),
            (
                "parallel-2".to_string(),
                "test_sync_tool".to_string(),
                PARALLEL_ARGS.to_string(),
            ),
            (
                "parallel-3".to_string(),
                "test_sync_tool".to_string(),
                PARALLEL_ARGS.to_string(),
            ),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnosis_and_dynamic_validation_keep_model_boundaries() -> anyhow::Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let responses = mount_semantically_gated_sequence(
        &server,
        vec![
            (
                vec![],
                sse(vec![
                    ev_response_created("1"),
                    ev_function_call("failing-check", "exec_command", FAILING_CHECK_ARGS),
                    ev_completed("failed"),
                ]),
            ),
            (
                diagnosis_calls(&[("failing-check", FAILING_CHECK_ARGS)]),
                sse(vec![
                    ev_response_created("2"),
                    ev_function_call("diagnose", "test_sync_tool", DIAGNOSE_ARGS),
                    ev_completed("diagnosed"),
                ]),
            ),
            (
                diagnosis_calls(&[
                    ("failing-check", FAILING_CHECK_ARGS),
                    ("diagnose", DIAGNOSE_ARGS),
                ]),
                sse(vec![
                    ev_response_created("3"),
                    ev_function_call(
                        "targeted-validation",
                        "test_sync_tool",
                        TARGETED_VALIDATION_ARGS,
                    ),
                    ev_completed("validated"),
                ]),
            ),
            (
                diagnosis_calls(&[
                    ("failing-check", FAILING_CHECK_ARGS),
                    ("diagnose", DIAGNOSE_ARGS),
                    ("targeted-validation", TARGETED_VALIDATION_ARGS),
                ]),
                sse(vec![
                    ev_assistant_message("done", "done"),
                    ev_completed("complete"),
                ]),
            ),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_raw_response_items();
    #[cfg(windows)]
    {
        builder = builder.with_windows_cmd_shell();
    }
    let test = builder.build(&server).await?;

    let completion = test
        .submit_turn_and_capture_completion("diagnose the failure, then select validation")
        .await?;

    let requests = requests_for_sequence(&responses);
    let timing = completion.timing.expect("turn timing");
    assert_timing_reconciles(&timing);
    let counters = &timing.counters;
    assert_eq!(counters.model_request_count, 4);
    assert_eq!(counters.logical_generation_count, 4);
    assert_eq!(counters.generations_by_reason.initial, 1);
    assert_eq!(counters.generations_by_reason.tool_continuation, 3);
    assert_eq!(counters.tool_call_count, 3);
    assert_eq!(requests.len(), 4);
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.tool_call_count)
            .collect::<Vec<_>>(),
        [1, 1, 1, 0]
    );
    assert_eq!(
        function_call_output_ids(&requests[3].input()),
        ["failing-check", "diagnose", "targeted-validation"]
    );
    print_timing_breakdown("dependent_diagnosis", 4, &timing);

    Ok(())
}

/// Each orchestration rule reaches a root request from exactly one surface. A
/// second copy at a different authority level is not redundant safety: the
/// model has to reconcile the two wordings on every generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_production_request_states_each_orchestration_rule_once() -> anyhow::Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_assistant_message("done", "done"),
            ev_completed("complete"),
        ])],
    )
    .await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .build(&server)
        .await?;

    test.submit_turn_and_capture_completion("inspect the workspace")
        .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 1);
    let developer_text = requests[0].message_input_texts("developer").join("\n\n");
    assert!(
        !developer_text.contains("<root_orchestration_instructions>"),
        "the root orchestration policy block must not be reintroduced"
    );

    // The surviving rules live in the base instructions, which own them for
    // roots and workers alike.
    let instructions = requests[0].instructions_text();
    for rule in [
        "Sequence dependencies: finish edits before checks that validate them.",
        "Serialize conflicting work",
        "Resume the existing process or cell; do not launch a duplicate.",
        "Reuse successful checks of the final source state",
        "When clippy is required, omit a preceding cargo check only if",
    ] {
        assert_eq!(
            instructions.matches(rule).count(),
            1,
            "base instructions must state this rule exactly once: {rule}"
        );
    }

    assert_eq!(
        instructions
            .matches("Batch independent calls using the available tool-native")
            .count(),
        1,
        "base instructions must state the batching policy exactly once"
    );

    Ok(())
}
