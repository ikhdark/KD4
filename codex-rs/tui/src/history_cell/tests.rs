//! Coverage for history-cell rendering, wrapping, and transcript behavior.

use super::*;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCall;
use crate::exec_cell::ExecCell;
use crate::legacy_core::config::Config;
use crate::legacy_core::config::ConfigBuilder;
use crate::session_state::ThreadSessionState;
use crate::wrapping::word_wrap_lines;
use codex_app_server_protocol::AskForApproval;
use codex_otel::RuntimeMetricTotals;
use codex_otel::RuntimeMetricsSummary;
use codex_protocol::ThreadId;
use codex_protocol::account::PlanType;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::parse_command::ParsedCommand;
use dirs::home_dir;
use http::StatusCode;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

use codex_app_server_protocol::CommandExecutionSource as ExecCommandSource;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::mcp::Tool;
use rmcp::model::Content;

const SMALL_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[test]
fn transcript_height_saturates_instead_of_hiding_large_cells() {
    let cell = PlainHistoryCell::new(vec![Line::from("x"); usize::from(u16::MAX) + 1]);
    assert_eq!(cell.desired_transcript_height(80), u16::MAX);
}

#[test]
fn user_history_cell_overlapping_and_spanning_elements_preserve_text() {
    let cell = UserHistoryCell {
        message: "abcdef\nghij\nαβ".into(),
        text_elements: vec![
            TextElement::new((2..10).into(), None),
            TextElement::new((0..4).into(), None),
            TextElement::new((1..3).into(), None),
            TextElement::new((12..13).into(), None), // Invalid UTF-8 endpoint.
            TextElement::new((14..16).into(), None),
        ],
        local_image_paths: vec![],
        remote_image_urls: vec![],
    };
    assert_eq!(
        render_lines(&cell.display_lines(80)),
        vec!["", "› abcdef", "  ghij", "  αβ", ""]
    );
    let lines = cell.display_lines(80);
    let styled = lines
        .iter()
        .flat_map(|line| &line.spans)
        .filter(|span| span.style.fg == Some(Color::Cyan))
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert_eq!(styled, "abcdefghiβ");
}

#[test]
fn completed_mcp_preview_is_bounded_and_transcript_preserves_all_text() {
    let mut cell = new_active_mcp_tool_call(
        "many".into(),
        McpInvocation {
            server: "s".into(),
            tool: "t".into(),
            arguments: None,
        },
        false,
    );
    let mut content: Vec<_> = (0..20).map(|i| text_block(&format!("block {i}"))).collect();
    content.push(text_block(&format!("{}\ntail-marker", "row\n".repeat(100))));
    let mut expected_raw = vec!["Called s.t()".to_string()];
    expected_raw.extend((0..20).map(|i| format!("block {i}")));
    expected_raw.extend(vec!["row".to_string(); 100]);
    expected_raw.push(String::new());
    expected_raw.push("tail-marker".to_string());
    assert!(
        cell.complete(
            Duration::ZERO,
            Ok(CallToolResult {
                content,
                is_error: None,
                structured_content: None,
                meta: None
            })
        )
        .is_none()
    );
    let preview = cell.display_lines(80);
    assert_eq!(preview.len(), TOOL_CALL_MAX_LINES + 1);
    assert!(
        preview
            .last()
            .unwrap()
            .to_string()
            .contains("output truncated")
    );
    let transcript = render_lines(&cell.transcript_lines(80)).join("\n");
    assert_eq!(transcript, expected_raw.join("\n"));
    assert_eq!(render_lines(&cell.raw_lines()), expected_raw);
    assert!(transcript.contains("block 19"));
    assert!(transcript.ends_with("tail-marker"));
    assert_eq!(
        cell.raw_lines()
            .iter()
            .filter(|line| line.to_string() == "row")
            .count(),
        100
    );
    let composite = CompositeHistoryCell::new(vec![Box::new(cell)]);
    assert!(
        render_lines(&composite.transcript_lines(80))
            .join("\n")
            .ends_with("tail-marker")
    );
}

#[test]
fn completed_mcp_preserves_malformed_content_fallback_and_media_placeholders() {
    let mut cell = new_active_mcp_tool_call(
        "media".into(),
        McpInvocation {
            server: "s".into(),
            tool: "t".into(),
            arguments: None,
        },
        false,
    );
    let malformed = json!({"type": "image"});
    assert!(
        cell.complete(
            Duration::ZERO,
            Ok(CallToolResult {
                content: vec![
                    malformed.clone(),
                    json!({"type": "audio", "data": "AAAA", "mimeType": "audio/wav"})
                ],
                is_error: Some(true),
                structured_content: None,
                meta: None,
            })
        )
        .is_none()
    );
    assert_eq!(
        render_lines(&cell.raw_lines()),
        vec!["Called s.t()", &malformed.to_string(), "<audio content>"]
    );
    assert_eq!(
        cell.display_lines(80)[0].spans[0].style.fg,
        Some(Color::Red)
    );
}

#[test]
fn process_summary_height_includes_wrapped_empty_message() {
    let composite = new_unified_exec_processes_output(vec![]);
    let cell = &composite.parts[1];
    let width = 10;
    let expected = Paragraph::new(Text::from(cell.display_lines(width)))
        .wrap(Wrap { trim: false })
        .line_count(width);
    assert!(expected > cell.display_lines(width).len());
    assert_eq!(usize::from(cell.desired_height(width)), expected);
}
async fn test_config() -> Config {
    let codex_home = std::env::temp_dir();
    ConfigBuilder::default()
        .codex_home(codex_home.clone())
        .build()
        .await
        .expect("config")
}

fn test_cwd() -> PathBuf {
    // These tests only need a stable absolute cwd; using temp_dir() avoids baking Unix- or
    // Windows-specific root semantics into the fixtures.
    std::env::temp_dir()
}

