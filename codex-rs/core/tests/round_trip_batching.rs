#![allow(clippy::expect_used)]

use codex_features::Feature;
use codex_protocol::protocol::TurnTiming;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;

const EDIT_ARGS: &str = r#"{"sleep_before_ms":1}"#;
const VALIDATE_TEST_ARGS: &str = r#"{"sleep_before_ms":2}"#;
const VALIDATE_FORMAT_ARGS: &str = r#"{"sleep_before_ms":3}"#;
const GIT_DIFF_ARGS: &str = r#"{"sleep_before_ms":4}"#;
const FAILING_CHECK_ARGS: &str = r#"{"sleep_after_ms":1}"#;
const DIAGNOSE_ARGS: &str = r#"{"sleep_after_ms":2}"#;
const TARGETED_VALIDATION_ARGS: &str = r#"{"sleep_after_ms":3}"#;

fn function_call_output_ids(input: &[Value]) -> Vec<&str> {
    input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .collect()
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
    skip_if_no_network!(Ok(()));

    let baseline_server = start_mock_server().await;
    let baseline_responses = mount_sse_sequence(
        &baseline_server,
        vec![
            sse(vec![
                ev_response_created("edit"),
                ev_function_call("edit", "test_sync_tool", EDIT_ARGS),
                ev_completed("edit"),
            ]),
            sse(vec![
                ev_response_created("validate-test"),
                ev_function_call("validate-test", "test_sync_tool", VALIDATE_TEST_ARGS),
                ev_completed("validate-test"),
            ]),
            sse(vec![
                ev_response_created("validate-format"),
                ev_function_call("validate-format", "test_sync_tool", VALIDATE_FORMAT_ARGS),
                ev_completed("validate-format"),
            ]),
            sse(vec![
                ev_response_created("git-diff"),
                ev_function_call("git-diff", "test_sync_tool", GIT_DIFF_ARGS),
                ev_completed("git-diff"),
            ]),
            sse(vec![
                ev_assistant_message("done", "done"),
                ev_completed("complete"),
            ]),
        ],
    )
    .await;
    let baseline_test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            let _ = config.features.disable(Feature::TaskCompletionReviewer);
        })
        .build(&baseline_server)
        .await?;

    let baseline_completion = baseline_test
        .submit_turn_and_capture_completion(
            "make the edit, then validate it and inspect the Git diff",
        )
        .await?;

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("edit"),
                ev_function_call("edit", "test_sync_tool", EDIT_ARGS),
                ev_completed("edit"),
            ]),
            sse(vec![
                ev_response_created("post-edit"),
                ev_function_call("validate-test", "test_sync_tool", VALIDATE_TEST_ARGS),
                ev_function_call("validate-format", "test_sync_tool", VALIDATE_FORMAT_ARGS),
                ev_function_call("git-diff", "test_sync_tool", GIT_DIFF_ARGS),
                ev_completed("post-edit"),
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
        .with_config(|config| {
            let _ = config.features.disable(Feature::TaskCompletionReviewer);
        })
        .build(&server)
        .await?;

    let completion = test
        .submit_turn_and_capture_completion(
            "make the edit, then validate it and inspect the Git diff",
        )
        .await?;

    let baseline_requests = baseline_responses.requests();
    let requests = responses.requests();
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
async fn diagnosis_and_dynamic_validation_keep_model_boundaries() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("1"),
                ev_function_call("failing-check", "test_sync_tool", FAILING_CHECK_ARGS),
                ev_completed("failed"),
            ]),
            sse(vec![
                ev_response_created("2"),
                ev_function_call("diagnose", "test_sync_tool", DIAGNOSE_ARGS),
                ev_completed("diagnosed"),
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
            sse(vec![
                ev_assistant_message("done", "done"),
                ev_completed("complete"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_config(|config| {
            let _ = config.features.disable(Feature::TaskCompletionReviewer);
        })
        .build(&server)
        .await?;

    let completion = test
        .submit_turn_and_capture_completion("diagnose the failure, then select validation")
        .await?;

    let requests = responses.requests();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_production_request_contains_bounded_orchestration_guidance() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

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
        .with_config(|config| {
            let _ = config.features.disable(Feature::TaskCompletionReviewer);
        })
        .build(&server)
        .await?;

    test.submit_turn_and_capture_completion("inspect the workspace")
        .await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 1);
    let developer_text = requests[0].message_input_texts("developer").join("\n\n");
    let open = "<root_orchestration_instructions>";
    let close = "</root_orchestration_instructions>";
    let guidance = developer_text
        .split_once(open)
        .and_then(|(_, suffix)| suffix.split_once(close).map(|(body, _)| body))
        .expect("normal root request should contain registered orchestration guidance");
    assert!(guidance.contains("request them together using available parallel tools"));
    assert!(guidance.contains("one `functions.exec` packet"));
    assert!(guidance.contains("split only for approvals, output bounds"));
    assert!(guidance.contains("existing wait or session path"));
    assert!(
        codex_utils_output_truncation::approx_token_count(guidance) <= 256,
        "registered orchestration guidance exceeded its per-request token budget"
    );

    Ok(())
}
