use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::responses_stream::ResponsesEventError;
use crate::responses_stream::ResponsesEventInterpreter;
use crate::responses_stream::ResponsesStreamMetadata;
use crate::telemetry::SseCleanupOutcome;
use crate::telemetry::SsePollPhase;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;
use tracing::trace;

#[cfg(test)]
use crate::common::SafetyBufferingTreatment;
#[cfg(test)]
use crate::responses_stream::OPENAI_MODEL_HEADER;
#[cfg(test)]
use crate::responses_stream::REQUEST_ID_HEADER;
#[cfg(test)]
use crate::responses_stream::ResponsesStreamEvent;
#[cfg(test)]
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";

pub fn spawn_response_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> ResponseStream {
    let metadata = ResponsesStreamMetadata::from_headers(&stream_response.headers);
    metadata.apply_turn_state(turn_state.as_deref());
    let upstream_request_id = metadata.upstream_request_id().map(str::to_string);
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        for event in metadata.initial_events() {
            if tx_event.send(Ok(event)).await.is_err() {
                return;
            }
        }
        process_sse_with_metadata(
            stream_response.bytes,
            tx_event,
            idle_timeout,
            telemetry,
            metadata,
            turn_state,
        )
        .await;
    });

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

#[cfg(test)]
pub async fn process_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    process_sse_with_metadata(
        stream,
        tx_event,
        idle_timeout,
        telemetry,
        ResponsesStreamMetadata::default(),
        None,
    )
    .await;
}

