use std::sync::Arc;
use std::time::Duration;

use super::CellId;
use super::FallbackCodeModeSessionProvider;
use super::InProcessCodeModeSession;
use super::InProcessCodeModeSessionProvider;
use super::RuntimeResponse;
use super::WaitOutcome;
use super::WaitRequest;
use super::runtime_request;
use crate::CodeModeNestedToolCall;
use crate::CodeModeSessionDelegate;
use crate::CodeModeSessionProvider;
use crate::CodeModeToolKind;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::NoopCodeModeSessionDelegate;
use crate::NotificationFuture;
use crate::ProcessOwnedCodeModeSessionProvider;
use crate::ToolDefinition;
use crate::ToolInvocationFuture;
use crate::runtime::MAX_SESSION_STORED_VALUE_BYTES;
use codex_protocol::ToolName;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

/// Answers every nested tool call with its own name and input so tests can
/// prove which tool a call form reached.
struct EchoDelegate;

impl CodeModeSessionDelegate for EchoDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            Ok(serde_json::json!({
                "tool": invocation.tool_name.to_string(),
                "input": invocation.input,
            }))
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn exec_command_definition() -> ToolDefinition {
    ToolDefinition {
        name: "exec_command".to_string(),
        tool_name: ToolName::plain("exec_command"),
        description: "run a command".to_string(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }
}

fn result_text(response: &RuntimeResponse) -> String {
    let RuntimeResponse::Result {
        content_items,
        error_text: None,
        ..
    } = response
    else {
        panic!("expected a completed cell, got {response:?}");
    };
    content_items
        .iter()
        .map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => text.clone(),
            other => panic!("unexpected content item {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call_1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
    }
}

fn cell_id(value: &str) -> CellId {
    CellId::new(value.to_string())
}

#[tokio::test]
async fn fallback_provider_uses_in_process_session_when_host_is_missing() {
    let provider = FallbackCodeModeSessionProvider::new(
        Arc::new(ProcessOwnedCodeModeSessionProvider::with_host_program(
            "codex-code-mode-host-does-not-exist".into(),
        )),
        Arc::new(InProcessCodeModeSessionProvider),
    );

    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("missing process host should fall back to an in-process session");
    let response = session
        .execute(execute_request("text('fallback-ready');"))
        .await
        .expect("fallback execution should start")
        .initial_response()
        .await
        .expect("fallback execution should finish");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "fallback-ready".to_string(),
            }],
            error_text: None,
        }
    );
}

#[test]
fn yield_time_does_not_extend_the_default_nested_tool_timeout() {
    let request = runtime_request(ExecuteRequest {
        yield_time_ms: Some(120_000),
        ..execute_request("text('done');")
    });

    assert_eq!(request.default_tool_timeout_ms, 60_000);
}

async fn execute(service: &InProcessCodeModeSession, request: ExecuteRequest) -> RuntimeResponse {
    service
        .execute(request)
        .await
        .unwrap()
        .initial_response()
        .await
        .unwrap()
}

