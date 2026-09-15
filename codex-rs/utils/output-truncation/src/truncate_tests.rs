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
fn truncate_tokens_less_than_placeholder_retains_bounded_omission_signal() {
    let content = "example output";

    let result = formatted_truncate_text(content, TruncationPolicy::Tokens(1));
    assert_eq!(result, "…");
    assert!(result.len() < content.len());
    assert!(approx_token_count(&result) <= 1);
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

    let result = formatted_truncate_text(content, TruncationPolicy::Tokens(5));
    assert!(approx_token_count(&result) <= 5);
    assert!(result.starts_with("this"));
    assert!(!result.contains("long output"));
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
    let content = "this is an example of a long output\n".repeat(200);
    let result = formatted_truncate_text(&content, TruncationPolicy::Tokens(100));
    assert!(approx_token_count(&result) <= 100);
    assert!(result.starts_with("Warning: truncated output (original token count:"));
    assert!(result.contains("Total output lines: 200\n"));
    assert!(result.contains("this is an example"));
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
fn formatted_truncate_text_content_items_with_policy_preserves_text_and_image_order() {
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
                text: "Warning: truncated output (original token count: 1)\nTotal output lines: 1\n\na…2 chars truncated…d".to_string(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "img:one".to_string(),
                detail: Some(DEFAULT_IMAGE_DETAIL),
            },
            FunctionCallOutputContentItem::InputText {
                text: "Warning: truncated output (original token count: 3)\nTotal output lines: 2\n\nefg…3 chars truncated…jkl".to_string(),
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
fn formatted_mixed_content_keeps_each_text_run_in_place_within_token_budget() {
    let image = FunctionCallOutputContentItem::InputImage {
        image_url: "img:one".to_string(),
        detail: Some(DEFAULT_IMAGE_DETAIL),
    };
    let encrypted = FunctionCallOutputContentItem::EncryptedContent {
        encrypted_content: "opaque".to_string(),
    };
    let items = vec![
        image.clone(),
        FunctionCallOutputContentItem::InputText {
            text: "first description ".repeat(100),
        },
        encrypted.clone(),
        FunctionCallOutputContentItem::InputText {
            text: "second description ".repeat(100),
        },
    ];
    let (output, original_tokens) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Tokens(64));
    let [
        first_image,
        FunctionCallOutputContentItem::InputText { text: first },
        preserved_encrypted,
        FunctionCallOutputContentItem::InputText { text: second },
    ] = output.as_slice()
    else {
        panic!("mixed content was reordered: {output:?}");
    };
    assert_eq!(first_image, &image);
    assert_eq!(preserved_encrypted, &encrypted);
    assert!(first.contains("first"));
    assert!(second.contains("second"));
    assert!(approx_token_count(first) + approx_token_count(second) <= 64);
    assert!(original_tokens.is_some_and(|tokens| tokens > 64));
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
            text: "a…".to_string(),
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
fn optimization_priority_coherent_packet_defaults_precede_trimming() {
    assert_eq!(DEFAULT_SUCCESS_OUTPUT_TOKENS, 4_000);
    assert_eq!(DEFAULT_FAILURE_OUTPUT_TOKENS, 10_000);
    assert_eq!(DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS, 10_000);
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
fn fork_validation_routes_receive_the_diagnostic_budget() {
    for command in [
        "just core-test-fast core_lib -E test(parser)",
        "just core-gate tool-output-recovery",
        "just core-test-lane core_lib -E test(parser)",
        "just _core-test-lane-reserved core_lib -E test(parser)",
        "just core-test-parity core_lib -E test(parser)",
        "just test-lane-main",
        "just test-lane-fast",
        "just test-lane-package codex-core",
        "just validate-crate-focused codex-core",
        "just validate-crate-full codex-core",
        "just clippy-lane",
        "just fmt-check-fast",
        "just sdk-ts-check",
        "just sdk-python-check",
        "just source-map-check",
        "just source-owners-check",
        "just --justfile codex-rs/justfile fmt-check",
        "just --justfile 'C:/repo with spaces/justfile' config-schema-check",
        "just -q -f codex-rs/justfile -- app-server-schema-check",
        "just --justfile=codex-rs/justfile fmt-check",
        "python scripts/rust_test_runner.py run-target core_lib -E test(parser)",
        "uv run pytest -q",
        "node --test",
        "npx tsc --noEmit",
        "eslint src",
        "ruff check .",
        "mypy src",
        "npm run lint",
        "pnpm typecheck",
    ] {
        let limits =
            resolve_output_limits(None, OutputOutcome::Success, Some(command), "ok", 20_000);
        assert_eq!(limits.applied_limit, 10_000, "{command}");
        let explicit = resolve_output_limits(
            Some(500),
            OutputOutcome::Success,
            Some(command),
            "ok",
            20_000,
        );
        assert_eq!(explicit.applied_limit, 500, "{command}");
    }
}

#[test]
fn just_non_validation_commands_keep_the_success_budget() {
    for command in [
        "just --dry-run test",
        "just --list test",
        "just --justfile",
        "just --justfile test",
        "echo just --justfile codex-rs/justfile test",
        "just run test",
    ] {
        assert_eq!(
            resolve_output_limits(None, OutputOutcome::Success, Some(command), "ok", 20_000)
                .applied_limit,
            DEFAULT_SUCCESS_OUTPUT_TOKENS,
            "{command}"
        );
    }
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
        DEFAULT_SUCCESS_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_projected_output_limits(
            None,
            OutputOutcome::Failure,
            OutputDiagnosticClass::Normal,
            usize::MAX,
        )
        .applied_limit,
        DEFAULT_FAILURE_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve_projected_output_limits(
            None,
            OutputOutcome::TimedOut,
            OutputDiagnosticClass::Normal,
            usize::MAX,
        )
        .applied_limit,
        DEFAULT_FAILURE_OUTPUT_TOKENS
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
    assert!(truncated.text.contains("xxxx"));
}

#[test]
fn exact_token_ceiling_includes_the_truncation_marker() {
    let truncated = truncate_text_to_token_ceiling(&"abcd ".repeat(10_000), 32);

    assert!(approx_token_count(&truncated) <= 32);
    assert!(truncated.contains("abcd"));
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
    let limits = resolve_output_limits(Some(256), OutputOutcome::Success, None, &content, 256);
    let formatted = formatted_truncate_text_with_output_limit(&content, limits);
    assert!(formatted.text.contains("HEAD_EVIDENCE"));
    assert!(formatted.text.contains("MIDDLE_EVIDENCE"));
    assert!(formatted.text.contains("TAIL_EVIDENCE"));
    assert_eq!(
        formatted
            .text
            .matches("[omitted before retained middle]")
            .count(),
        1
    );
    assert_eq!(
        formatted
            .text
            .matches("[omitted after retained middle]")
            .count(),
        1
    );
    assert!(approx_token_count(&formatted.text) <= 256);
}

#[test]
fn exact_token_ceiling_zero_returns_no_text() {
    assert_eq!(truncate_text_to_token_ceiling("content", 0), "");
}

#[test]
fn just_over_budget_retains_most_of_the_source() {
    let source = "x".repeat(4004);
    let result = truncate_text_to_token_ceiling(&source, 1000);
    assert!(approx_token_count(&result) <= 1000);
    assert!(result.bytes().filter(|byte| *byte == b'x').count() > 3900);
}

#[test]
fn small_and_unicode_outputs_remain_useful_and_bounded() {
    for source in [
        "hello ".repeat(100),
        "漢字🦀 ".repeat(100),
        ":;! ".repeat(100),
    ] {
        for limit in 1..64 {
            let result = truncate_text_to_token_ceiling(&source, limit);
            assert!(approx_token_count(&result) <= limit, "{limit}: {result}");
            if limit == 1 {
                assert_eq!(
                    result, "…",
                    "a one-token Unicode budget can only fit the omission signal"
                );
                continue;
            }
            assert!(
                result
                    .chars()
                    .any(|ch| source.contains(ch) && !ch.is_whitespace()),
                "{limit}: {result}"
            );
        }
    }
}

#[test]
fn diagnostic_output_receives_budget_without_command_metadata() {
    for diagnostic in [
        "src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.",
        "npm ERR! code ELIFECYCLE",
        "failures:\n    parser::rejects_invalid_input",
        "failures:\n\n    parser::rejects_invalid_input\n",
        "Caused by:\n    process exited unsuccessfully",
        "    thread 'main' panicked at src/main.rs:12: assertion failed",
        "    stack backtrace:\n       0: app::main",
        "thread 'tokio-runtime-worker' panicked at src/worker.rs:42: assertion failed",
        "thread 'named worker' panicked at src/worker.rs:42: assertion failed",
        "error: could not compile crate",
        "warning: unused variable",
        "src/main.c:10:2: error: unknown type name",
        "FAILED",
        "AssertionError: expected ready",
        "Segmentation fault (core dumped)",
        "project.csproj: error MSB1009: Project file does not exist.",
    ] {
        let limits = resolve_output_limits(None, OutputOutcome::Success, None, diagnostic, 20_000);
        assert_eq!(limits.applied_limit, 10_000, "{diagnostic}");
    }
}

#[test]
fn validation_help_and_version_requests_use_the_ordinary_output_budget() {
    for command in [
        "pytest --help",
        "npx tsc -h",
        "uv run pytest --version",
        "rustc --help",
        "eslint --version",
        "ruff check --help",
        "mypy --help",
        "python -m unittest --help",
        "cargo test --help",
        "just core-test-fast --help",
    ] {
        let limits = resolve_output_limits(
            None,
            OutputOutcome::Success,
            Some(command),
            "usage information",
            20_000,
        );
        assert_eq!(
            limits.applied_limit, DEFAULT_SUCCESS_OUTPUT_TOKENS,
            "{command}"
        );
    }
    // Verbosity and operands after the option terminator still run validation.
    for command in ["pytest -v", "eslint -- --help"] {
        let limits =
            resolve_output_limits(None, OutputOutcome::Success, Some(command), "ok", 20_000);
        assert_eq!(
            limits.applied_limit, DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
            "{command}"
        );
    }
}

#[test]
fn validation_launcher_chains_do_not_require_recursive_stack_space() {
    let launchers = "& npx uv run ".repeat(16_384);
    for (command, expected) in [
        (
            format!("{launchers}pytest -q"),
            DEFAULT_DIAGNOSTIC_OUTPUT_TOKENS,
        ),
        (
            format!("{launchers}echo pytest"),
            DEFAULT_SUCCESS_OUTPUT_TOKENS,
        ),
    ] {
        let limits =
            resolve_output_limits(None, OutputOutcome::Success, Some(&command), "ok", 20_000);
        assert_eq!(limits.applied_limit, expected);
    }
}

#[test]
fn reading_typescript_configuration_is_not_validation() {
    let limits = resolve_output_limits(
        None,
        OutputOutcome::Success,
        Some("Get-Content tsconfig.json"),
        "{}",
        20_000,
    );
    assert_eq!(limits.applied_limit, 4_000);
}

#[test]
fn formatted_token_policies_bound_dense_output_including_metadata() {
    for content in [
        r#"{"a":1,"b":2}"#.to_string(),
        ":;!".repeat(500),
        "漢字🦀 ".repeat(500),
    ] {
        for budget in [0, 1, 4, 16, 64, 256] {
            let output = formatted_truncate_text(&content, TruncationPolicy::Tokens(budget));
            assert!(
                approx_token_count(&output) <= budget,
                "budget {budget}: {output}"
            );
            if approx_token_count(&content) > budget {
                assert_ne!(output, content);
            } else {
                assert_eq!(output, content);
            }
            if budget > 0 {
                assert!(!output.is_empty());
            }
        }
    }
}

#[test]
fn formatted_content_items_enforce_dense_token_budget_and_preserve_nontext() {
    let image = FunctionCallOutputContentItem::InputImage {
        image_url: "data:image/png;base64,AA==".to_string(),
        detail: Some(DEFAULT_IMAGE_DETAIL),
    };
    let encrypted = FunctionCallOutputContentItem::EncryptedContent {
        encrypted_content: "opaque".to_string(),
    };
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: r#"{"a":1,"b":2}"#.to_string(),
        },
        image.clone(),
        encrypted.clone(),
    ];
    let (output, original_tokens) =
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Tokens(4));
    assert_eq!(original_tokens, Some(13));
    let [
        FunctionCallOutputContentItem::InputText { text },
        preserved_image,
        preserved_encrypted,
    ] = output.as_slice()
    else {
        panic!("expected text, image, and encrypted content")
    };
    assert!(approx_token_count(text) <= 4);
    assert!(!text.is_empty());
    assert_eq!(preserved_image, &image);
    assert_eq!(preserved_encrypted, &encrypted);
    // The same content fits a byte policy; token density must not change that contract.
    assert_eq!(
        formatted_truncate_text_content_items_with_policy(&items, TruncationPolicy::Bytes(13)),
        (items, None),
    );
}

#[test]
fn validation_mentions_in_command_arguments_do_not_raise_output_budget() {
    for command in [
        "echo \"cargo test\"",
        "echo pytest",
        "cat /tmp/pytest-results.txt",
        "git commit -m 'run npm test before merging'",
        "rg 'caused by:' README.md",
        "echo eslint",
        "cargo test-results",
        "just core-testing-guide",
    ] {
        let limits =
            resolve_output_limits(None, OutputOutcome::Success, Some(command), "ok", 20_000);
        assert_eq!(limits.applied_limit, 4_000, "{command}");
    }
    for command in [
        "cargo +stable test",
        "just test-fast -p codex-utils-output-truncation --lib",
        "python -m pytest",
        "& 'cargo' test",
        "python scripts\\rust_test_runner.py run-target core_lib",
    ] {
        assert_eq!(
            resolve_output_limits(None, OutputOutcome::Success, Some(command), "ok", 20_000)
                .applied_limit,
            10_000,
            "{command}"
        );
    }
}

#[test]
fn diagnostic_mentions_in_prose_do_not_raise_output_budget() {
    for output in [
        "Document failures: retry with a fresh checkout.",
        "The error ts prefix denotes a TypeScript diagnostic.",
        "This was caused by: a configuration change.",
        "failures:\nThis section explains failure handling.",
        "A compiler error may include error[E0308] or npm ERR!.",
        "thread 'worker' finished successfully",
        "The tokio-runtime-worker panicked at the old code in this example.",
        "error MSBexample: documentation text",
        "\u{1f642} ordinary Unicode output",
    ] {
        assert_eq!(
            resolve_output_limits(None, OutputOutcome::Success, None, output, 20_000).applied_limit,
            4_000,
            "{output}"
        );
    }
    for output in [
        "error[E0308]: mismatched types",
        "  Traceback (most recent call last):",
        "test result: FAILED. 1 failed",
        "error TS2322: Type mismatch",
    ] {
        assert_eq!(
            resolve_output_limits(None, OutputOutcome::Success, None, output, 20_000).applied_limit,
            10_000,
            "{output}"
        );
    }
}
