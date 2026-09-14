#![allow(clippy::expect_used)]
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponseEvent;
use codex_api::ResponsesClient;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use http::HeaderMap;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::Value;

#[derive(Clone)]
struct FixtureSseTransport {
    body: String,
}

impl FixtureSseTransport {
    fn new(body: String) -> Self {
        Self { body }
    }
}

impl HttpTransport for FixtureSseTransport {
    async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("execute should not run".to_string()))
    }

    async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
        let stream = futures::stream::iter(vec![Ok::<Bytes, TransportError>(Bytes::from(
            self.body.clone(),
        ))]);
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(stream),
        })
    }
}

#[derive(Clone, Default)]
struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

fn provider(name: &str) -> Provider {
    Provider {
        name: name.to_string(),
        base_url: "https://example.com/v1".to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: codex_api::RetryConfig {
            max_retries: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: true,
        },
        stream_idle_timeout: Duration::from_millis(50),
    }
}

fn build_responses_body(events: Vec<Value>) -> String {
    let mut body = String::new();
    for e in events {
        let kind = e
            .get("type")
            .and_then(|v| v.as_str())
            .expect("SSE fixture event should have a type");
        body.push_str(&format!("event: {kind}\ndata: {e}\n\n"));
    }
    body
}

#[tokio::test]
async fn responses_stream_parses_items_and_completed_end_to_end() -> Result<()> {
    let item1 = serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello"}]
        }
    });

    let item2 = serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "World"}]
        }
    });

    let completed = serde_json::json!({
        "type": "response.completed",
        "response": { "id": "resp1" }
    });

    let body = build_responses_body(vec![item1, item2, completed]);
    let transport = FixtureSseTransport::new(body);
    let client = ResponsesClient::new(transport, provider("openai"), Arc::new(NoAuth));

    let mut stream = client
        .stream(
            serde_json::json!({"echo": true}),
            HeaderMap::new(),
            Compression::None,
            /*turn_state*/ None,
        )
        .await?;

    let mut events = Vec::new();
    while let Some(ev) = stream.next().await {
        events.push(ev?);
    }

    let events: Vec<ResponseEvent> = events
        .into_iter()
        .filter(|ev| !matches!(ev, ResponseEvent::RateLimits(_)))
        .collect();

    assert_eq!(events.len(), 3);

    match &events[0] {
        ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }) => {
            assert_eq!(role, "assistant");
        }
        other => panic!("unexpected first event: {other:?}"),
    }

    match &events[1] {
        ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }) => {
            assert_eq!(role, "assistant");
        }
        other => panic!("unexpected second event: {other:?}"),
    }

    match &events[2] {
        ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        } => {
            assert_eq!(response_id, "resp1");
            assert!(token_usage.is_none());
            assert!(end_turn.is_none());
        }
        other => panic!("unexpected third event: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn responses_stream_preserves_error_identity_with_unrelated_metadata() -> Result<()> {
    for code in [
        "context_length_exceeded",
        "insufficient_quota",
        "cyber_policy",
    ] {
        let body = build_responses_body(vec![serde_json::json!({
            "type": "response.failed",
            "response": {"error": {
                "code": code,
                "message": "provider rejection",
                "type": [],
                "plan_type": {},
                "resets_at": "unknown"
            }}
        })]);
        let client = ResponsesClient::new(
            FixtureSseTransport::new(body),
            provider("openai"),
            Arc::new(NoAuth),
        );
        let mut stream = client
            .stream(
                serde_json::json!({}),
                HeaderMap::new(),
                Compression::None,
                None,
            )
            .await?;
        let mut errors = Vec::new();
        while let Some(event) = stream.next().await {
            if let Err(error) = event {
                errors.push(error);
            }
        }
        assert_eq!(errors.len(), 1, "{code}: {errors:?}");
        match (code, &errors[0]) {
            ("context_length_exceeded", ApiError::ContextWindowExceeded)
            | ("insufficient_quota", ApiError::QuotaExceeded) => {}
            ("cyber_policy", ApiError::CyberPolicy { message }) => {
                assert_eq!(message, "provider rejection");
            }
            _ => panic!("error identity lost for {code}: {errors:?}"),
        }
    }
    Ok(())
}

#[tokio::test]
async fn responses_stream_rejects_missing_completion_response_end_to_end() -> Result<()> {
    let body = build_responses_body(vec![serde_json::json!({"type": "response.completed"})]);
    let client = ResponsesClient::new(
        FixtureSseTransport::new(body),
        provider("openai"),
        Arc::new(NoAuth),
    );
    let mut stream = client
        .stream(
            serde_json::json!({}),
            HeaderMap::new(),
            Compression::None,
            None,
        )
        .await?;
    let mut errors = Vec::new();
    while let Some(event) = stream.next().await {
        if let Err(error) = event {
            errors.push(error);
        }
    }
    assert!(matches!(errors.as_slice(), [ApiError::Stream(message)]
        if message == "response.completed event missing response"));
    Ok(())
}

#[tokio::test]
async fn responses_stream_accepts_unknown_events_and_exceptional_rate_frames() -> Result<()> {
    let body = build_responses_body(vec![
        serde_json::json!({"type": "future.event"}),
        serde_json::json!({
            "type": "codex.rate_limits",
            "delta": {"future": "shape"},
            "metered_limit_name": "custom-limit",
            "rate_limits": {"secondary": {"used_percent": 42.0}}
        }),
        serde_json::json!({"type": "response.output_text.delta", "delta": "hello"}),
        serde_json::json!({"type": "response.completed", "response": {"id": "done"}}),
    ]);
    let client = ResponsesClient::new(
        FixtureSseTransport::new(body),
        provider("openai"),
        Arc::new(NoAuth),
    );
    let mut stream = client
        .stream(
            serde_json::json!({}),
            HeaderMap::new(),
            Compression::None,
            None,
        )
        .await?;
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let event = event?;
        // Empty headers intentionally emit the default Codex snapshot.
        if matches!(&event, ResponseEvent::RateLimits(snapshot)
            if snapshot.limit_id.as_deref() == Some("codex"))
        {
            continue;
        }
        events.push(event);
    }
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0], ResponseEvent::RateLimits(snapshot)
        if snapshot.limit_id.as_deref() == Some("custom_limit")
            && snapshot.primary.is_none()
            && snapshot.secondary.as_ref().is_some_and(|window| window.used_percent == 42.0)));
    assert!(matches!(&events[1], ResponseEvent::OutputTextDelta(delta) if delta == "hello"));
    assert!(
        matches!(&events[2], ResponseEvent::Completed { response_id, .. } if response_id == "done")
    );
    Ok(())
}
