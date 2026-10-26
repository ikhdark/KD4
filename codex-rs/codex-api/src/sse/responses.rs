use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::response_event_channel::ResponseEventSender;
use crate::response_event_channel::response_event_channel;
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
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::Instrument;
use tracing::debug;
use tracing::trace;


const MAX_SSE_EVENT_BYTES: usize = 32 * 1024 * 1024;

// Bound parser accumulation before UTF-8/JSON decoding. Scan raw bytes once,
// recognizing LF, CRLF and CR even across chunks. Only pass complete events to
// eventsource-stream: partial payloads make it repeatedly scan the same line.
fn bound_sse_events(stream: ByteStream, limit: usize) -> ByteStream {
    struct Framing {
        stream: ByteStream,
        pending: bytes::Bytes,
        event: bytes::BytesMut,
        bytes: usize,
        line_has_data: bool,
        after_cr: bool,
        finished: bool,
        since_yield: usize,
        observed_body_byte: bool,
    }
    let state = Framing {
        stream,
        pending: bytes::Bytes::new(),
        event: bytes::BytesMut::new(),
        bytes: 0,
        line_has_data: false,
        after_cr: false,
        finished: false,
        since_yield: 0,
        observed_body_byte: false,
    };
    futures::stream::unfold(state, move |mut state| async move {
        if state.finished {
            return None;
        }
        loop {
            // Bound work even for an always-ready source of comments, empty
            // chunks, or one large unfinished event.
            tokio::task::consume_budget().await;
            const SCAN_CHUNK_BYTES: usize = 64 * 1024;
            if state.since_yield >= SCAN_CHUNK_BYTES {
                tokio::task::yield_now().await;
                state.since_yield = 0;
            }
            if state.pending.is_empty() {
                match state.stream.next().await {
                    Some(Ok(chunk)) => {
                        if !state.observed_body_byte && !chunk.is_empty() {
                            state.observed_body_byte = true;
                            codex_http_client::record_first_response_body_byte();
                        }
                        state.pending = chunk;
                    }
                    Some(Err(error)) => {
                        state.finished = true;
                        return Some((Err(error), state));
                    }
                    None => {
                        state.finished = true;
                        // Preserve EOF/UTF-8 classification for truncated input.
                        return if state.event.is_empty() {
                            None
                        } else {
                            Some((Ok(state.event.split().freeze()), state))
                        };
                    }
                }
            }
            if state.after_cr && state.bytes == 0 && state.pending.first() == Some(&b'\n') {
                // The previous empty CR line was already terminated below.
                let _ = state.pending.split_to(1);
                state.after_cr = false;
            }
            let len = state.pending.len().min(SCAN_CHUNK_BYTES);
            let chunk = &state.pending[..len];
            let mut index = 0;
            let mut complete = false;
            let mut overflow = false;
            while index < chunk.len() {
                if state.after_cr && chunk[index] == b'\n' {
                    state.after_cr = false;
                    if state.bytes == limit {
                        overflow = true;
                        break;
                    }
                    state.bytes += 1;
                    index += 1;
                    continue;
                }
                state.after_cr = false;
                let run = chunk[index..]
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .unwrap_or(chunk.len() - index);
                if run > limit - state.bytes {
                    overflow = true;
                    break;
                }
                state.bytes += run;
                state.line_has_data |= run != 0;
                index += run;
                if index == chunk.len() {
                    break;
                }
                if state.bytes == limit {
                    overflow = true;
                    break;
                }
                state.bytes += 1;
                state.after_cr = chunk[index] == b'\r';
                complete = !state.line_has_data;
                state.line_has_data = false;
                index += 1;
                if complete {
                    state.bytes = 0;
                    break;
                }
            }
            if overflow {
                state.finished = true;
                return Some((
                    Err(codex_client::TransportError::Build(format!(
                        "SSE event exceeds the {limit}-byte framing limit"
                    ))),
                    state,
                ));
            }
            state.since_yield += index;
            let chunk = state.pending.split_to(index);
            if complete && state.event.is_empty() && !state.after_cr {
                // Common case: an event in one network chunk needs no copy.
                return Some((Ok(chunk), state));
            }
            state.event.extend_from_slice(&chunk);
            if complete {
                if state.after_cr {
                    // nom's streaming CRLF parser needs lookahead after CR.
                    // Finish an empty CR line without waiting for another event;
                    // swallow its optional wire LF on the next iteration.
                    state.event.extend_from_slice(b"\n");
                }
                return Some((Ok(state.event.split().freeze()), state));
            }
        }
    })
    .boxed()
}