#[tokio::test]
async fn synchronous_exit_returns_successfully() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"text("before"); exit(); text("after");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "before".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn compact_tool_discovery_resolves_one_exact_description() {
    let service = InProcessCodeModeSession::new();
    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![ToolDefinition {
                name: "sample-tool".to_string(),
                tool_name: ToolName::plain("sample-tool"),
                description: "exact schema description".to_string(),
                kind: CodeModeToolKind::Function,
                input_schema: None,
                output_schema: None,
            }],
            source: r#"text(JSON.stringify({ names: ALL_TOOL_NAMES, resolved: resolve_tool("sample_tool"), missing: resolve_tool("missing") === undefined }));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: r#"{"names":["sample_tool"],"resolved":{"name":"sample_tool","description":"exact schema description"},"missing":true}"#.to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn stored_values_are_shared_between_cells_but_not_sessions() {
    let first_session = InProcessCodeModeSession::new();
    let second_session = InProcessCodeModeSession::new();

    let write_response = execute(
        &first_session,
        ExecuteRequest {
            source: r#"store("key", "visible");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let same_session = execute(
        &first_session,
        ExecuteRequest {
            source: r#"text(String(load("key")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let other_session = execute(
        &second_session,
        ExecuteRequest {
            source: r#"text(String(load("key")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        write_response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: None,
        }
    );
    assert_eq!(
        same_session,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "visible".to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(
        other_session,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "undefined".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn oversized_store_rejects_all_writes_from_the_cell() {
    let session = InProcessCodeModeSession::new();
    let response = execute(
        &session,
        ExecuteRequest {
            source: format!(
                r#"store("partial", "must-not-commit"); store("oversized", "x".repeat({}));"#,
                MAX_SESSION_STORED_VALUE_BYTES + 1
            ),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let RuntimeResponse::Result { error_text, .. } = response else {
        panic!("oversized store should complete with an error");
    };
    assert!(
        error_text
            .as_deref()
            .is_some_and(|error| error.contains("code mode session storage exceeds its limit"))
    );

    let read_response = execute(
        &session,
        ExecuteRequest {
            source: r#"text(String(load("partial")));"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    assert_eq!(
        read_response,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "undefined".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn shutdown_interrupts_cpu_bound_cells() {
    let service = InProcessCodeModeSession::new();

    let cell = service
        .execute(ExecuteRequest {
            source: "while (true) {}".to_string(),
            ..execute_request("")
        })
        .await
        .unwrap();
    assert_eq!(
        cell.initial_response().await.unwrap(),
        RuntimeResponse::Yielded {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
        }
    );

    tokio::time::timeout(Duration::from_secs(1), service.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn start_cell_rejects_new_cell_after_shutdown_begins() {
    let service = InProcessCodeModeSession::new();
    service.shutdown().await.unwrap();

    let error = service
        .execute(execute_request("text('late');"))
        .await
        .err()
        .unwrap();

    assert_eq!(error, "code mode session is shutting down".to_string());
}

#[tokio::test]
async fn console_shim_forwards_to_text_instead_of_v8_console() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"console.log("alias", 1, { ok: true });
console.error("bad");
text(String(typeof console.info === "function"));"#
                .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: r#"alias 1 {"ok":true}"#.to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "bad".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "true".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn bare_exec_aliases_forward_to_the_exec_command_tool() {
    let service = InProcessCodeModeSession::with_delegate(Arc::new(EchoDelegate));

    let response = execute(
        &service,
        ExecuteRequest {
            enabled_tools: vec![exec_command_definition()],
            source: r#"const viaExec = await exec({ cmd: "echo hi" });
const viaBare = await exec_command("echo again");
const viaShell = await shell({ cmd: "pwd" });
text(JSON.stringify([viaExec, viaBare, viaShell]));"#
                .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let parsed: serde_json::Value =
        serde_json::from_str(&result_text(&response)).expect("alias results are JSON");
    assert_eq!(
        parsed,
        serde_json::json!([
            { "tool": "exec_command", "input": { "cmd": "echo hi" } },
            { "tool": "exec_command", "input": "echo again" },
            { "tool": "exec_command", "input": { "cmd": "pwd" } },
        ])
    );
}

#[tokio::test]
async fn exec_alias_without_exec_command_names_the_canonical_call_form() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"await exec("echo hi");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    let RuntimeResponse::Result {
        error_text: Some(error_text),
        ..
    } = response
    else {
        panic!("expected a failed cell, got {response:?}");
    };
    assert!(error_text.contains("no `exec_command` nested tool is enabled"));
    assert!(error_text.contains("call `await tools.<name>(...)`"));
    assert!(error_text.contains("(no nested tools are enabled)"));
}

#[tokio::test]
async fn completion_budget_holds_a_cell_through_output_until_it_finishes() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"text("first");
await new Promise((resolve) => setTimeout(resolve, 200));
text("second");"#
                .to_string(),
            yield_time_ms: Some(10_000),
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "first".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "second".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn date_locale_string_formats_with_icu_data() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const value = new Date("2025-01-02T03:04:05Z")
  .toLocaleString("fr-FR", {
    weekday: "long",
    month: "long",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
    timeZone: "UTC",
  });
text(value);
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn intl_date_time_format_formats_with_icu_data() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const formatter = new Intl.DateTimeFormat("fr-FR", {
  weekday: "long",
  month: "long",
  day: "numeric",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
  timeZone: "UTC",
});
text(formatter.format(new Date("2025-01-02T03:04:05Z")));
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "jeudi 2 janvier \u{e0} 03:04:05".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn bounded_parallel_notify_returns_delivery_promise() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
const notification = notify("ping");
const returnsExpectedTypes = [
  text("first") === undefined,
  image("data:image/png;base64,AAA") === undefined,
  notification instanceof Promise,
];
await notification;
text(JSON.stringify(returnsExpectedTypes));
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "first".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(crate::DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "[true,true,true]".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_accepts_raw_mcp_image_block_with_original_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  type: "image",
  data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
  mimeType: "image/png",
  _meta: { "codex/imageDetail": "original" },
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::Original),
                }],
                error_text: None,
            }
        );
}

#[tokio::test]
async fn generated_image_helper_appends_image_and_output_hint() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
generatedImage({
  image_url: "data:image/png;base64,AAA",
  output_hint: "generated image save hint",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![
                FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAA".to_string(),
                    detail: Some(crate::DEFAULT_IMAGE_DETAIL),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "generated image save hint".to_string(),
                },
            ],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_second_arg_overrides_explicit_object_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image(
  {
    image_url: "data:image/png;base64,AAA",
    detail: "high",
  },
  "original",
);
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(crate::ImageDetail::Original),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helper_second_arg_overrides_raw_mcp_image_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image(
  {
    type: "image",
    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
    mimeType: "image/png",
    _meta: { "codex/imageDetail": "original" },
  },
  "high",
);
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: vec![FunctionCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
                    detail: Some(crate::ImageDetail::High),
                }],
                error_text: None,
            }
        );
}

#[tokio::test]
async fn image_helper_accepts_low_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image({
  image_url: "data:image/png;base64,AAA",
  detail: "low",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,AAA".to_string(),
                detail: Some(crate::ImageDetail::Low),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn image_helpers_reject_remote_urls() {
    for image_url in [
        "http://example.com/image.jpg",
        "https://example.com/image.jpg",
    ] {
        for source in [
            format!("image({image_url:?});"),
            format!("generatedImage({{ image_url: {image_url:?} }});"),
        ] {
            let service = InProcessCodeModeSession::new();

            let response = execute(
                &service,
                ExecuteRequest {
                    source,
                    yield_time_ms: None,
                    ..execute_request("")
                },
            )
            .await;

            assert_eq!(
                    response,
                    RuntimeResponse::Result {
                        cell_id: cell_id("1"),
                        content_items: Vec::new(),
                        error_text: Some(
                            "Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead".to_string(),
                        ),
                    }
                );
        }
    }
}

#[tokio::test]
async fn image_helper_rejects_unsupported_detail() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
        &service,
        ExecuteRequest {
            source: r#"
image({
  image_url: "data:image/png;base64,AAA",
  detail: "medium",
});
"#
            .to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: Some("image detail must be one of: auto, low, high, original".to_string()),
        }
    );
}

#[tokio::test]
async fn image_helper_rejects_raw_mcp_result_container() {
    let service = InProcessCodeModeSession::new();

    let response = execute(
            &service,
            ExecuteRequest {
                source: r#"
image({
  content: [
    {
      type: "image",
      data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
      mimeType: "image/png",
      _meta: { "codex/imageDetail": "original" },
    },
  ],
  isError: false,
});
"#
                .to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await;

    assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id("1"),
                content_items: Vec::new(),
                error_text: Some(
                    "image expects a non-empty image URL string, an object with image_url and optional detail, or a raw MCP image block".to_string(),
                ),
            }
        );
}

#[tokio::test]
async fn wait_reports_missing_cell_separately_from_runtime_results() {
    let service = InProcessCodeModeSession::new();

    let response = service
        .wait(WaitRequest {
            cell_id: cell_id("missing"),
            yield_time_ms: 1,
        })
        .await
        .unwrap();

    assert_eq!(
        response,
        WaitOutcome::MissingCell(RuntimeResponse::Result {
            cell_id: cell_id("missing"),
            content_items: Vec::new(),
            error_text: Some("exec cell missing not found".to_string()),
        })
    );
}
