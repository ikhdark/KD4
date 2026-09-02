use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem as RuntimeContentItem;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;

use super::CodeModeNestedResultEvidence;
use super::FAILED_CELL_ERROR_TRUNCATION_MARKER;
use super::MAX_FAILED_CELL_ERROR_BYTES;
use super::failed_code_mode_cell_item;
use super::format_runtime_response;
use super::response_needs_retained_nested_results;

fn nested_result_evidence(output: &str) -> CodeModeNestedResultEvidence {
    CodeModeNestedResultEvidence {
        ordinal: 0,
        call_id: "exec-cell-1-call-1".to_string(),
        parent_call_id: Some("outer-exec-call".to_string()),
        parent_cell_id: "cell-1".to_string(),
        runtime_tool_call_id: "call-1".to_string(),
        tool_name: "exec_command".to_string(),
        output: output.to_string(),
        output_truncated: false,
    }
}

#[test]
fn failed_runtime_response_builds_a_linkable_cell_item() {
    let item = failed_code_mode_cell_item(
        "exec-call-3",
        &RuntimeResponse::Result {
            cell_id: CellId::new("cell-7".to_string()),
            content_items: Vec::new(),
            error_text: Some("TypeError at line 4".to_string()),
        },
        Duration::from_millis(12),
    )
    .expect("failed result should emit a cell item");

    assert_eq!(item.id, "code-mode-cell:cell-7");
    assert_eq!(item.namespace.as_deref(), Some("codex.internal"));
    assert_eq!(item.tool, "code_mode_cell");
    assert_eq!(item.arguments["call_id"], "exec-call-3");
    assert_eq!(item.arguments["cell_id"], "cell-7");
    assert_eq!(item.success, Some(false));
    assert_eq!(item.error.as_deref(), Some("TypeError at line 4"));
}

#[test]
fn failed_cell_error_is_bounded_without_splitting_utf8() {
    let oversized_error = format!(
        "{}{}",
        "é".repeat(MAX_FAILED_CELL_ERROR_BYTES),
        "TAIL_MUST_NOT_SURVIVE"
    );
    let item = failed_code_mode_cell_item(
        "exec-call-3",
        &RuntimeResponse::Result {
            cell_id: CellId::new("cell-7".to_string()),
            content_items: Vec::new(),
            error_text: Some(oversized_error),
        },
        Duration::from_millis(12),
    )
    .expect("failed result should emit a cell item");
    let error = item.error.expect("failed cell error");

    assert!(error.len() <= MAX_FAILED_CELL_ERROR_BYTES);
    assert!(error.ends_with(FAILED_CELL_ERROR_TRUNCATION_MARKER));
    assert!(!error.contains("TAIL_MUST_NOT_SURVIVE"));
}

