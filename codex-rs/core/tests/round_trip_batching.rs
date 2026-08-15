#![allow(clippy::expect_used)]

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
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
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;

const EMPTY_TEST_TOOL_ARGS: &str = "{}";

fn function_call_output_ids(input: &[Value]) -> Vec<&str> {
    input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .collect()
}

async fn turn_complete(test: &core_test_support::test_codex::TestCodex) -> TurnCompleteEvent {
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(event) => Some(event.clone()),
        _ => None,
    })
    .await
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
                ev_function_call("edit", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("edit"),
            ]),
            sse(vec![
                ev_response_created("validate-test"),
                ev_function_call("validate-test", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("validate-test"),
            ]),
            sse(vec![
                ev_response_created("validate-format"),
                ev_function_call("validate-format", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("validate-format"),
            ]),
            sse(vec![
                ev_response_created("git-diff"),
                ev_function_call("git-diff", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
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
        .build(&baseline_server)
        .await?;

    baseline_test
        .submit_turn("make the edit, then validate it and inspect the Git diff")
        .await?;
    let baseline_completion = turn_complete(&baseline_test).await;

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("edit"),
                ev_function_call("edit", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("edit"),
            ]),
            sse(vec![
                ev_response_created("post-edit"),
                ev_function_call("validate-test", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_function_call("validate-format", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_function_call("git-diff", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
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
        .build(&server)
        .await?;

    test.submit_turn("make the edit, then validate it and inspect the Git diff")
        .await?;
    let completion = turn_complete(&test).await;

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
                ev_function_call("failing-check", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("failed"),
            ]),
            sse(vec![
                ev_response_created("2"),
                ev_function_call("diagnose", "test_sync_tool", EMPTY_TEST_TOOL_ARGS),
                ev_completed("diagnosed"),
            ]),
            sse(vec![
                ev_response_created("3"),
                ev_function_call(
                    "targeted-validation",
                    "test_sync_tool",
                    EMPTY_TEST_TOOL_ARGS,
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
        .build(&server)
        .await?;

    test.submit_turn("diagnose the failure, then select validation")
        .await?;
    let completion = turn_complete(&test).await;

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

#[test]
fn orchestrator_prompt_preserves_only_substantive_model_boundaries() {
    let prompt = include_str!("../templates/agents/orchestrator.md");

    for required_guidance in [
        "request them together",
        "default to available parallel tool",
        "validation commands and read-only Git inspection",
        "default Clippy and dead-code validation to the changed packages",
        "covers exactly the same packages, targets, features, toolchain",
        "Start independent non-Cargo checks alongside Rust validation",
        "Publish remains the final validation barrier",
        "only possible decision is to wait again",
        "Keep a model boundary when the previous result determines",
    ] {
        assert!(
            prompt.contains(required_guidance),
            "missing orchestration guidance: {required_guidance}"
        );
    }
}
