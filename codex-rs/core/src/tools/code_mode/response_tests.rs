use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem as RuntimeContentItem;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_tools::ToolOutput;

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
fn runtime_response_sampling_identity_excludes_wall_time() {
    let response = || RuntimeResponse::Result {
        cell_id: CellId::new("cell-1".to_string()),
        content_items: vec![RuntimeContentItem::InputText {
            text: "same result".to_string(),
        }],
        error_text: None,
    };
    let recent = format_runtime_response(response(), None, usize::MAX, true, Instant::now());
    let older = format_runtime_response(
        response(),
        None,
        usize::MAX,
        true,
        Instant::now() - Duration::from_secs(5),
    );

    assert_ne!(recent.body, older.body);
    assert_eq!(
        recent.sampling_request_signal(),
        older.sampling_request_signal(),
    );
}
