use crate::DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS;
use crate::DEFAULT_FAILURE_OUTPUT_TOKENS;
use crate::DEFAULT_SUCCESS_OUTPUT_TOKENS;
use crate::OutputDiagnosticClass;
use crate::OutputLimitResolution;
use crate::OutputOutcome;
use crate::TruncationPolicy;
use crate::approx_token_count;
use crate::approx_tokens_from_byte_count_i64;
use crate::formatted_truncate_text;
use crate::formatted_truncate_text_content_items_with_policy;
use crate::formatted_truncate_text_with_output_limit;
use crate::resolve_output_limits;
use crate::resolve_projected_output_limits;
use crate::truncate_function_output_items_with_policy;
use crate::truncate_text;
use crate::truncate_text_to_token_ceiling;
use crate::truncate_text_with_output_limit;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use pretty_assertions::assert_eq;

#[test]
fn truncate_bytes_less_than_placeholder_returns_placeholder() {
    let content = "example output";

    assert_eq!(
        "Warning: truncated output (original token count: 4)\nTotal output lines: 1\n\n…13 chars truncated…t",
        formatted_truncate_text(content, TruncationPolicy::Bytes(1)),
    );
}

#[test]
fn truncate_tokens_less_than_placeholder_returns_placeholder() {
    let content = "example output";

    assert_eq!(
        "Warning: truncated output (original token count: 4)\nTotal output lines: 1\n\n…4 tokens truncated…",
        formatted_truncate_text(content, TruncationPolicy::Tokens(1)),
    );
}

#[test]
fn truncate_tokens_under_limit_returns_original() {
    let content = "example output";

    assert_eq!(
        content,
        formatted_truncate_text(content, TruncationPolicy::Tokens(10)),
    );
}

#[test]
fn truncate_bytes_under_limit_returns_original() {
    let content = "example output";

    assert_eq!(
        content,
        formatted_truncate_text(content, TruncationPolicy::Bytes(20)),
    );
}

#[test]
fn truncate_tokens_over_limit_returns_truncated() {
    let content = "this is an example of a long output that should be truncated";

    assert_eq!(
        "Warning: truncated output (original token count: 17)\nTotal output lines: 1\n\n…15 tokens truncated…",
        formatted_truncate_text(content, TruncationPolicy::Tokens(5)),
    );
}

#[test]
fn truncate_bytes_over_limit_returns_truncated() {
    let content = "this is an example of a long output that should be truncated";

    assert_eq!(
        "Warning: truncated output (original token count: 17)\nTotal output lines: 1\n\nthis is an exam…30 chars truncated…ld be truncated",
        formatted_truncate_text(content, TruncationPolicy::Bytes(30)),
    );
}

#[test]
fn truncate_bytes_reports_original_line_count_when_truncated() {
    let content =
        "this is an example of a long output that should be truncated\nalso some other line";

    assert_eq!(
        "Warning: truncated output (original token count: 22)\nTotal output lines: 2\n\nthis is an exam…51 chars truncated…some other line",
        formatted_truncate_text(content, TruncationPolicy::Bytes(30)),
    );
}

#[test]
fn truncate_tokens_reports_original_line_count_when_truncated() {
    let content =
        "this is an example of a long output that should be truncated\nalso some other line";

    assert_eq!(
        "Warning: truncated output (original token count: 22)\nTotal output lines: 2\n\nthis …18 tokens truncated… line",
        formatted_truncate_text(content, TruncationPolicy::Tokens(10)),
    );
}

#[test]
fn truncate_middle_bytes_handles_utf8_content() {
    let s = "😀😀😀😀😀😀😀😀😀😀\nsecond line with text\n";
    let out = truncate_text(s, TruncationPolicy::Bytes(20));
    assert_eq!(out, "😀😀…21 chars truncated…with text\n");
}

