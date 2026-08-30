use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem as RuntimeContentItem;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_tools::ToolOutput;
use codex_tools::ToolOutputOutcome;

use super::format_runtime_response;

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
    );
    let older = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now() - Duration::from_secs(5),
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