#[test]
fn runtime_response_paths_preserve_status_success_and_output_limits() {
    let cell_id = || CellId::new("cell-1".to_string());
    let content_items = || {
        vec![RuntimeContentItem::InputText {
            text: "x".repeat(400),
        }]
    };
    let cases = vec![
        (
            RuntimeResponse::Yielded {
                cell_id: cell_id(),
                content_items: content_items(),
            },
            None,
            "Script running with cell ID cell-1",
        ),
        (
            RuntimeResponse::Terminated {
                cell_id: cell_id(),
                content_items: content_items(),
            },
            Some(false),
            "Script terminated",
        ),
        (
            RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: content_items(),
                error_text: None,
            },
            Some(true),
            "Script completed",
        ),
        (
            RuntimeResponse::Result {
                cell_id: cell_id(),
                content_items: content_items(),
                error_text: Some("boom".to_string()),
            },
            Some(false),
            "Script failed",
        ),
    ];

    for (response, expected_success, expected_status) in cases {
        let output = format_runtime_response(
            response,
            Some(20),
            5,
            /*original_image_detail_supported*/ true,
            Instant::now(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(output.success, expected_success);
        assert!(matches!(
            output.body.first(),
            Some(FunctionCallOutputContentItem::InputText { text })
                if text.starts_with(expected_status)
        ));
        assert!(output.body.iter().any(|item| matches!(
            item,
            FunctionCallOutputContentItem::InputText { text }
                if text.contains("Warning: truncated output")
        )));
    }
}

#[test]
fn yielded_runtime_response_is_resumable_not_timed_out() {
    let output = format_runtime_response(
        RuntimeResponse::Yielded {
            cell_id: CellId::new("cell-live".to_string()),
            content_items: Vec::new(),
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
    );

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Yielded);
    assert_eq!(output.success, None);
}

#[test]
fn terminated_runtime_response_emits_failure_sampling_evidence() {
    let output = format_runtime_response(
        RuntimeResponse::Terminated {
            cell_id: CellId::new("cell-terminated".to_string()),
            content_items: Vec::new(),
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
    );

    assert_eq!(output.outcome_for_logging(), ToolOutputOutcome::Failure);
    assert!(!output.success_for_logging());
    assert!(
        output
            .sampling_request_signal()
            .is_some_and(|signal| signal.to_string().contains("failure_signature"))
    );
}

#[test]
fn runtime_response_sampling_identity_excludes_wall_time() {
    let response = || RuntimeResponse::Result {
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "same result".to_string(),
        }],
        error_text: None,
    };
    let recent = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        Vec::new(),
    );
    let older = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now() - Duration::from_secs(5),
        Vec::new(),
        Vec::new(),
    );

    assert_ne!(recent.body, older.body);
    assert_eq!(
        recent.sampling_request_signal(),
        older.sampling_request_signal(),
    );
}

#[test]
fn post_tool_feedback_survives_code_mode_projection() {
    let output = format_runtime_response(
        RuntimeResponse::Result {
            cell_id: CellId::new("cell-feedback".to_string()),
            content_items: Vec::new(),
            error_text: None,
        },
        None,
        usize::MAX,
        true,
        Instant::now(),
        vec![FunctionCallOutputContentItem::InputText {
            text: "hook feedback".to_string(),
        }],
        Vec::new(),
    );

    assert!(output.body.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::InputText { text } if text == "hook feedback"
    )));
    assert!(
        output
            .sampling_request_signal()
            .is_some_and(|signal| signal.to_string().contains("hook feedback"))
    );
}

#[test]
fn failed_script_keeps_successful_nested_result_and_linkage_visible() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "before failure".to_string(),
        }],
        error_text: Some("boom at line 7".to_string()),
    };
    assert!(response_needs_retained_nested_results(&response));

    let output = format_runtime_response(
        response,
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        vec![nested_result_evidence("COMMAND_SENTINEL")],
    )
    .into_text();

    assert!(output.contains("Nested tool result:"));
    assert!(output.contains("COMMAND_SENTINEL"));
    assert!(output.contains("\"parent_call_id\":\"outer-exec-call\""));
    assert!(output.contains("\"parent_cell_id\":\"cell-1\""));
    assert!(output.contains("\"runtime_tool_call_id\":\"call-1\""));
    assert!(output.contains("Script error:\nboom at line 7"));
}

#[test]
fn empty_successful_script_projects_retained_nested_result() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("cell-1".to_string()),
        content_items: Vec::new(),
        error_text: None,
    };
    assert!(response_needs_retained_nested_results(&response));

    let output = format_runtime_response(
        response,
        None,
        usize::MAX,
        true,
        Instant::now(),
        Vec::new(),
        vec![nested_result_evidence("NO_TEXT_SENTINEL")],
    )
    .into_text();

    assert!(output.contains("NO_TEXT_SENTINEL"));
}

#[test]
fn successful_script_output_suppresses_duplicate_retained_result_projection() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "already projected".to_string(),
        }],
        error_text: None,
    };

    assert!(!response_needs_retained_nested_results(&response));
}