#[test]
fn truncates_across_multiple_under_limit_texts_and_reports_omitted() {
    let chunk = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho sigma tau upsilon phi chi psi omega.\n";
    let chunk_tokens = approx_token_count(chunk);
    assert!(chunk_tokens > 0, "chunk must consume tokens");
    let limit = chunk_tokens * 3;
    let t1 = chunk.to_string();
    let t2 = chunk.to_string();
    let t3 = chunk.repeat(10);
    let t4 = chunk.to_string();
    let t5 = chunk.to_string();

    let items = vec![
        FunctionCallOutputContentItem::InputText { text: t1.clone() },
        FunctionCallOutputContentItem::InputText { text: t2.clone() },
        FunctionCallOutputContentItem::InputImage {
            image_url: "img:mid".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        },
        FunctionCallOutputContentItem::InputText { text: t3 },
        FunctionCallOutputContentItem::InputText { text: t4 },
        FunctionCallOutputContentItem::InputText { text: t5 },
    ];

    let output =
        truncate_function_output_items_with_policy(&items, TruncationPolicy::Tokens(limit));

    assert_eq!(output.len(), 5);

    let first_text = match &output[0] {
        FunctionCallOutputContentItem::InputText { text } => text,
        other => panic!("unexpected first item: {other:?}"),
    };
    assert_eq!(first_text, &t1);

    let second_text = match &output[1] {
        FunctionCallOutputContentItem::InputText { text } => text,
        other => panic!("unexpected second item: {other:?}"),
    };
    assert_eq!(second_text, &t2);

    assert_eq!(
        output[2],
        FunctionCallOutputContentItem::InputImage {
            image_url: "img:mid".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        }
    );

    let fourth_text = match &output[3] {
        FunctionCallOutputContentItem::InputText { text } => text,
        other => panic!("unexpected fourth item: {other:?}"),
    };
    assert!(
        fourth_text.contains("tokens truncated"),
        "expected marker in truncated snippet: {fourth_text}"
    );

    let summary_text = match &output[4] {
        FunctionCallOutputContentItem::InputText { text } => text,
        other => panic!("unexpected summary item: {other:?}"),
    };
    assert!(summary_text.contains("omitted 2 text items"));
}

#[test]
fn formatted_truncate_text_content_items_with_policy_returns_original_under_limit() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "alpha".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: String::new(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "beta".to_string(),
        },
    ];

    let (output, original_token_count) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Bytes(32));

    assert_eq!(output, items);
    assert_eq!(original_token_count, None);
}

#[test]
fn formatted_truncate_text_content_items_with_policy_preserves_empty_leading_text_behavior() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: String::new(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "abc".to_string(),
        },
    ];

    let (output, original_token_count) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Bytes(0));

    assert_eq!(
        output,
        vec![FunctionCallOutputContentItem::InputText {
            text: "Warning: truncated output (original token count: 1)\nTotal output lines: 1\n\n…3 chars truncated…".to_string(),
        }]
    );
    assert_eq!(original_token_count, Some(1));
}

#[test]
fn formatted_truncate_text_content_items_with_policy_merges_text_and_appends_images() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "abcd".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: "img:one".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        },
        FunctionCallOutputContentItem::InputText {
            text: "efgh".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "ijkl".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: "img:two".to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        },
    ];

    let (output, original_token_count) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Bytes(8));

    assert_eq!(
        output,
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "Warning: truncated output (original token count: 4)\nTotal output lines: 3\n\nabcd…6 chars truncated…ijkl".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "img:one".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "img:two".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
        ]
    );
    assert_eq!(original_token_count, Some(4));
}

#[test]
fn formatted_truncate_text_content_items_with_policy_preserves_encrypted_content() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "abcdefgh".to_string(),
        },
        FunctionCallOutputContentItem::EncryptedContent {
            encrypted_content: "enc_opaque".to_string(),
        },
    ];

    let (output, original_token_count) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Bytes(2));

    assert_eq!(
        output,
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "Warning: truncated output (original token count: 2)\nTotal output lines: 1\n\na…6 chars truncated…h".to_string(),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_opaque".to_string(),
            },
        ]
    );
    assert_eq!(original_token_count, Some(2));
}

#[test]
fn truncate_function_output_items_with_policy_preserves_encrypted_content() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "abcdefgh".to_string(),
        },
        FunctionCallOutputContentItem::EncryptedContent {
            encrypted_content: "enc_opaque".to_string(),
        },
    ];

    let output = truncate_function_output_items_with_policy(&items, TruncationPolicy::Bytes(2));

    assert_eq!(
        output,
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "a…6 chars truncated…h".to_string(),
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_opaque".to_string(),
            },
        ]
    );
}

#[test]
fn formatted_truncate_text_content_items_with_policy_merges_all_text_for_token_budget() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "abcdefgh".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "ijklmnop".to_string(),
        },
    ];

    let (output, original_token_count) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Tokens(2));

    assert_eq!(
        output,
        vec![FunctionCallOutputContentItem::InputText {
            text: "Warning: truncated output (original token count: 5)\nTotal output lines: 2\n\n…5 tokens truncated…".to_string(),
        }]
    );
    assert_eq!(original_token_count, Some(5));
}

#[test]
fn byte_count_conversion_clamps_non_positive_values() {
    assert_eq!(approx_tokens_from_byte_count_i64(/*bytes*/ -1), 0);
    assert_eq!(approx_tokens_from_byte_count_i64(/*bytes*/ 0), 0);
    assert_eq!(approx_tokens_from_byte_count_i64(/*bytes*/ 5), 2);
}