#[test]
fn streaming_agent_tail_blank_line_uses_one_viewport_row() {
    let cell = StreamingAgentTailCell::new(
        vec![
            HyperlinkLine::from("first"),
            HyperlinkLine::from(""),
            HyperlinkLine::from("second"),
        ],
        /*is_first_line*/ false,
    );

    let lines = cell.display_lines(/*width*/ 80);
    insta::assert_snapshot!(render_lines(&lines).join("\n"), @"  first

  second");
    assert_eq!(cell.desired_height(/*width*/ 80), 3);
}

fn render_lines(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn render_transcript(cell: &dyn HistoryCell) -> Vec<String> {
    render_lines(&cell.transcript_lines(u16::MAX))
}

fn assert_unstyled_lines(lines: &[Line<'static>]) {
    for line in lines {
        assert_eq!(line.style, Style::default());
        for span in &line.spans {
            assert_eq!(span.style, Style::default());
        }
    }
}

fn image_block(data: &str) -> serde_json::Value {
    serde_json::to_value(Content::image(data.to_string(), "image/png"))
        .expect("image content should serialize")
}

fn text_block(text: &str) -> serde_json::Value {
    serde_json::to_value(Content::text(text)).expect("text content should serialize")
}

fn resource_link_block(
    uri: &str,
    name: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> serde_json::Value {
    serde_json::to_value(Content::resource_link(rmcp::model::RawResource {
        uri: uri.to_string(),
        name: name.to_string(),
        title: title.map(str::to_string),
        description: description.map(str::to_string),
        mime_type: None,
        size: None,
        icons: None,
        meta: None,
    }))
    .expect("resource link content should serialize")
}

#[test]
fn raw_lines_from_source_preserves_blank_lines_but_not_final_newline() {
    for (source, expected) in [
        ("alpha\n\nbeta\n", vec!["alpha", "", "beta"]),
        ("alpha\n\n", vec!["alpha", ""]),
        ("", vec![]),
    ] {
        let lines = raw_lines_from_source(source);
        assert_eq!(render_lines(&lines), expected, "{source:?}");
        assert_unstyled_lines(&lines);
    }
}

#[test]
fn source_backed_cells_render_raw_source_without_prefix_or_style() {
    let user = new_user_prompt(
        "hello\n\nworld\n".to_string(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let assistant = AgentMarkdownCell::new(
        "- item\n\n| A | B |\n| - | - |\n| x | y |\n".to_string(),
        &test_cwd(),
    );
    let reasoning = ReasoningSummaryCell::new(
        "thinking".to_string(),
        "first thought\n\nsecond thought".to_string(),
        &test_cwd(),
        /*transcript_only*/ false,
    );
    let plan = new_proposed_plan(
        "1. Inspect\n\n```sh\ncargo test\n```".to_string(),
        &test_cwd(),
    );

    let user_lines = user.raw_lines();
    assert_eq!(
        render_lines(&user_lines),
        vec!["hello".to_string(), String::new(), "world".to_string()]
    );
    assert_unstyled_lines(&user_lines);

    let assistant_lines = assistant.raw_lines();
    assert_eq!(
        render_lines(&assistant_lines),
        vec![
            "- item".to_string(),
            String::new(),
            "| A | B |".to_string(),
            "| - | - |".to_string(),
            "| x | y |".to_string(),
        ]
    );
    assert_unstyled_lines(&assistant_lines);

    let reasoning_lines = reasoning.raw_lines();
    assert_eq!(
        render_lines(&reasoning_lines),
        vec![
            "first thought".to_string(),
            String::new(),
            "second thought".to_string(),
        ]
    );
    assert_unstyled_lines(&reasoning_lines);

    let plan_lines = plan.raw_lines();
    assert_eq!(
        render_lines(&plan_lines),
        vec![
            "1. Inspect".to_string(),
            String::new(),
            "```sh".to_string(),
            "cargo test".to_string(),
            "```".to_string(),
        ]
    );
    assert_unstyled_lines(&plan_lines);
}

#[test]
fn proposed_plan_cell_renders_markdown_table() {
    let plan = new_proposed_plan(
        "## Plan\n\n| Step | Owner |\n| --- | --- |\n| Verify | Codex |\n".to_string(),
        &test_cwd(),
    );

    let rendered = render_lines(&plan.display_lines(/*width*/ 80));

    assert!(
        rendered.iter().any(|line| line.contains('━')),
        "expected separated table in proposed plan output: {rendered:?}"
    );
    assert!(
        !rendered
            .iter()
            .any(|line| line.trim() == "| Step | Owner |"),
        "did not expect raw table header in rich proposed plan output: {rendered:?}"
    );

    let raw = render_lines(&plan.raw_lines());
    assert!(
        raw.iter().any(|line| line == "| Step | Owner |"),
        "expected raw mode to preserve table markdown source: {raw:?}"
    );
}

#[test]
fn proposed_plan_cell_preserves_wrapped_table_web_links() {
    let destination = "https://example.com/a/very/long/path/to/a/table/artifact";
    let plan = new_proposed_plan(
        format!("| Step | URL |\n| --- | --- |\n| Verify | {destination} |\n"),
        &test_cwd(),
    );

    let lines = plan.display_hyperlink_lines(/*width*/ 32);
    let linked_rows = lines
        .iter()
        .filter(|line| !line.hyperlinks.is_empty())
        .collect::<Vec<_>>();

    assert!(linked_rows.len() > 1);
    assert!(linked_rows.iter().all(|line| {
        line.hyperlinks
            .iter()
            .all(|link| link.destination == destination)
    }));
}

#[test]
fn composite_cell_preserves_child_web_links() {
    let destination = "https://chatgpt.com/codex/settings/usage";
    let cell = CompositeHistoryCell::new(vec![
        Box::new(PlainHistoryCell::new(vec![Line::from("/status")])),
        Box::new(WebHyperlinkHistoryCell::new(vec![Line::from(destination)])),
    ]);

    let lines = cell.display_hyperlink_lines(/*width*/ 80);

    assert_eq!(
        lines[2].hyperlinks,
        vec![crate::terminal_hyperlinks::TerminalHyperlink {
            columns: 0..destination.len(),
            destination: destination.to_string(),
        }]
    );
}

#[test]
fn proposed_plan_cell_unwraps_markdown_fenced_table() {
    let plan = new_proposed_plan(
        "## Plan\n\n```markdown\n| Step | Owner |\n| --- | --- |\n| Verify | Codex |\n```\n"
            .to_string(),
        &test_cwd(),
    );

    let rendered = render_lines(&plan.display_lines(/*width*/ 80));

    assert!(
        rendered.iter().any(|line| line.contains('━')),
        "expected separated table for markdown-fenced proposed plan output: {rendered:?}"
    );
    assert!(
        !rendered.iter().any(|line| line.trim() == "```markdown"),
        "did not expect markdown fence to render as code block: {rendered:?}"
    );
}

#[test]
fn structured_tool_cell_renders_raw_plain_text_without_prefix_or_style() {
    let invocation = McpInvocation {
        server: "search".into(),
        tool: "find_docs".into(),
        arguments: Some(json!({"query": "raw mode"})),
    };
    let result = CallToolResult {
        content: vec![text_block("alpha\nbeta")],
        is_error: None,
        structured_content: None,
        meta: None,
    };
    let mut cell = new_active_mcp_tool_call(
        "call-raw".to_string(),
        invocation,
        /*animations_enabled*/ false,
    );
    assert!(
        cell.complete(Duration::from_millis(1), Ok(result))
            .is_none()
    );

    let lines = cell.raw_lines();
    let rendered = render_lines(&lines);
    assert_eq!(rendered[0], "Called search.find_docs({\"query\":\"raw mode\"})");
    assert_eq!(rendered[1..], ["alpha".to_string(), "beta".to_string()]);
    assert_unstyled_lines(&lines);
}

#[test]
fn raw_mode_toggle_transcript_snapshot() {
    let mut tool_cell = new_active_mcp_tool_call(
        "call-snapshot".to_string(),
        McpInvocation {
            server: "workspace".to_string(),
            tool: "inspect".to_string(),
            arguments: Some(json!({"path": "README.md"})),
        },
        /*animations_enabled*/ false,
    );
    assert!(
        tool_cell
            .complete(
                Duration::from_millis(5),
                Ok(CallToolResult {
                    content: vec![text_block("structured output\nsecond line")],
                    is_error: None,
                    structured_content: None,
                    meta: None,
                }),
            )
            .is_none()
    );
    let cells: Vec<Box<dyn HistoryCell>> = vec![
            Box::new(new_user_prompt(
                "Please format this\nfor copying".to_string(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )),
            Box::new(AgentMarkdownCell::new(
                "- first item\n- second item\n\n| Col | Value |\n| --- | --- |\n| code | `x = 1` |\n\n```text\ncopy me\n```".to_string(),
                &test_cwd(),
            )),
            Box::new(tool_cell),
        ];

    let render = |mode| {
        cells
            .iter()
            .flat_map(|cell| cell.display_lines_for_mode(/*width*/ 40, mode))
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let rendered = format!(
        "rich before:\n{}\n\nraw on:\n{}\n\nrich after:\n{}",
        render(HistoryRenderMode::Rich),
        render(HistoryRenderMode::Raw),
        render(HistoryRenderMode::Rich)
    );

    insta::assert_snapshot!("raw_mode_toggle_transcript", rendered);
}

#[test]
fn image_generation_call_renders_saved_path() {
    let saved_path = test_path_buf("/tmp/generated-image.png").abs();
    let expected_saved_path = format!(
        "  └ Saved to: {}",
        Url::from_file_path(saved_path.as_path()).expect("test path should convert to file URL")
    );
    let cell = new_image_generation_call(
        "call-image-generation".to_string(),
        "completed",
        Some("A tiny blue square".to_string()),
        Some(saved_path),
    );

    assert_eq!(
        render_lines(&cell.display_lines(/*width*/ 80)),
        vec![
            "• Generated Image:".to_string(),
            "  └ A tiny blue square".to_string(),
            expected_saved_path,
        ],
    );
}

fn session_configured_event(model: &str) -> ThreadSessionState {
    ThreadSessionState {
        thread_id: ThreadId::new(),
        forked_from_id: None,
        fork_parent_title: None,
        thread_name: None,
        model: model.to_string(),
        model_provider_id: "test-provider".to_string(),
        service_tier: None,
        approval_policy: AskForApproval::Never,
        permission_profile: PermissionProfile::read_only(),
        active_permission_profile: None,
        cwd: test_path_buf("/tmp/project").abs(),
        runtime_workspace_roots: Vec::new(),
        instruction_source_paths: Vec::new(),
        reasoning_effort: None,
        collaboration_mode: None,
        personality: None,
        message_history: None,
        network_proxy: None,
        rollout_path: Some(PathBuf::new()),
    }
}

#[test]
fn unified_exec_interaction_cell_renders_input() {
    let cell = new_unified_exec_interaction(Some("echo hello".to_string()), "ls\npwd".to_string());
    let lines = render_transcript(&cell);
    assert_eq!(
        lines,
        vec![
            "↳ Interacted with background terminal · echo hello",
            "  └ ls",
            "    pwd",
        ],
    );
}

#[test]
fn unified_exec_interaction_cell_renders_wait() {
    let cell = new_unified_exec_interaction(/*command_display*/ None, String::new());
    let lines = render_transcript(&cell);
    assert_eq!(lines, vec!["• Waited for background terminal"]);
}

#[test]
fn final_message_separator_hides_short_worked_label_and_includes_runtime_metrics() {
    let summary = RuntimeMetricsSummary {
        tool_calls: RuntimeMetricTotals {
            count: 3,
            duration_ms: 2_450,
        },
        api_calls: RuntimeMetricTotals {
            count: 2,
            duration_ms: 1_200,
        },
        streaming_events: RuntimeMetricTotals {
            count: 6,
            duration_ms: 900,
        },
        websocket_calls: RuntimeMetricTotals {
            count: 1,
            duration_ms: 700,
        },
        websocket_events: RuntimeMetricTotals {
            count: 4,
            duration_ms: 1_200,
        },
        responses_api_overhead_ms: 650,
        responses_api_inference_time_ms: 1_940,
        responses_api_engine_iapi_ttft_ms: 410,
        responses_api_engine_service_ttft_ms: 460,
        responses_api_engine_iapi_tbt_ms: 1_180,
        responses_api_engine_service_tbt_ms: 1_240,
        turn_ttft_ms: 0,
        turn_ttfm_ms: 0,
    };
    let cell = FinalMessageSeparator::new(Some(12), Some(summary));
    let rendered = render_lines(&cell.display_lines(/*width*/ 600));

    assert_eq!(rendered.len(), 1);
    assert!(!rendered[0].contains("Worked for"));
    assert!(rendered[0].contains("Local tools: 3 calls (2.5s)"));
    assert!(rendered[0].contains("Inference: 2 calls (1.2s)"));
    assert!(rendered[0].contains("WebSocket: 1 events send (700ms)"));
    assert!(rendered[0].contains("Streams: 6 events (900ms)"));
    assert!(rendered[0].contains("4 events received (1.2s)"));
    assert!(rendered[0].contains("Responses API overhead: 650ms"));
    assert!(rendered[0].contains("Responses API inference: 1.9s"));
    assert!(rendered[0].contains("TTFT: 410ms (iapi) 460ms (service)"));
    assert!(rendered[0].contains("TBT: 1.2s (iapi) 1.2s (service)"));
}

#[test]
fn final_message_separator_includes_worked_label_after_one_minute() {
    let cell = FinalMessageSeparator::new(Some(61), /*runtime_metrics*/ None);
    let rendered = render_lines(&cell.display_lines(/*width*/ 200));

    assert_eq!(rendered.len(), 1);
    assert!(rendered[0].starts_with("─ Worked for 1m 01s ─"));
    assert_eq!(render_lines(&cell.raw_lines()), vec!["Worked for 1m 01s"]);
}

#[test]
fn ps_output_empty_snapshot() {
    let cell = new_unified_exec_processes_output(Vec::new());
    let rendered = render_lines(&cell.display_lines(/*width*/ 60)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[tokio::test]
async fn session_info_availability_nux_tooltip_snapshot() {
    let mut config = test_config().await;
    config.cwd = test_path_buf(r"C:\project").abs();
    let cell = new_session_info(
        &config,
        "gpt-5",
        &session_configured_event("gpt-5"),
        /*is_first_event*/ false,
        Some("Model just became available".to_string()),
        Some(PlanType::Free),
        /*show_fast_status*/ false,
    );

    let rendered = render_transcript(&cell).join("\n");
    insta::assert_snapshot!(rendered);
}

#[tokio::test]
async fn session_info_first_event_suppresses_tooltips_and_nux() {
    let config = test_config().await;
    let cell = new_session_info(
        &config,
        "gpt-5",
        &session_configured_event("gpt-5"),
        /*is_first_event*/ true,
        Some("Model just became available".to_string()),
        Some(PlanType::Free),
        /*show_fast_status*/ false,
    );

    let rendered = render_transcript(&cell).join("\n");
    assert!(!rendered.contains("Model just became available"));
    assert!(rendered.contains("To get started"));
}

#[tokio::test]
async fn session_info_hides_tooltips_when_disabled() {
    let mut config = test_config().await;
    config.show_tooltips = false;
    let cell = new_session_info(
        &config,
        "gpt-5",
        &session_configured_event("gpt-5"),
        /*is_first_event*/ false,
        Some("Model just became available".to_string()),
        Some(PlanType::Free),
        /*show_fast_status*/ false,
    );

    let rendered = render_transcript(&cell).join("\n");
    assert!(!rendered.contains("Model just became available"));
}

#[test]
fn ps_output_multiline_snapshot() {
    let cell = new_unified_exec_processes_output(vec![
        UnifiedExecProcessDetails {
            command_display: "echo hello\nand then some extra text".to_string(),
            recent_chunks: vec!["hello".to_string(), "done".to_string()],
        },
        UnifiedExecProcessDetails {
            command_display: "rg \"foo\" src".to_string(),
            recent_chunks: vec!["src/main.rs:12:foo".to_string()],
        },
    ]);
    let rendered = render_lines(&cell.display_lines(/*width*/ 40)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn cyber_policy_error_event_snapshot() {
    let cell = new_cyber_policy_error_event();
    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn safety_access_block_event_snapshot() {
    let cell = new_safety_access_block_event();
    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn cyber_policy_error_event_narrow_snapshot() {
    let cell = new_cyber_policy_error_event();
    let rendered = render_lines(&cell.display_lines(/*width*/ 36)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn ps_output_long_command_snapshot() {
    let cell = new_unified_exec_processes_output(vec![UnifiedExecProcessDetails {
        command_display: String::from(
            "rg \"foo\" src --glob '**/*.rs' --max-count 1000 --no-ignore --hidden --follow --glob '!target/**'",
        ),
        recent_chunks: vec!["searching...".to_string()],
    }]);
    let rendered = render_lines(&cell.display_lines(/*width*/ 36)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn ps_output_many_sessions_snapshot() {
    let cell = new_unified_exec_processes_output(
        (0..20)
            .map(|idx| UnifiedExecProcessDetails {
                command_display: format!("command {idx}"),
                recent_chunks: Vec::new(),
            })
            .collect(),
    );
    let rendered = render_lines(&cell.display_lines(/*width*/ 32)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn ps_output_chunk_leading_whitespace_snapshot() {
    let cell = new_unified_exec_processes_output(vec![UnifiedExecProcessDetails {
        command_display: "just fix".to_string(),
        recent_chunks: vec![
            "  indented first".to_string(),
            "    more indented".to_string(),
        ],
    }]);
    let rendered = render_lines(&cell.display_lines(/*width*/ 60)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn empty_mcp_output_links_docs_without_escape_sequences() {
    let cell = empty_mcp_output();
    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");
    assert!(!rendered.contains('\x1b'), "{rendered:?}");
    let destination = "https://developers.openai.com/codex/mcp";
    assert!(
        cell.display_hyperlink_lines(/*width*/ 80)
            .iter()
            .any(|line| line.hyperlinks.iter().any(|link| link.destination == destination))
    );
}

#[test]
fn ps_output_chunks_show_text_without_escape_sequences() {
    let cell = new_unified_exec_processes_output(vec![UnifiedExecProcessDetails {
        command_display: "cargo build".to_string(),
        recent_chunks: vec!["\x1b[32mCompiling\x1b[0m foo".to_string()],
    }]);
    let rendered = render_lines(&cell.display_lines(/*width*/ 60)).join("\n");
    assert!(rendered.contains("    ↳ Compiling foo"), "{rendered:?}");
    assert!(!rendered.contains("[32m"), "{rendered:?}");
}

#[test]
fn error_event_oversized_input_snapshot() {
    let cell = new_error_event(
        "Message exceeds the maximum length of 1048576 characters (1048577 provided).".to_string(),
    );
    let rendered = render_lines(&cell.display_lines(/*width*/ 120)).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn error_event_bedrock_expired_signature_snapshot() {
    let error = UnexpectedResponseError {
        retry_after: None,
        status: StatusCode::UNAUTHORIZED,
        body: "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z \
(20260614T063025Z - 5 min.)"
            .to_string(),
        user_message: Some(
            "Amazon Bedrock rejected the request because its AWS signature has expired. \
Refresh your AWS credentials and retry. If `AWS_BEARER_TOKEN_BEDROCK` is set, update or \
unset it, then restart Codex"
                .to_string(),
        ),
        url: Some("https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses".to_string()),
        cf_ray: None,
        request_id: None,
        identity_authorization_error: None,
        identity_error_code: None,
    };
    let cell = new_error_event(error.to_string());
    let rendered = render_lines(&cell.display_lines(/*width*/ 100)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn mcp_tools_output_from_statuses_renders_verbose_inventory() {
    let statuses = vec![McpServerStatus {
        name: "plugin_docs".to_string(),
        server_info: None,
        tools: HashMap::from([(
            "lookup".to_string(),
            Tool {
                description: None,
                name: "lookup".to_string(),
                title: None,
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                output_schema: None,
                annotations: None,
                icons: None,
                meta: None,
            },
        )]),
        resources: vec![Resource {
            annotations: None,
            description: None,
            mime_type: None,
            name: "docs".to_string(),
            size: None,
            title: Some("Docs".to_string()),
            uri: "file:///docs".to_string(),
            icons: None,
            meta: None,
        }],
        resource_templates: vec![ResourceTemplate {
            annotations: None,
            uri_template: "file:///docs/{id}".to_string(),
            name: "doc-template".to_string(),
            title: Some("Doc Template".to_string()),
            description: None,
            mime_type: None,
        }],
        auth_status: codex_app_server_protocol::McpAuthStatus::Unsupported,
    }];

    let compact = new_mcp_tools_output_from_statuses(&statuses, McpServerStatusDetail::ToolsAndAuthOnly);
    insta::assert_snapshot!(
        "mcp_tools_output_from_statuses_renders_status_only_servers",
        render_lines(&compact.display_lines(120)).join("\n"),
    );
    let cell = new_mcp_tools_output_from_statuses(&statuses, McpServerStatusDetail::Full);
    let rendered = render_lines(&cell.display_lines(/*width*/ 120)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn empty_agent_message_cell_transcript() {
    let cell = AgentMessageCell::new(vec![Line::default()], /*is_first_line*/ false);
    assert_eq!(cell.transcript_lines(/*width*/ 80), vec![Line::from("  ")]);
    assert_eq!(cell.desired_transcript_height(/*width*/ 80), 1);
}

#[test]
fn prefixed_wrapped_history_cell_indents_wrapped_lines() {
    let summary = Line::from(vec![
        "You ".into(),
        "approved".bold(),
        " codex to run ".into(),
        "echo something really long to ensure wrapping happens".dim(),
        " this time".bold(),
    ]);
    let cell = PrefixedWrappedHistoryCell::new(summary, "✔ ".green(), "  ");
    let rendered = render_lines(&cell.display_lines(/*width*/ 24));
    assert_eq!(
        rendered,
        vec![
            "✔ You approved codex to".to_string(),
            "  run echo something".to_string(),
            "  really long to ensure".to_string(),
            "  wrapping happens this".to_string(),
            "  time".to_string(),
        ]
    );
}

#[test]
fn prefixed_wrapped_history_cell_does_not_split_url_like_token() {
    let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890";
    let cell = PrefixedWrappedHistoryCell::new(Line::from(url_like), "✔ ".green(), "  ");
    let rendered = render_lines(&cell.display_lines(/*width*/ 24));

    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains(url_like))
            .count(),
        1,
        "expected full URL-like token in one rendered line, got: {rendered:?}"
    );
}

#[test]
fn unified_exec_interaction_cell_does_not_split_url_like_stdin_token() {
    let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890";
    let cell = UnifiedExecInteractionCell::new(Some("true".to_string()), url_like.to_string());
    let rendered = render_lines(&cell.display_lines(/*width*/ 24));

    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains(url_like))
            .count(),
        1,
        "expected full URL-like token in one rendered line, got: {rendered:?}"
    );
}

#[test]
fn prefixed_wrapped_history_cell_height_matches_wrapped_rendering() {
    let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path";
    let cell: Box<dyn HistoryCell> = Box::new(PrefixedWrappedHistoryCell::new(
        Line::from(url_like),
        "✔ ".green(),
        "  ",
    ));

    let width: u16 = 24;
    let logical_height = cell.display_lines(width).len() as u16;
    let wrapped_height = cell.desired_height(width);
    assert!(
        wrapped_height > logical_height,
        "expected wrapped height to exceed logical line count ({logical_height}), got {wrapped_height}"
    );

    let area = Rect::new(0, 0, width, wrapped_height);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    cell.render(area, &mut buf);

    let first_row = (0..area.width)
        .map(|x| {
            let symbol = buf[(x, 0)].symbol();
            if symbol.is_empty() {
                ' '
            } else {
                symbol.chars().next().unwrap_or(' ')
            }
        })
        .collect::<String>();
    assert!(
        first_row.contains("✔"),
        "expected first rendered row to keep the prefix visible, got: {first_row:?}"
    );
}

#[test]
fn unified_exec_interaction_cell_height_matches_wrapped_rendering() {
    let url_like = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path";
    let cell: Box<dyn HistoryCell> = Box::new(UnifiedExecInteractionCell::new(
        Some("true".to_string()),
        url_like.to_string(),
    ));

    let width: u16 = 24;
    let logical_height = cell.display_lines(width).len() as u16;
    let wrapped_height = cell.desired_height(width);
    assert!(
        wrapped_height > logical_height,
        "expected wrapped height to exceed logical line count ({logical_height}), got {wrapped_height}"
    );

    let area = Rect::new(0, 0, width, wrapped_height);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    cell.render(area, &mut buf);

    let first_row = (0..area.width)
        .map(|x| {
            let symbol = buf[(x, 0)].symbol();
            if symbol.is_empty() {
                ' '
            } else {
                symbol.chars().next().unwrap_or(' ')
            }
        })
        .collect::<String>();
    assert!(
        first_row.contains("Interacted with"),
        "expected first rendered row to keep the header visible, got: {first_row:?}"
    );
}

#[test]
fn web_search_history_cell_snapshot() {
    let query = "example search query with several generic words to exercise wrapping".to_string();
    let cell = new_web_search_call(
        "call-1".to_string(),
        query.clone(),
        WebSearchAction::Search {
            query: Some(query),
            queries: None,
        },
    );
    let rendered = render_lines(&cell.display_lines(/*width*/ 64)).join("\n");

    insta::assert_snapshot!(rendered);
    assert_eq!(
        render_lines(&cell.display_lines(64)),
        ["• Searched the web for example search query with several generic",
         "  words to exercise wrapping"],
    );
    insta::assert_snapshot!(
        "web_search_history_cell_transcript_snapshot",
        render_lines(&cell.transcript_lines(64)).join("\n"),
    );
}

#[test]
fn standalone_windows_update_available_history_cell_snapshot() {
    let cell =
        UpdateAvailableHistoryCell::new("9.9.9".to_string(), Some(UpdateAction::StandaloneWindows));
    let rendered = render_lines(&cell.display_lines(/*width*/ 110)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn manual_update_available_history_cell_snapshot() {
    // Package-manager installs have no automatic update action.
    let cell = UpdateAvailableHistoryCell::new("9.9.9".to_string(), /*update_action*/ None);
    let rendered = render_lines(&cell.display_lines(/*width*/ 110)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn web_search_history_cell_without_detail_snapshot() {
    let cell = new_web_search_call("call-1".to_string(), String::new(), WebSearchAction::Other);
    let rendered = render_lines(&cell.display_lines(/*width*/ 64)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn web_search_history_cell_short_query_does_not_wrap() {
    let query = "short query".to_string();
    let cell = new_web_search_call(
        "call-1".to_string(),
        query.clone(),
        WebSearchAction::Search {
            query: Some(query),
            queries: None,
        },
    );
    let rendered = render_lines(&cell.display_lines(/*width*/ 64));

    assert_eq!(
        rendered,
        vec!["• Searched the web for short query".to_string()]
    );
}

#[test]
fn active_mcp_tool_call_snapshot() {
    let invocation = McpInvocation {
        server: "search".into(),
        tool: "find_docs".into(),
        arguments: Some(json!({
            "query": "ratatui styling",
            "limit": 3,
        })),
    };

    let cell = new_active_mcp_tool_call(
        "call-1".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn mcp_inventory_loading_snapshot() {
    let cell = new_mcp_inventory_loading(/*animations_enabled*/ true);
    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn mcp_inventory_loading_without_animations_is_stable() {
    let cell = new_mcp_inventory_loading(/*animations_enabled*/ false);
    let first = render_lines(&cell.display_lines(/*width*/ 80));
    let second = render_lines(&cell.display_lines(/*width*/ 80));

    assert_eq!(first, second);
    assert_eq!(first, vec!["• Loading MCP inventory…".to_string()]);
}

#[test]
fn completed_mcp_tool_call_success_snapshot() {
    let invocation = McpInvocation {
        server: "search".into(),
        tool: "find_docs".into(),
        arguments: Some(json!({
            "query": "ratatui styling",
            "limit": 3,
        })),
    };

    let result = CallToolResult {
        content: vec![text_block("Found styling guidance in styles.md")],
        is_error: None,
        structured_content: None,
        meta: None,
    };

    let mut cell = new_active_mcp_tool_call(
        "call-2".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    assert!(
        cell.complete(Duration::from_millis(1420), Ok(result))
            .is_none()
    );

    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn completed_mcp_tool_call_finds_valid_images_among_invalid_blocks() {
    let data_url = format!("data:image/png;base64,{SMALL_PNG_BASE64}");
    for (content, has_image) in [
        (vec![text_block("Here is the image:"), image_block(SMALL_PNG_BASE64)], true),
        (vec![image_block(&data_url)], true),
        (vec![image_block("not-base64"), image_block(SMALL_PNG_BASE64)], true),
        (vec![image_block("bm90IGFuIGltYWdl"), image_block(SMALL_PNG_BASE64)], true),
        (vec![image_block("not-base64")], false),
        (vec![image_block("bm90IGFuIGltYWdl")], false),
        (vec![image_block(&format!("data:text/plain;base64,{SMALL_PNG_BASE64}"))], false),
    ] {
        let mut cell = new_active_mcp_tool_call(
            "call-image".into(),
            McpInvocation { server: "image".into(), tool: "generate".into(),
                arguments: Some(json!({"prompt": "tiny image"})) },
            true,
        );
        let extra_cell = cell.complete(Duration::from_millis(25), Ok(CallToolResult {
            content, is_error: None, structured_content: None, meta: None,
        }));
        assert_eq!(extra_cell.is_some(), has_image);
        if let Some(extra_cell) = extra_cell {
            assert_eq!(render_lines(&extra_cell.display_lines(80)), ["tool result (image output)"]);
        }
    }
}

#[test]
fn completed_mcp_tool_call_error_snapshot() {
    let invocation = McpInvocation {
        server: "search".into(),
        tool: "find_docs".into(),
        arguments: Some(json!({
            "query": "ratatui styling",
            "limit": 3,
        })),
    };

    let mut cell = new_active_mcp_tool_call(
        "call-3".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    assert!(
        cell.complete(Duration::from_secs(2), Err("network timeout".into()))
            .is_none()
    );

    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn completed_mcp_tool_call_multiple_outputs_snapshot() {
    let invocation = McpInvocation {
        server: "search".into(),
        tool: "find_docs".into(),
        arguments: Some(json!({
            "query": "ratatui styling",
            "limit": 3,
        })),
    };

    let result = CallToolResult {
        content: vec![
            text_block(
                "Found styling guidance in styles.md and additional notes in CONTRIBUTING.md.",
            ),
            resource_link_block(
                "file:///docs/styles.md",
                "styles.md",
                Some("Styles"),
                Some("Link to styles documentation"),
            ),
        ],
        is_error: None,
        structured_content: None,
        meta: None,
    };

    let mut cell = new_active_mcp_tool_call(
        "call-4".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    assert!(
        cell.complete(Duration::from_millis(640), Ok(result))
            .is_none()
    );

    let rendered = render_lines(&cell.display_lines(/*width*/ 48)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn completed_mcp_tool_call_wrapped_outputs_snapshot() {
    let invocation = McpInvocation {
        server: "metrics".into(),
        tool: "get_nearby_metric".into(),
        arguments: Some(json!({
            "query": "very_long_query_that_needs_wrapping_to_display_properly_in_the_history",
            "limit": 1,
        })),
    };

    let result = CallToolResult {
        content: vec![text_block(
            "Line one of the response, which is quite long and needs wrapping.\nLine two continues the response with more detail.",
        )],
        is_error: None,
        structured_content: None,
        meta: None,
    };

    let mut cell = new_active_mcp_tool_call(
        "call-5".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    assert!(
        cell.complete(Duration::from_millis(1280), Ok(result))
            .is_none()
    );

    let rendered = render_lines(&cell.display_lines(/*width*/ 40)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn completed_mcp_preview_bounds_rows_and_cells_without_losing_raw_output() {
    let text = (0..TOOL_CALL_MAX_LINES + 3)
        .map(|index| format!("row {index}: 界界界 e\u{301} 👩‍💻"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut cell = new_active_mcp_tool_call(
        "preview-bounds".into(),
        McpInvocation {
            server: "s".into(),
            tool: "t".into(),
            arguments: None,
        },
        false,
    );
    assert!(
        cell.complete(
            Duration::ZERO,
            Ok(CallToolResult {
                content: vec![text_block(&text)],
                is_error: None,
                structured_content: None,
                meta: None,
            })
        )
        .is_none()
    );
    let lines = cell.display_lines(24);
    assert_eq!(lines.len(), TOOL_CALL_MAX_LINES + 1);
    assert!(lines.iter().all(|line| line.width() <= 24));
    let preview = render_lines(&lines).join("\n");
    assert!(preview.contains("row 0:"));
    assert!(preview.ends_with('…'));
    assert!(render_lines(&cell.raw_lines()).join("\n").contains(&text));
}

#[test]
fn completed_mcp_tool_call_multiple_outputs_inline_snapshot() {
    let invocation = McpInvocation {
        server: "metrics".into(),
        tool: "summary".into(),
        arguments: Some(json!({
            "metric": "trace.latency",
            "window": "15m",
        })),
    };

    let result = CallToolResult {
        content: vec![
            text_block("Latency summary: p50=120ms, p95=480ms."),
            text_block("No anomalies detected."),
        ],
        is_error: None,
        structured_content: None,
        meta: None,
    };

    let mut cell = new_active_mcp_tool_call(
        "call-6".into(),
        invocation,
        /*animations_enabled*/ true,
    );
    assert!(
        cell.complete(Duration::from_millis(320), Ok(result))
            .is_none()
    );

    let rendered = render_lines(&cell.display_lines(/*width*/ 120)).join("\n");

    insta::assert_snapshot!(rendered);
}

#[test]
fn session_header_includes_reasoning_and_respects_fast_status() {
    for show_fast_status in [false, true] {
        let cell = SessionHeaderHistoryCell::new(
            "gpt-4o".to_string(), Some(ReasoningEffortConfig::High),
            show_fast_status, std::env::temp_dir(), "test",
        );
        let lines = render_lines(&cell.display_lines(80));
        let model_line = lines.iter().find(|line| line.contains("model:")).expect("model line");
        assert!(model_line.contains("gpt-4o high"), "{model_line}");
        assert_eq!(model_line.contains("gpt-4o high   fast"), show_fast_status, "{model_line}");
        assert_eq!(model_line.contains("fast"), show_fast_status, "{model_line}");
        assert!(model_line.contains("/model to change"), "{model_line}");
    }
}

#[test]
fn session_header_indicates_full_access_mode() {
    let cell = SessionHeaderHistoryCell::new(
        "gpt-5".to_string(),
        /*reasoning_effort*/ None,
        /*show_fast_status*/ false,
        test_path_buf("/tmp/project").abs().to_path_buf(),
        "test",
    )
    .with_full_access_mode(/*full_access_mode*/ true);

    assert!(cell.raw_lines().iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.content == "permissions: Full Access")
    }));
    let rendered = render_lines(&cell.display_lines(/*width*/ 80));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("permissions: Full Access")),
        "{rendered:?}"
    );

    // Control: a restricted session must not show a permissions row at all.
    let restricted = SessionHeaderHistoryCell::new(
        "gpt-5".to_string(),
        /*reasoning_effort*/ None,
        /*show_fast_status*/ false,
        test_path_buf("/tmp/project").abs().to_path_buf(),
        "test",
    );
    for lines in [
        restricted.raw_lines(),
        restricted.display_lines(/*width*/ 80),
    ] {
        let rendered = render_lines(&lines);
        assert!(
            !rendered.iter().any(|line| line.contains("permissions")),
            "{rendered:?}"
        );
    }
}

#[test]
fn full_access_mode_requires_unrestricted_profile_and_no_approvals() {
    for (profile, expected) in [
        (PermissionProfile::Disabled, true),
        (PermissionProfile::Managed {
            network: NetworkSandboxPolicy::Enabled,
            file_system: ManagedFileSystemPermissions::Unrestricted,
        }, true),
        (PermissionProfile::External { network: NetworkSandboxPolicy::Enabled }, false),
        (PermissionProfile::read_only(), false),
    ] {
        assert_eq!(has_full_access_permissions(AskForApproval::Never, &profile), expected);
        assert!(!has_full_access_permissions(AskForApproval::OnRequest, &profile));
    }
}

#[test]
fn session_header_directory_center_truncates() {
    let mut dir = home_dir().expect("home directory");
    for part in ["hello", "the", "fox", "is", "very", "fast"] {
        dir.push(part);
    }

    let formatted = SessionHeaderHistoryCell::format_directory_inner(&dir, Some(24));
    let sep = std::path::MAIN_SEPARATOR;
    let expected = format!("~{sep}hello{sep}the{sep}…{sep}very{sep}fast");
    assert_eq!(formatted, expected);
}

#[test]
fn session_header_directory_front_truncates_long_segment() {
    let mut dir = home_dir().expect("home directory");
    dir.push("supercalifragilisticexpialidocious");

    let formatted = SessionHeaderHistoryCell::format_directory_inner(&dir, Some(18));
    let sep = std::path::MAIN_SEPARATOR;
    let expected = format!("~{sep}…cexpialidocious");
    assert_eq!(formatted, expected);
}

#[test]
fn mixed_search_and_read_call_lists_each_read_on_its_own_row() {
    // Reads share one row only when a call parses to nothing but reads. A call that also
    // searches keeps one row per parsed command.
    let call_id = "c1".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![
                ParsedCommand::Search {
                    query: Some("shimmer_spans".into()),
                    path: None,
                    cmd: "rg shimmer_spans".into(),
                },
                ParsedCommand::Read {
                    name: "shimmer.rs".into(),
                    cmd: "cat shimmer.rs".into(),
                    path: "shimmer.rs".into(),
                },
                ParsedCommand::Read {
                    name: "status_indicator_widget.rs".into(),
                    cmd: "cat status_indicator_widget.rs".into(),
                    path: "status_indicator_widget.rs".into(),
                },
            ],
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));

    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines).join("\n");
    assert_eq!(
        rendered,
        "• Explored\n  └ Search shimmer_spans\n    Read shimmer.rs\n    Read status_indicator_widget.rs"
    );
}

#[test]
fn coalesces_reads_across_multiple_calls() {
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: "c1".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![ParsedCommand::Search {
                query: Some("shimmer_spans".into()),
                path: None,
                cmd: "rg shimmer_spans".into(),
            }],
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    // Call 1: Search only
    cell.complete_call("c1", CommandOutput::default(), Duration::from_millis(1));
    // Call 2: Read A
    cell = cell
        .with_added_call(
            "c2".into(),
            vec!["bash".into(), "-lc".into(), "echo".into()],
            vec![ParsedCommand::Read {
                name: "shimmer.rs".into(),
                cmd: "cat shimmer.rs".into(),
                path: "shimmer.rs".into(),
            }],
            ExecCommandSource::Agent,
        )
        .unwrap();
    cell.complete_call("c2", CommandOutput::default(), Duration::from_millis(1));
    // Call 3: Read B
    cell = cell
        .with_added_call(
            "c3".into(),
            vec!["bash".into(), "-lc".into(), "echo".into()],
            vec![ParsedCommand::Read {
                name: "status_indicator_widget.rs".into(),
                cmd: "cat status_indicator_widget.rs".into(),
                path: "status_indicator_widget.rs".into(),
            }],
            ExecCommandSource::Agent,
        )
        .unwrap();
    cell.complete_call("c3", CommandOutput::default(), Duration::from_millis(1));

    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn coalesced_reads_dedupe_names() {
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: "c1".to_string(),
            command: vec!["bash".into(), "-lc".into(), "echo".into()],
            parsed: vec![
                ParsedCommand::Read {
                    name: "auth.rs".into(),
                    cmd: "cat auth.rs".into(),
                    path: "auth.rs".into(),
                },
                ParsedCommand::Read {
                    name: "auth.rs".into(),
                    cmd: "cat auth.rs".into(),
                    path: "auth.rs".into(),
                },
                ParsedCommand::Read {
                    name: "shimmer.rs".into(),
                    cmd: "cat shimmer.rs".into(),
                    path: "shimmer.rs".into(),
                },
            ],
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call("c1", CommandOutput::default(), Duration::from_millis(1));
    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn multiline_command_wraps_later_lines_under_pipe_gutter() {
    // Create a completed exec cell with a multiline command
    let cmd = "set -o pipefail\ncargo test -p codex-tui --quiet".to_string();
    let call_id = "c1".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    // Mark call complete so it renders as "Ran"
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));

    // Small width so the second command line itself wraps onto another gutter row.
    let width: u16 = 28;
    let lines = cell.display_lines(width);
    let rendered = render_lines(&lines).join("\n");
    assert_eq!(
        rendered,
        "• Ran set -o pipefail\n  │ cargo test -p codex-tui\n  │ --quiet\n  └ (no output)"
    );
}

#[test]
fn single_line_command_compact_when_fits() {
    let call_id = "c1".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["echo".into(), "ok".into()],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));
    // Wide enough that it fits inline
    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn single_line_command_wraps_under_pipe_gutter() {
    let call_id = "c1".to_string();
    let long = "a_very_long_token_without_spaces_to_force_wrapping".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), long],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));
    let lines = cell.display_lines(/*width*/ 24);
    let rendered = render_lines(&lines).join("\n");
    assert_eq!(
        rendered,
        "• Ran a_very_long_token_\n  │ without_spaces_to_\n  │ force_wrapping\n  └ (no output)"
    );
}

#[test]
fn multiline_command_without_wrap_puts_second_line_under_pipe_gutter() {
    let call_id = "c1".to_string();
    let cmd = "echo one\necho two".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));
    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines).join("\n");
    assert_eq!(rendered, "• Ran echo one\n  │ echo two\n  └ (no output)");
}

#[test]
fn multiline_command_both_lines_wrap_with_correct_prefixes() {
    let call_id = "c1".to_string();
    let cmd =
        "first_token_is_long_enough_to_wrap\nsecond_token_is_also_long_enough_to_wrap".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), cmd],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    cell.complete_call(&call_id, CommandOutput::default(), Duration::from_millis(1));
    let lines = cell.display_lines(/*width*/ 28);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn stderr_tail_more_than_five_lines_snapshot() {
    // Build an exec cell with a non-zero exit and 10 lines on stderr to exercise
    // the head/tail rendering and gutter prefixes.
    let call_id = "c_err".to_string();
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), "seq 1 10 1>&2 && false".into()],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );
    let stderr: String = (1..=10)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    cell.complete_call(
        &call_id,
        CommandOutput::new(1, stderr, String::new()),
        Duration::from_millis(1),
    );

    let rendered = cell
        .display_lines(/*width*/ 80)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn ran_cell_multiline_with_stderr_snapshot() {
    // Build an exec cell that completes (so it renders as "Ran") with a
    // command long enough that it must render on its own line under the
    // header, and include a couple of stderr lines to verify the output
    // block prefixes and wrapping.
    let call_id = "c_wrap_err".to_string();
    let long_cmd =
        "echo this_is_a_very_long_single_token_that_will_wrap_across_the_available_width";
    let mut cell = ExecCell::new(
        ExecCall {
            call_id: call_id.clone(),
            command: vec!["bash".into(), "-lc".into(), long_cmd.to_string()],
            parsed: Vec::new(),
            output: None,
            source: ExecCommandSource::Agent,
            start_time: Some(Instant::now()),
            duration: None,
        },
        /*animations_enabled*/ true,
    );

    let stderr = "error: first line on stderr\nerror: second line on stderr".to_string();
    cell.complete_call(
        &call_id,
        CommandOutput::new(1, stderr, String::new()),
        Duration::from_millis(5),
    );

    // Narrow width to force the command to render under the header line.
    let width: u16 = 28;
    let rendered = cell
        .display_lines(width)
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered);
}
#[test]
fn user_history_cell_wraps_and_prefixes_each_line_snapshot() {
    let msg = "_count_r\x1b[13;2:3uows";
    let cell = UserHistoryCell {
        message: msg.to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
    };

    // Small width to force wrapping more clearly. Effective wrap width is width-2 due to the ▌ prefix and trailing space.
    let width: u16 = 12;
    let lines = cell.display_lines(width);
    let rendered = render_lines(&lines).join("\n");

    assert_eq!(render_lines(&cell.raw_lines()), ["_count_rows"]);
    insta::assert_snapshot!(rendered);
}

#[test]
fn user_history_cell_renders_remote_image_urls() {
    let cell = UserHistoryCell {
        message: "describe these".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: vec!["https://example.com/example.png".to_string()],
    };

    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    assert!(rendered.contains("[Image #1]"));
    assert!(rendered.contains("describe these"));
    insta::assert_snapshot!(rendered);
}

#[test]
fn user_history_cell_summarizes_inline_data_urls() {
    let cell = UserHistoryCell {
        message: "describe inline image".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: vec!["data:image/png;base64,aGVsbG8=".to_string()],
    };

    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    assert!(rendered.contains("[Image #1]"));
    assert!(rendered.contains("describe inline image"));
    assert!(!rendered.contains("data:"), "{rendered:?}");
    assert!(!rendered.contains("aGVsbG8="), "{rendered:?}");
}

#[test]
fn user_history_cell_numbers_multiple_remote_images() {
    let cell = UserHistoryCell {
        message: "describe both".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: vec![
            "https://example.com/one.png".to_string(),
            "https://example.com/two.png".to_string(),
        ],
    };

    let rendered = render_lines(&cell.display_lines(/*width*/ 80)).join("\n");

    assert!(rendered.contains("[Image #1]"));
    assert!(rendered.contains("[Image #2]"));
    insta::assert_snapshot!(rendered);
}

#[test]
fn user_history_cell_height_matches_rendered_lines_with_remote_images() {
    let cell = UserHistoryCell {
        message: "line one\nline two".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: vec![
            "https://example.com/one.png".to_string(),
            "https://example.com/two.png".to_string(),
        ],
    };

    let width = 80;
    let rendered_len: u16 = cell
        .display_lines(width)
        .len()
        .try_into()
        .unwrap_or(u16::MAX);
    assert_eq!(cell.desired_height(width), rendered_len);
    assert_eq!(cell.desired_transcript_height(width), rendered_len);
}

#[test]
fn user_history_cell_trims_trailing_blank_message_lines() {
    let cell = UserHistoryCell {
        message: "line one\n\n   \n\t \n".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: vec!["https://example.com/one.png".to_string()],
    };

    let rendered = render_lines(&cell.display_lines(/*width*/ 80));
    let trailing_blank_count = rendered
        .iter()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count();
    assert_eq!(trailing_blank_count, 1);
    assert!(rendered.iter().any(|line| line.contains("line one")));
}

#[test]
fn user_history_cell_trims_trailing_blank_message_lines_with_text_elements() {
    let message = "tokenized\n\n\n".to_string();
    let cell = UserHistoryCell {
        message,
        text_elements: vec![TextElement::new(
            (0..8).into(),
            Some("tokenized".to_string()),
        )],
        local_image_paths: Vec::new(),
        remote_image_urls: vec!["https://example.com/one.png".to_string()],
    };

    let rendered = render_lines(&cell.display_lines(/*width*/ 80));
    let trailing_blank_count = rendered
        .iter()
        .rev()
        .take_while(|line| line.trim().is_empty())
        .count();
    assert_eq!(trailing_blank_count, 1);
    assert!(rendered.iter().any(|line| line.contains("tokenized")));
}

#[test]
fn render_uses_wrapping_for_long_url_like_line() {
    let url = "https://example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path/that/keeps/going/for/testing/purposes-only-and-does/not/need/to/resolve/index.html?session_id=abc123def456ghi789jkl012mno345pqr678stu901vwx234yz";
    let cell: Box<dyn HistoryCell> = Box::new(UserHistoryCell {
        message: url.to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
    });

    let width: u16 = 52;
    let height = cell.desired_height(width);
    assert!(
        height > 1,
        "expected wrapped height for long URL, got {height}"
    );

    let area = Rect::new(0, 0, width, height);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    cell.render(area, &mut buf);

    let rendered = (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| {
                    let symbol = buf[(x, y)].symbol();
                    if symbol.is_empty() {
                        ' '
                    } else {
                        symbol.chars().next().unwrap_or(' ')
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let rendered_blob = rendered.join("\n");
    let complete_url = rendered.iter().map(|row| row.trim()).collect::<String>();
    // The unbreakable URL can move to the row after the prompt glyph, so
    // trimming each rendered row also removes the space after that glyph.
    assert_eq!(
        complete_url.strip_prefix('›').map(str::trim_start),
        Some(url),
        "expected the entire URL, including its tail, after wrapping:\n{rendered_blob}"
    );

    let non_empty_rows = rendered.iter().filter(|row| !row.trim().is_empty()).count() as u16;
    assert!(
        non_empty_rows > 3,
        "expected long URL to span multiple visible rows, got:\n{rendered_blob}"
    );
}

#[test]
fn plan_update_with_note_and_wrapping_snapshot() {
    // Long explanation forces wrapping; include long step text to verify step wrapping and alignment.
    let update = UpdatePlanArgs {
            explanation: Some(
                "I’ll update Grafana call error handling by adding retries and clearer messages when the backend is unreachable."
                    .to_string(),
            ),
            plan: vec![
                PlanItemArg {
                    step: "Investigate existing error paths and logging around HTTP timeouts".into(),
                    status: StepStatus::Completed,
                },
                PlanItemArg {
                    step: "Harden Grafana client error handling with retry/backoff and user‑friendly messages".into(),
                    status: StepStatus::InProgress,
                },
                PlanItemArg {
                    step: "Add tests for transient failure scenarios and surfacing to the UI".into(),
                    status: StepStatus::Pending,
                },
            ],
        };

    let cell = new_plan_update(update);
    // Narrow width to force wrapping for both the note and steps
    let lines = cell.display_lines(/*width*/ 32);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn plan_update_without_note_snapshot() {
    let update = UpdatePlanArgs {
        explanation: None,
        plan: vec![
            PlanItemArg {
                step: "Define error taxonomy".into(),
                status: StepStatus::InProgress,
            },
            PlanItemArg {
                step: "Implement mapping to user messages".into(),
                status: StepStatus::Pending,
            },
        ],
    };

    let cell = new_plan_update(update);
    let lines = cell.display_lines(/*width*/ 40);
    let rendered = render_lines(&lines).join("\n");
    insta::assert_snapshot!(rendered);
}

#[test]
fn plan_update_does_not_split_url_like_tokens_in_note_or_step() {
    let note_url = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890";
    let step_url = "example.test/api/v1/projects/beta-team/releases/2026-02-17/builds/0987654321/artifacts/reports/performance";
    let update = UpdatePlanArgs {
        explanation: Some(format!(
            "Investigate failures under {note_url} immediately."
        )),
        plan: vec![PlanItemArg {
            step: format!("Validate callbacks under {step_url} before rollout."),
            status: StepStatus::InProgress,
        }],
    };

    let cell = new_plan_update(update);
    let rendered = render_lines(&cell.display_lines(/*width*/ 30));

    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains(note_url))
            .count(),
        1,
        "expected full note URL-like token in one rendered line, got: {rendered:?}"
    );
    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains(step_url))
            .count(),
        1,
        "expected full step URL-like token in one rendered line, got: {rendered:?}"
    );
}

#[test]
fn reasoning_summary_block() {
    for (source, expected) in [
        ("**High level reasoning**\n\nDetailed reasoning goes here.", "• Detailed reasoning goes here."),
        ("**High level plan**\n\nWe should fix the bug next.", "• We should fix the bug next."),
    ] {
    let cell = new_reasoning_summary_block(
        vec![source.to_string()],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    assert_eq!(rendered_display, vec![expected]);

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec![expected]);
    }
}

#[test]
fn reasoning_summary_height_matches_wrapped_rendering_for_url_like_content() {
    let summary = "example.test/api/v1/projects/alpha-team/releases/2026-02-17/builds/1234567890/artifacts/reports/performance/summary/detail/with/a/very/long/path/that/keeps/going";
    let cell: Box<dyn HistoryCell> = Box::new(ReasoningSummaryCell::new(
        "High level reasoning".to_string(),
        summary.to_string(),
        &test_cwd(),
        /*transcript_only*/ false,
    ));
    let width: u16 = 24;

    let logical_height = cell.display_lines(width).len() as u16;
    let wrapped_height = cell.desired_height(width);
    let expected_wrapped_height = Paragraph::new(Text::from(cell.display_lines(width)))
        .wrap(Wrap { trim: false })
        .line_count(width) as u16;
    assert_eq!(wrapped_height, expected_wrapped_height);
    assert!(
        wrapped_height > logical_height,
        "expected wrapped height to exceed logical line count ({logical_height}), got {wrapped_height}"
    );

    let wrapped_transcript_height = cell.desired_transcript_height(width);
    assert_eq!(wrapped_transcript_height, wrapped_height);

    let area = Rect::new(0, 0, width, wrapped_height);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    cell.render(area, &mut buf);

    let first_row = (0..area.width)
        .map(|x| {
            let symbol = buf[(x, 0)].symbol();
            if symbol.is_empty() {
                ' '
            } else {
                symbol.chars().next().unwrap_or(' ')
            }
        })
        .collect::<String>();
    assert!(
        first_row.contains("•"),
        "expected first rendered row to keep summary bullet visible, got: {first_row:?}"
    );
}

#[test]
fn reasoning_summary_block_renders_headerless_text() {
    let cell = new_reasoning_summary_block(
        vec!["Detailed reasoning goes here.".to_string()],
        &test_cwd(),
    );

    let rendered = render_transcript(cell.as_ref());
    assert_eq!(rendered, vec!["• Detailed reasoning goes here."]);
}

#[test]
fn reasoning_summary_block_falls_back_when_header_is_missing() {
    let cell = new_reasoning_summary_block(
        vec!["**High level reasoning without closing".to_string()],
        &test_cwd(),
    );

    let rendered = render_transcript(cell.as_ref());
    assert_eq!(rendered, vec!["• **High level reasoning without closing"]);
}

#[test]
fn reasoning_summary_block_falls_back_when_summary_is_missing() {
    let cell = new_reasoning_summary_block(
        vec!["**High level reasoning without closing**".to_string()],
        &test_cwd(),
    );

    let rendered = render_transcript(cell.as_ref());
    assert_eq!(rendered, vec!["• High level reasoning without closing"]);

    let cell = new_reasoning_summary_block(
        vec!["**High level reasoning without closing**\n\n  ".to_string()],
        &test_cwd(),
    );

    let rendered = render_transcript(cell.as_ref());
    assert_eq!(rendered, vec!["• High level reasoning without closing"]);
}

#[test]
fn reasoning_summary_block_hides_empty_html_comment_parts() {
    let cell = new_reasoning_summary_block(
        vec![
            "**Checking the first thing**\n\n<!-- -->".to_string(),
            "**Checking the second thing**\n\n<!-- -->".to_string(),
        ],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    insta::assert_snapshot!(rendered_display.join("\n"), @"");

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, Vec::<String>::new());
}

#[test]
fn reasoning_summary_block_preserves_bold_content_after_empty_html_comment_part() {
    let cell = new_reasoning_summary_block(
        vec![
            "**Status**\n\n<!-- -->".to_string(),
            "**Important conclusion**".to_string(),
            "<!-- -->".to_string(),
        ],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    insta::assert_snapshot!(rendered_display.join("\n"), @"• Important conclusion");

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec!["• Important conclusion"]);

    let cell = new_reasoning_summary_block(
        vec![
            "**Status**\n\n<!-- -->".to_string(),
            "**Result:** keep **this**".to_string(),
        ],
        &test_cwd(),
    );

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec!["• Result: keep this"]);
}

#[test]
fn reasoning_summary_block_strips_header_after_leading_empty_part() {
    let cell = new_reasoning_summary_block(
        vec![
            "**Status**\n\n<!-- -->".to_string(),
            "**Checking tests**\n\nTests passed".to_string(),
        ],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    insta::assert_snapshot!(rendered_display.join("\n"), @"• Tests passed");

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec!["• Tests passed"]);
}

#[test]
fn reasoning_summary_block_drops_empty_part_after_real_content() {
    let cell = new_reasoning_summary_block(
        vec![
            "**Plan**\n\ndone".to_string(),
            "**Checking tests**\n\n<!-- -->".to_string(),
        ],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    insta::assert_snapshot!(rendered_display.join("\n"), @"• done");

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec!["• done"]);
}

#[test]
fn reasoning_summary_block_preserves_literal_html_comment() {
    let cell = new_reasoning_summary_block(
        vec!["**Plan**\n\nUse `<!-- -->` in JSX.".to_string()],
        &test_cwd(),
    );

    let rendered_display = render_lines(&cell.display_lines(/*width*/ 80));
    insta::assert_snapshot!(rendered_display.join("\n"), @"• Use <!-- --> in JSX.");

    let rendered_transcript = render_transcript(cell.as_ref());
    assert_eq!(rendered_transcript, vec!["• Use <!-- --> in JSX."]);
}

#[test]
fn deprecation_notice_renders_summary_with_details() {
    let cell = new_deprecation_notice(
        "Feature flag `foo`".to_string(),
        Some("Use flag `bar` instead.".to_string()),
    );
    let lines = cell.display_lines(/*width*/ 80);
    let rendered = render_lines(&lines);
    assert_eq!(
        rendered,
        vec![
            "⚠ Feature flag `foo`".to_string(),
            "Use flag `bar` instead.".to_string(),
        ]
    );
}

#[test]
fn agent_markdown_cell_renders_source_at_different_widths() {
    let source =
        "A long agent message that should wrap differently when the terminal width changes.\n";
    let cell = AgentMarkdownCell::new(source.to_string(), &test_cwd());

    let lines_80 = render_lines(&cell.display_lines(/*width*/ 80));
    assert!(
        lines_80.first().is_some_and(|line| line.starts_with("• ")),
        "first line should start with bullet prefix: {:?}",
        lines_80[0]
    );

    let lines_32 = render_lines(&cell.display_lines(/*width*/ 32));
    assert!(
        lines_32.len() > lines_80.len(),
        "narrower width should produce more wrapped lines: {lines_32:?}",
    );
    for lines in [&lines_80, &lines_32] {
        let text = lines.iter().map(|line| line.chars().skip(2).collect::<String>()).collect::<Vec<_>>().join(" ");
        assert_eq!(text.split_whitespace().collect::<Vec<_>>(), source.split_whitespace().collect::<Vec<_>>());
    }
}

#[test]
fn agent_markdown_cell_does_not_split_words_after_inline_markdown() {
    let source = "This paragraph is intentionally long so you can inspect soft wrapping behavior while also checking inline formatting like **bold text**, *italic text*, ***bold italic text***, `inline code`, ~~strikethrough~~, a [link to example.com](https://example.com), and a literal path like [README.md](/Users/felipe.coury/code/codex.fcoury-worktrees/README.md) without introducing manual line breaks.\n";
    let cell = AgentMarkdownCell::new(source.to_string(), &test_cwd());

    let lines = render_lines(&cell.display_lines(/*width*/ 190));
    assert!(
        lines[0].ends_with("inline code,"),
        "expected wrapping to stop before 'strikethrough': {lines:?}",
    );
    assert!(
        lines[1].starts_with("  strikethrough,"),
        "expected the next line to resume with the full word: {lines:?}",
    );
}

#[test]
fn streamed_agent_list_paragraph_preserves_item_indent_when_wrapped() {
    let cell = AgentMessageCell::new(
        vec![
            Line::from("1. Correctness issue: server tool-search completions are rejected."),
            Line::default(),
            Line::from(
                "   In next_prompt_suggestion.rs, ToolSearchCall records its call id, but a paired output is ignored and suppresses suggestions.",
            ),
        ],
        /*is_first_line*/ true,
    );

    let lines = render_lines(&cell.display_lines(/*width*/ 64));
    assert!(
        lines
            .iter()
            .filter(|line| line.contains("paired output") || line.contains("suggestions."))
            .all(|line| line.starts_with("     ")),
        "expected all wrapped paragraph rows to retain the assistant gutter and list indent: {lines:?}",
    );
    insta::assert_snapshot!(lines.join("\n"));
}

#[test]
fn agent_markdown_cell_narrow_width_shows_prefix_only() {
    let source = "narrow width coverage\n";
    let cell = AgentMarkdownCell::new(source.to_string(), &test_cwd());

    let lines = render_lines(&cell.display_lines(/*width*/ 2));
    assert_eq!(lines, vec!["• ".to_string()]);
}

#[test]
fn wrapped_and_prefixed_cells_handle_tiny_widths() {
    let user_cell = UserHistoryCell {
        message: "tiny width coverage for wrapped user history".to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
    };
    let agent_message_cell = AgentMessageCell::new(
        vec!["tiny width agent line".into()],
        /*is_first_line*/ true,
    );
    let reasoning_cell = ReasoningSummaryCell::new(
        "Plan".to_string(),
        "Reasoning summary content for tiny widths.".to_string(),
        &test_cwd(),
        /*transcript_only*/ false,
    );
    let agent_markdown_cell =
        AgentMarkdownCell::new("tiny width agent markdown line\n".to_string(), &test_cwd());

    for width in 1..=4 {
        assert!(
            !user_cell.display_lines(width).is_empty(),
            "user cell should render at width {width}",
        );
        assert!(
            !agent_message_cell.display_lines(width).is_empty(),
            "agent message cell should render at width {width}",
        );
        assert!(
            !reasoning_cell.display_lines(width).is_empty(),
            "reasoning cell should render at width {width}",
        );
        assert!(
            !agent_markdown_cell.display_lines(width).is_empty(),
            "agent markdown cell should render at width {width}",
        );
    }
}

#[test]
fn render_clears_area_when_cell_content_shrinks() {
    let area = Rect::new(0, 0, 40, 6);
    let mut buf = Buffer::empty(area);

    let first: Box<dyn HistoryCell> = Box::new(PlainHistoryCell::new(vec![
        Line::from("STALE ROW 1"),
        Line::from("STALE ROW 2"),
        Line::from("STALE ROW 3"),
        Line::from("STALE ROW 4"),
    ]));
    first.render(area, &mut buf);

    let second: Box<dyn HistoryCell> = Box::new(PlainHistoryCell::new(vec![Line::from("fresh")]));
    second.render(area, &mut buf);

    let mut rendered_rows: Vec<String> = Vec::new();
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buf.cell((x, y)).expect("cell should exist").symbol());
        }
        rendered_rows.push(row);
    }

    assert_eq!(rendered_rows[0], format!("fresh{}", " ".repeat(35)));
    assert_eq!(rendered_rows[1..], vec![" ".repeat(40); 5]);
}

#[test]
fn agent_markdown_cell_survives_insert_history_rewrap() {
    let source = "\
  Canary rollout remained at limited traffic longer than planned because p95
  latency briefly regressed during cold-cache periods.
  Regional expansion succeeded with stable error rates, though internal
  analytics lagged temporarily.
  ";
    let cell = AgentMarkdownCell::new(source.to_string(), &test_cwd());
    let width: u16 = 80;
    let lines = cell.display_lines(width);

    // Simulate what insert_history_lines does: word_wrap_lines with
    // the terminal width and no indent.
    let rewrapped = word_wrap_lines(&lines, width as usize);
    let before = render_lines(&lines);
    let after = render_lines(&rewrapped);
    assert_eq!(
        before, after,
        "word_wrap_lines should not alter lines that already fit within width"
    );
}