pub(crate) fn spawn_response_stream_with_dialect(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    turn_state: Option<Arc<OnceLock<String>>>,
    ollama_single_summary: bool,
) -> ResponseStream {
    let mut metadata = ResponsesStreamMetadata::from_headers(&stream_response.headers);
    metadata.ollama_single_summary = ollama_single_summary;
    metadata.apply_turn_state(turn_state.as_deref());
    let upstream_request_id = metadata.upstream_request_id().map(str::to_string);
    let (tx_event, rx_event) = response_event_channel::<ApiError>(1600);
    tokio::spawn(
        codex_http_client::with_transport_timing(async move {
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
        })
        .in_current_span(),
    );

    ResponseStream {
        rx_event,
        upstream_request_id,
    }
}

async fn process_sse_with_metadata(
    stream: ByteStream,
    tx_event: ResponseEventSender<ApiError>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
    metadata: ResponsesStreamMetadata,
    turn_state: Option<Arc<OnceLock<String>>>,
) {
    let mut stream = bound_sse_events(stream, MAX_SSE_EVENT_BYTES).eventsource();
    let mut interpreter = ResponsesEventInterpreter::new(&metadata, turn_state);
    let mut poll_ordinal = 0_u64;
    let mut received_events = 0_u64;
    let mut received_payload_bytes = 0_usize;
    let mut progress_deadline = Instant::now() + idle_timeout;

    loop {
        if Instant::now() >= progress_deadline {
            let _ = tx_event
                .send(Err(ApiError::Stream(
                    "model progress timeout waiting for SSE".into(),
                )))
                .await;
            return;
        }
        let start = telemetry.as_ref().map(|_| Instant::now());
        let response = tokio::select! {
            _ = tx_event.closed() => {
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(SseCleanupOutcome::ConsumerCancelled, start.elapsed());
                }
                return;
            }
            response = tokio::time::timeout_at(progress_deadline.min(Instant::now() + idle_timeout), stream.next()) => response,
        };
        if let Some((t, start)) = telemetry.as_ref().zip(start) {
            let poll_duration = start.elapsed();
            t.on_sse_poll(&response, poll_duration);
            t.on_sse_phase(
                if poll_ordinal == 0 {
                    SsePollPhase::FirstEvent
                } else {
                    SsePollPhase::SubsequentEvent
                },
                poll_ordinal,
                poll_duration,
            );
        }
        poll_ordinal = poll_ordinal.saturating_add(1);
        let sse = match response {
            Ok(Some(Ok(sse))) => sse,
            Ok(Some(Err(e))) => {
                let (error, outcome) = match e {
                    eventsource_stream::EventStreamError::Transport(
                        codex_client::TransportError::Build(message),
                    ) => (
                        ApiError::invalid_response(message),
                        SseCleanupOutcome::ProtocolError,
                    ),
                    eventsource_stream::EventStreamError::Utf8(error)
                        if error.utf8_error().error_len().is_none() =>
                    {
                        // The decoder emits this only when EOF splits a code point:
                        // unlike invalid bytes in a complete frame, transport recovery
                        // may produce the missing suffix.
                        (
                            ApiError::Stream(
                                "stream closed within a UTF-8 sequence before response.completed"
                                    .to_string(),
                            ),
                            SseCleanupOutcome::CarrierEofBeforeCompleted,
                        )
                    }
                    eventsource_stream::EventStreamError::Utf8(_) => (
                        ApiError::invalid_response("invalid UTF-8 in SSE event"),
                        SseCleanupOutcome::ProtocolError,
                    ),
                    eventsource_stream::EventStreamError::Parser(_) => (
                        ApiError::invalid_response("invalid SSE framing"),
                        SseCleanupOutcome::ProtocolError,
                    ),
                    error => (
                        ApiError::Stream(error.to_string()),
                        SseCleanupOutcome::TransportError,
                    ),
                };
                debug!("SSE Error: {error:#}");
                let _ = tx_event.send(Err(error)).await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(outcome, start.elapsed());
                }
                return;
            }
            Ok(None) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(format!(
                        "stream closed before response.completed (received {received_events} SSE events, {received_payload_bytes} payload bytes)"
                    ))))
                    .await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(
                        SseCleanupOutcome::CarrierEofBeforeCompleted,
                        start.elapsed(),
                    );
                }
                return;
            }
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(format!(
                        "idle timeout waiting for SSE after {}ms",
                        idle_timeout.as_millis()
                    ))))
                    .await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(SseCleanupOutcome::IdleTimeout, start.elapsed());
                }
                return;
            }
        };

        received_events = received_events.saturating_add(1);
        received_payload_bytes = received_payload_bytes.saturating_add(sse.data.len());
        let event_name_end = sse
            .event
            .char_indices()
            .nth(128)
            .map_or(sse.event.len(), |(index, _)| index);
        let event_name = &sse.event[..event_name_end];
        trace!(event = %event_name, payload_bytes = sse.data.len(), "SSE event");

        let interpretation_start = telemetry.as_ref().map(|_| Instant::now());
        let events = match interpreter.process_payload(&sse.data) {
            Ok(events) => {
                if let Some((t, start)) = telemetry.as_ref().zip(interpretation_start) {
                    t.on_sse_event(event_name, start.elapsed(), None);
                }
                events
            }
            Err(ResponsesEventError::Parse(error)) => {
                let diagnostic = crate::responses_stream::decode_diagnostic(
                    &format!(
                        "failed to parse SSE event {event_name:?} ({} payload bytes)",
                        sse.data.len()
                    ),
                    &error,
                );
                let safe_error = ApiError::invalid_response(diagnostic);
                if let Some((t, start)) = telemetry.as_ref().zip(interpretation_start) {
                    t.on_sse_event(event_name, start.elapsed(), Some(&safe_error));
                }
                debug!(event = %event_name, payload_bytes = sse.data.len(), error = %safe_error, "Failed to parse SSE event");
                let _ = tx_event.send(Err(safe_error)).await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(SseCleanupOutcome::ProtocolError, start.elapsed());
                }
                return;
            }
            Err(ResponsesEventError::Api(error)) => {
                if let Some((t, start)) = telemetry.as_ref().zip(interpretation_start) {
                    t.on_sse_event(event_name, start.elapsed(), Some(&error));
                }
                let _ = tx_event.send(Err(error)).await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(SseCleanupOutcome::ResponseError, start.elapsed());
                }
                return;
            }
        };

        for event in events {
            let advances = event.advances_model_response();
            let is_completed = matches!(event, ResponseEvent::Completed { .. });
            let delivery_start = Instant::now();
            if let Err(error) = tx_event.send(Ok(event)).await {
                let outcome = if matches!(&error, ApiError::ProviderFailure { .. }) {
                    SseCleanupOutcome::ProtocolError
                } else {
                    SseCleanupOutcome::ConsumerCancelled
                };
                let _ = tx_event.send(Err(error)).await;
                if let Some((t, start)) = telemetry.as_ref().zip(start) {
                    t.on_sse_cleanup(outcome, start.elapsed());
                }
                return;
            }
            if advances {
                progress_deadline = Instant::now() + idle_timeout;
            } else {
                // Consumer backpressure is local waiting, not provider silence.
                progress_deadline += delivery_start.elapsed();
            }
            if is_completed {
                // Deliver completion immediately, then keep the carrier alive
                // only for a bounded drain so the pool can reclaim reusable
                // connections without delaying the consumer.
                drop(tx_event);
                let cleanup_start = Instant::now();
                let cleanup_timeout = idle_timeout.min(Duration::from_secs(1));
                let cleanup_deadline = cleanup_start + cleanup_timeout;
                let drain = timeout(cleanup_timeout, async {
                    loop {
                        if Instant::now() >= cleanup_deadline {
                            return SseCleanupOutcome::CompletedDrainTimeout;
                        }
                        match stream.next().await {
                            Some(Ok(_)) => {}
                            Some(Err(_)) => return SseCleanupOutcome::CompletedDrainError,
                            None => return SseCleanupOutcome::CompletedAndDrained,
                        }
                    }
                })
                .await;
                if let Some(t) = telemetry.as_ref() {
                    t.on_sse_cleanup(
                        drain.unwrap_or(SseCleanupOutcome::CompletedDrainTimeout),
                        cleanup_start.elapsed(),
                    );
                }
                return;
            }
        }
    }
}