async fn process_sse_with_metadata(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    metadata: ResponsesStreamMetadata,
    turn_state: Option<Arc<OnceLock<String>>>,
) {
    let mut stream = stream.eventsource();
    let mut response_error: Option<ApiError> = None;
    let mut interpreter = ResponsesEventInterpreter::new(&metadata, turn_state);
    let mut poll_ordinal = 0_u64;

    loop {
        let start = Instant::now();
        let response = tokio::select! {
            _ = tx_event.closed() => {
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(SseCleanupOutcome::ConsumerCancelled, start.elapsed());
                }
                return;
            }
            response = timeout(idle_timeout, stream.next()) => response,
        };
        if let Some(t) = telemetry.as_ref() {
            t.on_sse_poll(&response, start.elapsed());
            t.on_sse_phase(
                if poll_ordinal == 0 {
                    SsePollPhase::FirstEvent
                } else {
                    SsePollPhase::SubsequentEvent
                },
                poll_ordinal,
                start.elapsed(),
            );
        }
        poll_ordinal = poll_ordinal.saturating_add(1);
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                debug!("SSE Error: {e:#}");
                let _ = tx_event.send(Err(ApiError::Stream(e.to_string()))).await;
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(SseCleanupOutcome::TransportError, start.elapsed());
                }
                return;
            }
            Ok(None) => {
                let error = response_error.unwrap_or(ApiError::Stream(
                    "stream closed before response.completed".into(),
                ));
                let _ = tx_event.send(Err(error)).await;
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(
                        SseCleanupOutcome::CarrierEofBeforeCompleted,
                        start.elapsed(),
                    );
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream("idle timeout waiting for SSE".into())))
                    .await;
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(SseCleanupOutcome::IdleTimeout, start.elapsed());
                }
                return;
            }
        };

        trace!("SSE event: {}", &sse.data);

        let events = match interpreter.process_payload(&sse.data) {
            Ok(events) => events,
            Err(ResponsesEventError::Parse(error)) => {
                debug!("Failed to parse SSE event: {error}, data: {}", &sse.data);
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(SseCleanupOutcome::ProtocolError, start.elapsed());
                }
                continue;
            }
            Err(ResponsesEventError::Api(error)) => {
                // SSE may include a final protocol error followed by EOF. Preserve the existing
                // behavior of reporting that error when the carrier closes.
                response_error = Some(error);
                continue;
            }
        };

        for event in events {
            let is_completed = matches!(event, ResponseEvent::Completed { .. });
            if tx_event.send(Ok(event)).await.is_err() {
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(SseCleanupOutcome::ConsumerCancelled, start.elapsed());
                }
                return;
            }
            if is_completed {
                // Deliver completion immediately, then keep the carrier alive
                // only for a bounded drain so the pool can reclaim reusable
                // connections without delaying the consumer.
                drop(tx_event);
                let cleanup_start = Instant::now();
                let cleanup_timeout = idle_timeout.min(Duration::from_secs(1));
                let drain = timeout(cleanup_timeout, async {
                    while stream.next().await.is_some() {}
                })
                .await;
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(
                        if drain.is_ok() {
                            SseCleanupOutcome::CompletedAndDrained
                        } else {
                            SseCleanupOutcome::CompletedDrainTimeout
                        },
                        cleanup_start.elapsed(),
                    );
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::SafetyBuffering;
    use assert_matches::assert_matches;
    use bytes::Bytes;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use codex_protocol::models::MessagePhase;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::ModelVerification;
    use futures::TryStreamExt;
    use futures::stream;
    use http::HeaderMap;
    use http::HeaderValue;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_test::io::Builder as IoBuilder;
    use tokio_util::io::ReaderStream;

    async fn collect_events(chunks: &[&[u8]]) -> Vec<Result<ResponseEvent, ApiError>> {
        let mut builder = IoBuilder::new();
        for chunk in chunks {
            builder.read(chunk);
        }

        let reader = builder.build();
        let stream =
            ReaderStream::new(reader).map_err(|err| TransportError::Network(err.to_string()));
        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(16);
        tokio::spawn(process_sse(
            Box::pin(stream),
            tx,
            idle_timeout(),
            /*telemetry*/ None,
        ));

        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev);
        }
        events
    }

    async fn run_sse(events: Vec<serde_json::Value>) -> Vec<ResponseEvent> {
        let mut body = String::new();
        for e in events {
            let kind = e
                .get("type")
                .and_then(|v| v.as_str())
                .expect("fixture event missing type");
            if e.as_object().map(|o| o.len() == 1).unwrap_or(false) {
                body.push_str(&format!("event: {kind}\n\n"));
            } else {
                body.push_str(&format!("event: {kind}\ndata: {e}\n\n"));
            }
        }

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(8);
        let stream = ReaderStream::new(std::io::Cursor::new(body))
            .map_err(|err| TransportError::Network(err.to_string()));
        tokio::spawn(process_sse(
            Box::pin(stream),
            tx,
            idle_timeout(),
            /*telemetry*/ None,
        ));

        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev.expect("channel closed"));
        }
        out
    }

    fn idle_timeout() -> Duration {
        Duration::from_millis(1000)
    }

    #[tokio::test]
    async fn parses_items_and_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}],
                "phase": "commentary"
            }
        })
        .to_string();

        let item2 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "World"}]
            }
        })
        .to_string();

        let completed = json!({
            "type": "response.completed",
            "response": { "id": "resp1" }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");
        let sse2 = format!("event: response.output_item.done\ndata: {item2}\n\n");
        let sse3 = format!("event: response.completed\ndata: {completed}\n\n");

        let events = collect_events(&[sse1.as_bytes(), sse2.as_bytes(), sse3.as_bytes()]).await;

        assert_eq!(events.len(), 3);

        assert_matches!(
            &events[0],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message {
                role,
                phase: Some(MessagePhase::Commentary),
                ..
            })) if role == "assistant"
        );

        assert_matches!(
            &events[1],
            Ok(ResponseEvent::OutputItemDone(ResponseItem::Message { role, .. }))
                if role == "assistant"
        );

        match &events[2] {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                end_turn,
            }) => {
                assert_eq!(response_id, "resp1");
                assert!(token_usage.is_none());
                assert!(end_turn.is_none());
            }
            other => panic!("unexpected third event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_reasoning_summary_done() {
        let events = run_sse(vec![
            json!({
                "type": "response.reasoning_summary_text.done",
                "item_id": "reasoning-1",
                "summary_index": 0,
                "text": "Checking",
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp1" },
            }),
        ])
        .await;

        assert_matches!(
            &events[0],
            ResponseEvent::ReasoningSummaryDone {
                item_id,
                text,
                summary_index: 0,
            } if item_id == "reasoning-1" && text == "Checking"
        );
    }

    #[tokio::test]
    async fn error_when_missing_completed() {
        let item1 = json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello"}]
            }
        })
        .to_string();

        let sse1 = format!("event: response.output_item.done\ndata: {item1}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 2);

        assert_matches!(events[0], Ok(ResponseEvent::OutputItemDone(_)));

        match &events[1] {
            Err(ApiError::Stream(msg)) => {
                assert_eq!(msg, "stream closed before response.completed")
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_tool_search_call_items() {
        let events = run_sse(vec![
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "tool_search_call",
                    "call_id": "search-1",
                    "execution": "client",
                    "arguments": {
                        "query": "calendar create",
                        "limit": 1
                    }
                }
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp1" }
            }),
        ])
        .await;

        assert_eq!(events.len(), 2);
        assert_matches!(
            &events[0],
            ResponseEvent::OutputItemDone(ResponseItem::ToolSearchCall {
                call_id,
                execution,
                arguments,
                ..
            }) if call_id.as_deref() == Some("search-1")
                && execution == "client"
                && arguments == &json!({"query": "calendar create", "limit": 1})
        );
    }

    #[tokio::test]
    async fn parses_tool_call_input_deltas() {
        let events = run_sse(vec![
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": "ctc_1",
                "call_id": "call_1",
                "delta": "*** Begin",
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"input\":\"",
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp1" }
            }),
        ])
        .await;

        assert_matches!(
            &events[0],
            ResponseEvent::ToolCallInputDelta {
                item_id,
                call_id: Some(call_id),
                delta,
            } if item_id == "ctc_1" && call_id == "call_1" && delta == "*** Begin"
        );
        assert_matches!(&events[1], ResponseEvent::Completed { .. });
    }

    #[tokio::test]
    async fn emits_completed_without_stream_end() {
        let completed = json!({
            "type": "response.completed",
            "response": { "id": "resp1" }
        })
        .to_string();

        let sse1 = format!("event: response.completed\ndata: {completed}\n\n");
        let stream = stream::iter(vec![Ok(Bytes::from(sse1))]).chain(stream::pending());
        let stream: ByteStream = Box::pin(stream);

        let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(8);
        tokio::spawn(process_sse(
            stream,
            tx,
            idle_timeout(),
            /*telemetry*/ None,
        ));

        let events = tokio::time::timeout(Duration::from_millis(1000), async {
            let mut events = Vec::new();
            while let Some(ev) = rx.recv().await {
                events.push(ev);
            }
            events
        })
        .await
        .expect("timed out collecting events");

        assert_eq!(events.len(), 1);
        match &events[0] {
            Ok(ResponseEvent::Completed {
                response_id,
                token_usage,
                end_turn,
            }) => {
                assert_eq!(response_id, "resp1");
                assert!(token_usage.is_none());
                assert!(end_turn.is_none());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn dropping_response_stream_cancels_upstream_without_idle_timeout() {
        let stream: ByteStream = Box::pin(stream::pending());
        let (tx, rx) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1);
        let task = tokio::spawn(process_sse(
            stream,
            tx,
            Duration::from_secs(30),
            /*telemetry*/ None,
        ));

        drop(rx);

        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("dropping the consumer should wake the SSE task")
            .expect("SSE task should exit cleanly");
    }

    #[tokio::test]
    async fn error_when_error_event() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_689bcf18d7f08194bf3440ba62fe05d803fee0cdac429894","object":"response","created_at":1755041560,"status":"failed","background":false,"error":{"code":"rate_limit_exceeded","message":"Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."}, "usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(ApiError::Retryable { message, delay }) => {
                assert_eq!(
                    message,
                    "Rate limit reached for gpt-5.1 in organization org-AAA on tokens per min (TPM): Limit 30000, Used 22999, Requested 12528. Please try again in 11.054s. Visit https://platform.openai.com/account/rate-limits to learn more."
                );
                assert_eq!(*delay, Some(Duration::from_secs_f64(11.054)));
            }
            other => panic!("unexpected second event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_window_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_5c66275b97b9baef1ed95550adb3b7ec13b17aafd1d2f11b","object":"response","created_at":1759510079,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try again."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::ContextWindowExceeded));
    }

    #[tokio::test]
    async fn context_window_error_with_newline_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":4,"response":{"id":"resp_fatal_newline","object":"response","created_at":1759510080,"status":"failed","background":false,"error":{"code":"context_length_exceeded","message":"Your input exceeds the context window of this model. Please adjust your input and try\nagain."},"usage":null,"user":null,"metadata":{}}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::ContextWindowExceeded));
    }

    #[tokio::test]
    async fn quota_exceeded_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fatal_quota","object":"response","created_at":1759771626,"status":"failed","background":false,"error":{"code":"insufficient_quota","message":"You exceeded your current quota, please check your plan and billing details. For more information on this error, read the docs: https://platform.openai.com/docs/guides/error-codes/api-errors."},"incomplete_details":null}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        assert_matches!(events[0], Err(ApiError::QuotaExceeded));
    }

    #[tokio::test]
    async fn cyber_policy_error_is_fatal() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fatal_cyber","object":"response","created_at":1759771626,"status":"failed","background":false,"error":{"code":"cyber_policy","message":"This request was flagged for cyber policy."},"incomplete_details":null}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(ApiError::CyberPolicy { message }) => {
                assert_eq!(message, "This request was flagged for cyber policy.");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cyber_policy_error_uses_fallback_for_empty_message() {
        let raw_error = r#"{"type":"response.failed","sequence_number":3,"response":{"id":"resp_fatal_cyber","object":"response","created_at":1759771626,"status":"failed","background":false,"error":{"code":"cyber_policy","message":"   "},"incomplete_details":null}}"#;

        let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

        let events = collect_events(&[sse1.as_bytes()]).await;

        assert_eq!(events.len(), 1);

        match &events[0] {
            Err(ApiError::CyberPolicy { message }) => {
                assert_eq!(
                    message,
                    "This request has been flagged for possible cybersecurity risk."
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn content_policy_errors_without_type_are_invalid_requests() {
        for (code, expected_message) in [
            (
                "invalid_prompt",
                "Invalid prompt: we've limited access to this content for safety reasons.",
            ),
            (
                "bio_policy",
                "This content was flagged for possible biological risk.",
            ),
        ] {
            let raw_error = json!({
                "type": "response.failed",
                "sequence_number": 3,
                "response": {
                    "id": "resp_content_policy_no_type",
                    "object": "response",
                    "created_at": 1759771628,
                    "status": "failed",
                    "background": false,
                    "error": { "code": code, "message": expected_message },
                    "incomplete_details": null,
                },
            })
            .to_string();
            let sse1 = format!("event: response.failed\ndata: {raw_error}\n\n");

            let events = collect_events(&[sse1.as_bytes()]).await;

            assert_eq!(events.len(), 1);
            match &events[0] {
                Err(ApiError::InvalidRequest { message }) => {
                    assert_eq!(message, expected_message);
                }
                other => panic!("unexpected event for {code}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn table_driven_event_kinds() {
        struct TestCase {
            name: &'static str,
            event: serde_json::Value,
            expect_first: fn(&ResponseEvent) -> bool,
            expected_len: usize,
        }

        fn is_created(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::Created)
        }
        fn is_output(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::OutputItemDone(_))
        }
        fn is_completed(ev: &ResponseEvent) -> bool {
            matches!(ev, ResponseEvent::Completed { .. })
        }

        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "c",
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": null,
                    "output_tokens": 0,
                    "output_tokens_details": null,
                    "total_tokens": 0
                },
                "output": []
            }
        });

        let cases = vec![
            TestCase {
                name: "created",
                event: json!({"type": "response.created", "response": {}}),
                expect_first: is_created,
                expected_len: 2,
            },
            TestCase {
                name: "output_item.done",
                event: json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "hi"}
                        ]
                    }
                }),
                expect_first: is_output,
                expected_len: 2,
            },
            TestCase {
                name: "unknown",
                event: json!({"type": "response.new_tool_event"}),
                expect_first: is_completed,
                expected_len: 1,
            },
        ];

        for case in cases {
            let mut evs = vec![case.event];
            evs.push(completed.clone());

            let out = run_sse(evs).await;
            assert_eq!(out.len(), case.expected_len, "case {}", case.name);
            assert!(
                (case.expect_first)(&out[0]),
                "first event mismatch in case {}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn spawn_response_stream_emits_header_events() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req-1"));
        headers.insert(
            OPENAI_MODEL_HEADER,
            HeaderValue::from_static(CYBER_RESTRICTED_MODEL_FOR_TESTS),
        );
        let bytes = stream::iter(Vec::<Result<Bytes, TransportError>>::new());
        let stream_response = StreamResponse {
            status: StatusCode::OK,
            headers,
            bytes: Box::pin(bytes),
        };

        let mut stream = spawn_response_stream(
            stream_response,
            idle_timeout(),
            /*telemetry*/ None,
            /*turn_state*/ None,
        );
        assert_eq!(stream.upstream_request_id.as_deref(), Some("req-1"));
        let event = stream
            .rx_event
            .recv()
            .await
            .expect("expected server model event")
            .expect("expected ok event");
        match event {
            ResponseEvent::ServerModel(model) => {
                assert_eq!(model, CYBER_RESTRICTED_MODEL_FOR_TESTS);
            }
            other => panic!("expected server model event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_response_stream_ignores_model_verification_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "openai-verification-recommendation",
            HeaderValue::from_static(TRUSTED_ACCESS_FOR_CYBER_VERIFICATION),
        );
        let completed = json!({
            "type": "response.completed",
            "response": { "id": "resp-1" }
        });
        let sse = format!("event: response.completed\ndata: {completed}\n\n");
        let bytes = stream::iter(vec![Ok(Bytes::from(sse))]);
        let stream_response = StreamResponse {
            status: StatusCode::OK,
            headers,
            bytes: Box::pin(bytes),
        };

        let mut stream = spawn_response_stream(
            stream_response,
            idle_timeout(),
            /*telemetry*/ None,
            /*turn_state*/ None,
        );
        let mut events = Vec::new();
        while let Some(event) = stream.rx_event.recv().await {
            events.push(event.expect("expected ok event"));
        }

        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ResponseEvent::ModelVerifications(_)))
        );
    }

    #[tokio::test]
    async fn process_sse_ignores_response_model_field_in_payload() {
        let events = run_sse(vec![
            json!({
                "type": "response.created",
                "response": {
                    "id": "resp-1",
                    "model": CYBER_RESTRICTED_MODEL_FOR_TESTS
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-1",
                    "model": CYBER_RESTRICTED_MODEL_FOR_TESTS
                }
            }),
        ])
        .await;

        assert_eq!(events.len(), 2);
        assert_matches!(&events[0], ResponseEvent::Created);
        assert_matches!(
            &events[1],
            ResponseEvent::Completed {
                response_id,
                token_usage: None,
                end_turn: None,
            } if response_id == "resp-1"
        );
    }

    #[tokio::test]
    async fn process_sse_emits_server_model_from_response_headers_payload() {
        let events = run_sse(vec![
            json!({
                "type": "response.created",
                "response": {
                    "id": "resp-1",
                    "headers": {
                        "OpenAI-Model": CYBER_RESTRICTED_MODEL_FOR_TESTS
                    }
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-1"
                }
            }),
        ])
        .await;

        assert_eq!(events.len(), 3);
        assert_matches!(
            &events[0],
            ResponseEvent::ServerModel(model) if model == CYBER_RESTRICTED_MODEL_FOR_TESTS
        );
        assert_matches!(&events[1], ResponseEvent::Created);
        assert_matches!(
            &events[2],
            ResponseEvent::Completed {
                response_id,
                token_usage: None,
                end_turn: None,
            } if response_id == "resp-1"
        );
    }

    #[tokio::test]
    async fn process_sse_emits_model_verification_field() {
        let events = run_sse(vec![
            json!({
                "type": "response.metadata",
                "sequence_number": 1,
                "response_id": "resp-1",
                "metadata": {
                    "openai_verification_recommendation": [TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-1"
                }
            }),
        ])
        .await;

        assert_matches!(
            &events[0],
            ResponseEvent::ModelVerifications(verifications)
                if verifications == &vec![ModelVerification::TrustedAccessForCyber]
        );
        assert_matches!(
            &events[1],
            ResponseEvent::Completed {
                response_id,
                token_usage: None,
                end_turn: None,
            } if response_id == "resp-1"
        );
    }

    #[tokio::test]
    async fn process_sse_emits_turn_moderation_metadata_field() {
        let events = run_sse(vec![
            json!({
                "type": "response.metadata",
                "metadata": {
                    "openai_chatgpt_moderation_metadata": {
                        "presentation": "inline"
                    }
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-1"
                }
            }),
        ])
        .await;

        assert_matches!(
            &events[0],
            ResponseEvent::TurnModerationMetadata(result)
                if result.metadata == json!({"presentation": "inline"})
        );
        assert_matches!(
            &events[1],
            ResponseEvent::Completed {
                response_id,
                token_usage: None,
                end_turn: None,
            } if response_id == "resp-1"
        );
    }

    #[tokio::test]
    async fn process_sse_emits_all_safety_buffering_notifications_without_dropping_response_events()
    {
        let events = run_sse(vec![
            json!({
                "type": "response.created",
                "response": { "id": "resp-1" },
                "safety_buffering": false
            }),
            json!({
                "type": "response.output_text.delta",
                "delta": "hello",
                "safety_buffering": {
                    "use_cases": ["cyber"],
                    "reasons": ["user_risk"],
                    "retry_model": "gpt-fast-wire"
                }
            }),
            json!({
                "type": "response.output_text.delta",
                "delta": " world",
                "safety_buffering": {
                    "use_cases": ["cyber"],
                    "reasons": ["user_risk"]
                }
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp-1" },
                "safety_buffering": {
                    "use_cases": ["cyber"],
                    "reasons": ["user_risk"]
                }
            }),
        ])
        .await;

        assert_eq!(events.len(), 7);
        assert_matches!(&events[0], ResponseEvent::Created);
        assert_matches!(
            &events[1],
            ResponseEvent::SafetyBuffering(buffering)
                if buffering.use_cases == ["cyber"]
                    && buffering.reasons == ["user_risk"]
                    && buffering.show_buffering_ui
                    && buffering.faster_model.as_deref() == Some("gpt-fast-wire")
        );
        assert_matches!(&events[2], ResponseEvent::OutputTextDelta(delta) if delta == "hello");
        assert_matches!(
            &events[3],
            ResponseEvent::SafetyBuffering(buffering)
                if buffering.use_cases == ["cyber"] && buffering.reasons == ["user_risk"]
        );
        assert_matches!(&events[4], ResponseEvent::OutputTextDelta(delta) if delta == " world");
        assert_matches!(
            &events[5],
            ResponseEvent::SafetyBuffering(buffering)
                if buffering.use_cases == ["cyber"] && buffering.reasons == ["user_risk"]
        );
        assert_matches!(&events[6], ResponseEvent::Completed { response_id, .. } if response_id == "resp-1");
    }

    #[test]
    fn safety_buffering_prefers_wire_retry_model_and_only_falls_back_when_omitted() {
        let treatment = SafetyBufferingTreatment {
            faster_model: Some("gpt-fast-header".to_string()),
        };

        for (retry_model, expected_faster_model) in [
            (None, Some("gpt-fast-header")),
            (Some(Value::Null), None),
            (Some(json!("gpt-fast-wire")), Some("gpt-fast-wire")),
        ] {
            let mut event = json!({
                "type": "response.output_text.delta",
                "safety_buffering": {
                    "use_cases": ["cyber"],
                    "reasons": ["user_risk"]
                }
            });
            if let Some(retry_model) = retry_model {
                event["safety_buffering"]["retry_model"] = retry_model;
            }
            let event: ResponsesStreamEvent =
                serde_json::from_value(event).expect("deserialize safety buffering event");

            let buffering = event
                .safety_buffering(&treatment)
                .expect("expected safety buffering payload");

            assert_eq!(
                buffering,
                SafetyBuffering {
                    use_cases: vec!["cyber".to_string()],
                    reasons: vec!["user_risk".to_string()],
                    show_buffering_ui: true,
                    faster_model: expected_faster_model.map(str::to_string),
                }
            );
        }
    }

    #[test]
    fn responses_stream_event_response_model_reads_top_level_headers() {
        let ev: ResponsesStreamEvent = serde_json::from_value(json!({
            "type": "response.metadata",
            "headers": {
                "openai-model": CYBER_RESTRICTED_MODEL_FOR_TESTS,
            }
        }))
        .expect("expected event to deserialize");

        assert_eq!(
            ev.response_model().as_deref(),
            Some(CYBER_RESTRICTED_MODEL_FOR_TESTS)
        );
    }

    #[test]
    fn responses_stream_event_response_model_prefers_response_headers() {
        let ev: ResponsesStreamEvent = serde_json::from_value(json!({
            "type": "response.created",
            "headers": {
                "openai-model": "top-level-model"
            },
            "response": {
                "id": "resp-1",
                "headers": {
                    "openai-model": CYBER_RESTRICTED_MODEL_FOR_TESTS
                }
            }
        }))
        .expect("expected event to deserialize");

        assert_eq!(
            ev.response_model().as_deref(),
            Some(CYBER_RESTRICTED_MODEL_FOR_TESTS)
        );
    }

    #[test]
    fn responses_stream_event_model_verification_reads_metadata_field() {
        let event = json!({
            "type": "response.metadata",
            "sequence_number": 1,
            "response_id": "resp-1",
            "metadata": {
                "openai_verification_recommendation": [TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]
            }
        });
        let event: ResponsesStreamEvent =
            serde_json::from_value(event).expect("expected event to deserialize");

        assert_eq!(
            event.model_verifications(),
            Some(vec![ModelVerification::TrustedAccessForCyber])
        );
    }

    #[test]
    fn responses_stream_event_model_verification_ignores_unknown_field() {
        let event = json!({
            "type": "response.metadata",
            "metadata": {
                "openai_verification_recommendation": ["unknown"]
            }
        });
        let event: ResponsesStreamEvent =
            serde_json::from_value(event).expect("expected event to deserialize");

        assert_eq!(event.model_verifications(), None);
    }

    #[test]
    fn responses_stream_event_model_verification_ignores_non_array_field() {
        let event = json!({
            "type": "response.metadata",
            "metadata": {
                "openai_verification_recommendation": TRUSTED_ACCESS_FOR_CYBER_VERIFICATION
            }
        });
        let event: ResponsesStreamEvent =
            serde_json::from_value(event).expect("expected event to deserialize");

        assert_eq!(event.model_verifications(), None);
    }

    const CYBER_RESTRICTED_MODEL_FOR_TESTS: &str = "gpt-5.3-codex";
}