#[test]
fn adaptive_output_limits_resolve_defaults_override_and_hard_ceiling() {
    assert_eq!(
        resolve_output_limits(None, OutputOutcome::Success, Some("echo ok"), "ok", 20_000),
        OutputLimitResolution {
            requested_limit: None,
            default_limit: DEFAULT_SUCCESS_OUTPUT_TOKENS,
            hard_limit: 20_000,
            applied_limit: DEFAULT_SUCCESS_OUTPUT_TOKENS,
        }
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Failure,
            Some("custom-command"),
            "failed",
            20_000,
        ),
        OutputLimitResolution {
            requested_limit: None,
            default_limit: DEFAULT_FAILURE_OUTPUT_TOKENS,
            hard_limit: 20_000,
            applied_limit: DEFAULT_FAILURE_OUTPUT_TOKENS,
        }
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::from_exit_status(None, /*timed_out*/ true),
            Some("custom-command"),
            "still running",
            20_000,
        ),
        OutputLimitResolution {
            requested_limit: None,
            default_limit: DEFAULT_FAILURE_OUTPUT_TOKENS,
            hard_limit: 20_000,
            applied_limit: DEFAULT_FAILURE_OUTPUT_TOKENS,
        }
    );
    assert_eq!(
        resolve_output_limits(
            None,
            OutputOutcome::Success,
            Some("cargo nextest run -p codex-core"),
            "tests passed",
            20_000,
        ),
        OutputLimitResolution {
            requested_limit: None,
            default_limit: DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
            hard_limit: 20_000,
            applied_limit: DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
        }
    );
    assert_eq!(
        resolve_output_limits(
            Some(12_000),
            OutputOutcome::Failure,
            Some("cargo test"),
            "test result: FAILED",
            9_000,
        ),
        OutputLimitResolution {
            requested_limit: Some(12_000),
            default_limit: DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
            hard_limit: 9_000,
            applied_limit: 9_000,
        }
    );
}

#[test]
fn typed_projection_limits_use_exact_ceilings_and_requested_minimum() {
    assert_eq!(
        resolve_projected_output_limits(
            None,
            OutputOutcome::Success,
            OutputDiagnosticClass::Normal,
            usize::MAX,
        )
        .applied_limit,
        1_000
    );
    assert_eq!(
        resolve_projected_output_limits(
            None,
            OutputOutcome::Failure,
            OutputDiagnosticClass::Normal,
            usize::MAX,
        )
        .applied_limit,
        2_000
    );
    assert_eq!(
        resolve_projected_output_limits(
            None,
            OutputOutcome::TimedOut,
            OutputDiagnosticClass::Normal,
            usize::MAX,
        )
        .applied_limit,
        2_000
    );
    assert_eq!(
        resolve_projected_output_limits(
            Some(250),
            OutputOutcome::Failure,
            OutputDiagnosticClass::HighSignal,
            usize::MAX,
        )
        .applied_limit,
        250
    );
    assert_eq!(
        resolve_projected_output_limits(
            Some(20_000),
            OutputOutcome::Failure,
            OutputDiagnosticClass::HighSignal,
            4_000,
        )
        .applied_limit,
        4_000
    );
}

#[test]
fn output_limit_truncation_reports_whether_text_was_reduced() {
    let limits = resolve_output_limits(Some(5), OutputOutcome::Success, Some("echo ok"), "ok", 20);

    let retained = truncate_text_with_output_limit("short", limits);
    assert_eq!(retained.text, "short");
    assert!(!retained.was_truncated);

    let truncated = truncate_text_with_output_limit(&"x".repeat(400), limits);
    assert!(truncated.was_truncated);
    assert_ne!(truncated.text, "x".repeat(400));
}

#[test]
fn formatted_output_respects_the_complete_projection_ceiling() {
    let limits =
        resolve_output_limits(Some(32), OutputOutcome::Success, Some("echo ok"), "ok", 100);

    let truncated = formatted_truncate_text_with_output_limit(&"x".repeat(4_000), limits);

    assert!(truncated.was_truncated);
    assert!(approx_token_count(&truncated.text) <= limits.applied_limit);
}

#[test]
fn exact_token_ceiling_includes_the_truncation_marker() {
    let truncated = truncate_text_to_token_ceiling(&"abcd ".repeat(10_000), 32);

    assert!(approx_token_count(&truncated) <= 32);
}

#[test]
fn token_ceiling_retains_head_middle_and_tail_evidence() {
    let content = format!(
        "HEAD_EVIDENCE\n{}MIDDLE_EVIDENCE\n{}TAIL_EVIDENCE",
        "before\n".repeat(1_000),
        "afterx\n".repeat(1_000)
    );

    let truncated = truncate_text_to_token_ceiling(&content, 256);

    assert!(truncated.contains("HEAD_EVIDENCE"));
    assert!(truncated.contains("MIDDLE_EVIDENCE"));
    assert!(truncated.contains("TAIL_EVIDENCE"));
    assert!(approx_token_count(&truncated) <= 256);
}

#[test]
fn exact_token_ceiling_zero_returns_no_text() {
    assert_eq!(truncate_text_to_token_ceiling("content", 0), "");
}
