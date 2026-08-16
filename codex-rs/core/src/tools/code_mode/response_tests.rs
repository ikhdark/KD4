use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem as RuntimeContentItem;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;

use super::AggregatedNestedFailure;
use super::CellCompletionFeedback;
use super::format_runtime_response;
use codex_tools::ToolFailureClass;
use codex_tools::ToolFailureDiagnostic;

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
            Some(true),
            "Script running with cell ID cell-1",
        ),
        (
            RuntimeResponse::Terminated {
                cell_id: cell_id(),
                content_items: content_items(),
            },
            Some(true),
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
            &[],
            CellCompletionFeedback::default(),
            5,
            /*original_image_detail_supported*/ true,
            Instant::now(),
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
fn nested_failures_are_aggregated_after_truncation() {
    let output = format_runtime_response(
        RuntimeResponse::Result {
            cell_id: CellId::new("cell-failure".to_string()),
            content_items: vec![RuntimeContentItem::InputText {
                text: "x".repeat(400),
            }],
            error_text: None,
        },
        Some(20),
        &[],
        CellCompletionFeedback {
            batching_feedback: None,
            failures: vec![AggregatedNestedFailure {
                diagnostic: ToolFailureDiagnostic::model_visible(
                    ToolFailureClass::Test,
                    "command.test.same_failure",
                    "focused test failed",
                )
                .with_owner_hint("tests::focused"),
                occurrences: 3,
            }],
            omitted_failure_count: 2,
        },
        5,
        true,
        Instant::now(),
    );

    let text = output
        .body
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Nested tool failure summary:"));
    assert!(text.contains("\"total_occurrences\":5"));
    assert!(text.contains("command.test.same_failure"));
}
