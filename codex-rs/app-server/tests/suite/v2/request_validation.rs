use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml_with_chatgpt_base_url;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

use super::analytics::mount_analytics_capture;
use super::analytics::wait_for_matching_analytics_event;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);
const REMOTE_IMAGE_URL_ERROR: &str =
    "remote image URLs are not supported; use an inline data URL instead";

async fn assert_rejected_steer_analytics(
    server: &wiremock::MockServer,
    expected_turn_id: &str,
) -> Result<()> {
    let event = wait_for_matching_analytics_event(server, DEFAULT_READ_TIMEOUT, |event| {
        event["event_type"] == "codex_turn_steer_event"
            && event["event_params"]["expected_turn_id"] == expected_turn_id
    })
    .await?;
    assert_eq!(event["event_params"]["result"], "rejected");
    assert_eq!(event["event_params"]["accepted_turn_id"], json!(null));
    assert_eq!(event["event_params"]["rejection_reason"], json!(null));
    Ok(())
}

#[tokio::test]
async fn request_handlers_reject_remote_image_urls() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        codex_home.path(),
        "http://localhost/unused",
        "http://localhost/unused",
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;
    let thread_id = thread.id;

    let remote_tool_output = serde_json::to_value(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputImage {
                image_url: "https://example.com/tool.png".to_string(),
                detail: Some(ImageDetail::High),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    })?;
    let requests = [
        (
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{
                    "type": "image",
                    "url": "HTTP://example.com/start.png",
                    "detail": "high"
                }]
            }),
        ),
        (
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": "turn-id",
                "input": [{
                    "type": "image",
                    "url": "https://example.com/steer.png",
                    "detail": "high"
                }]
            }),
        ),
        (
            "thread/injectItems",
            json!({
                "threadId": thread_id,
                "items": [remote_tool_output]
            }),
        ),
    ];

    for (method, params) in requests {
        let request_id = mcp.send_raw_request(method, Some(params)).await?;
        let actual: JSONRPCError = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        let expected = JSONRPCError {
            id: RequestId::Integer(request_id),
            error: JSONRPCErrorError {
                code: -32600,
                data: None,
                message: REMOTE_IMAGE_URL_ERROR.to_string(),
            },
        };
        assert_eq!(actual, expected, "unexpected response for {method}");
    }

    Ok(())
}

#[tokio::test]
async fn turn_steer_rejections_emit_analytics_for_preflight_and_queue_failures() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml_with_chatgpt_base_url(
        codex_home.path(),
        &server.uri(),
        &server.uri(),
    )?;
    mount_analytics_capture(&server, codex_home.path()).await?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_request_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_response)?;

    let remote_image_request_id = mcp
        .send_raw_request(
            "turn/steer",
            Some(json!({
                "threadId": thread.id,
                "expectedTurnId": "remote-image",
                "input": [{
                    "type": "image",
                    "url": "https://example.com/steer.png"
                }]
            })),
        )
        .await?;
    let remote_image_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(remote_image_request_id)),
    )
    .await??;
    assert_eq!(remote_image_error.error.code, -32600);
    assert_rejected_steer_analytics(&server, "remote-image").await?;

    let oversized_source = "s".repeat(257);
    let additional_context_request_id = mcp
        .send_raw_request(
            "turn/steer",
            Some(json!({
                "threadId": thread.id,
                "expectedTurnId": "additional-context",
                "input": [{"type": "text", "text": "steer"}],
                "additionalContext": {
                    (oversized_source): {"value": "context", "kind": "untrusted"}
                }
            })),
        )
        .await?;
    let additional_context_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(additional_context_request_id)),
    )
    .await??;
    assert_eq!(additional_context_error.error.code, -32600);
    assert_rejected_steer_analytics(&server, "additional-context").await?;

    let empty_turn_id_request_id = mcp
        .send_raw_request(
            "turn/steer",
            Some(json!({
                "threadId": thread.id,
                "expectedTurnId": "",
                "input": [{"type": "text", "text": "steer"}]
            })),
        )
        .await?;
    let empty_turn_id_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(empty_turn_id_request_id)),
    )
    .await??;
    assert_eq!(empty_turn_id_error.error.code, -32600);
    assert_rejected_steer_analytics(&server, "").await?;

    let queue_overload_context = (0..384)
        .map(|index| {
            (
                format!("source-{index}"),
                json!({"value": "x".repeat(16 * 1024), "kind": "untrusted"}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let queue_overload_request_id = mcp
        .send_raw_request(
            "turn/steer",
            Some(json!({
                "threadId": thread.id,
                "expectedTurnId": "queue-overload",
                "input": [{"type": "text", "text": "steer"}],
                "additionalContext": queue_overload_context,
            })),
        )
        .await?;
    let queue_overload_error: JSONRPCError = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(queue_overload_request_id)),
    )
    .await??;
    assert_eq!(
        queue_overload_error.error.code,
        codex_app_server_protocol::OVERLOADED_ERROR_CODE
    );
    assert_rejected_steer_analytics(&server, "queue-overload").await?;

    Ok(())
}
