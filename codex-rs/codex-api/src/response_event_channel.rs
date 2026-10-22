//! Count and logical-payload bounds for decoded response queues.
//!
//! Each API/core stage has 32 MiB, hence at most 64 MiB across both queues.
//! Raw ingress, one in-flight decoded event per producer, active item assembly,
//! retained history, and a terminal error are separate owners, not an RSS bound.

use crate::ApiError;
use crate::ResponseEvent;
use serde::Serialize;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

pub const RESPONSE_EVENT_QUEUE_BYTES: usize = 32 * 1024 * 1024;

struct QueuedEvent<E> {
    event: Result<ResponseEvent, E>,
    _bytes: OwnedSemaphorePermit,
}

pub struct ResponseEventSender<E> {
    tx: mpsc::Sender<QueuedEvent<E>>,
    bytes: Arc<Semaphore>,
    limit: usize,
}

pub struct ResponseEventReceiver<E> {
    rx: mpsc::Receiver<QueuedEvent<E>>,
    bytes: Arc<Semaphore>,
}

pub fn response_event_channel<E>(
    capacity: usize,
) -> (ResponseEventSender<E>, ResponseEventReceiver<E>) {
    channel_with_limit(capacity, RESPONSE_EVENT_QUEUE_BYTES)
}

pub(crate) fn channel_with_limit<E>(
    capacity: usize,
    limit: usize,
) -> (ResponseEventSender<E>, ResponseEventReceiver<E>) {
    assert!(limit > 0 && limit <= u32::MAX as usize);
    let (tx, rx) = mpsc::channel(capacity);
    let bytes = Arc::new(Semaphore::new(limit));
    (
        ResponseEventSender {
            tx,
            bytes: bytes.clone(),
            limit,
        },
        ResponseEventReceiver { rx, bytes },
    )
}

impl<E> Clone for ResponseEventSender<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            bytes: self.bytes.clone(),
            limit: self.limit,
        }
    }
}

impl<E> ResponseEventSender<E> {
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
    pub async fn closed(&self) {
        self.tx.closed().await
    }

    pub fn is_backpressured(&self, payload_bytes: usize) -> bool {
        self.tx.capacity() == 0 || payload_bytes > self.bytes.available_permits()
    }

    #[cfg(test)]
    pub(crate) fn queued_payload_bytes(&self) -> usize {
        self.limit - self.bytes.available_permits()
    }

    pub async fn send(&self, event: Result<ResponseEvent, E>) -> Result<(), ApiError> {
        let size = payload_bytes(&event);
        if size > self.limit {
            return Err(ApiError::invalid_response(format!(
                "decoded response event exceeds the {}-byte payload limit",
                self.limit,
            )));
        }
        // Always acquire bytes before a count slot. Drop closes the semaphore,
        // and cancellation drops either permit, including while count-blocked.
        let permit = tokio::select! {
            biased;
            _ = self.tx.closed() => return Err(consumer_closed()),
            permit = self.bytes.clone().acquire_many_owned(size as u32) => {
                permit.map_err(|_| consumer_closed())?
            }
        };
        self.tx
            .send(QueuedEvent {
                event,
                _bytes: permit,
            })
            .await
            .map_err(|_| consumer_closed())
    }

    pub fn try_send(&self, event: Result<ResponseEvent, E>) -> Result<(), ApiError> {
        let size = payload_bytes(&event);
        if size > self.limit {
            return Err(ApiError::invalid_response(
                "decoded response event exceeds the queue payload limit",
            ));
        }
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(size as u32)
            .map_err(|_| ApiError::Stream("response event queue has no byte capacity".into()))?;
        self.tx
            .try_send(QueuedEvent {
                event,
                _bytes: permit,
            })
            .map_err(|_| ApiError::Stream("response event queue is full or closed".into()))
    }
}

fn consumer_closed() -> ApiError {
    ApiError::Stream("response event consumer dropped".into())
}

impl<E> ResponseEventReceiver<E> {
    pub fn len(&self) -> usize {
        self.rx.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    pub async fn recv(&mut self) -> Option<Result<ResponseEvent, E>> {
        self.rx.recv().await.map(|queued| queued.event)
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<ResponseEvent, E>>> {
        Pin::new(&mut self.rx)
            .poll_recv(cx)
            .map(|event| event.map(|queued| queued.event))
    }
}

impl<E> Drop for ResponseEventReceiver<E> {
    fn drop(&mut self) {
        self.bytes.close();
    }
}

fn payload_bytes<E>(event: &Result<ResponseEvent, E>) -> usize {
    event
        .as_ref()
        .map_or(0, ResponseEvent::queued_payload_bytes)
}

// Count encoded payload without allocating another copy of large output items.
// JSON escaping/envelopes conservatively charge more than their string contents.
fn encoded_bytes(value: &impl Serialize) -> usize {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter(0);
    if serde_json::to_writer(&mut count, value).is_err() {
        return usize::MAX;
    }
    count.0
}

impl ResponseEvent {
    /// Logical queued payload, not allocation capacity or process memory.
    pub fn queued_payload_bytes(&self) -> usize {
        match self {
            Self::OutputTextDelta(text) | Self::ServerModel(text) | Self::ModelsEtag(text) => {
                text.len()
            }
            Self::ToolCallInputDelta {
                item_id,
                call_id,
                delta,
            } => item_id
                .len()
                .saturating_add(call_id.as_ref().map_or(0, String::len))
                .saturating_add(delta.len()),
            Self::ReasoningSummaryDelta { delta, .. }
            | Self::ReasoningContentDelta { delta, .. } => delta.len(),
            Self::ReasoningSummaryDone { item_id, text, .. } => {
                item_id.len().saturating_add(text.len())
            }
            Self::OutputItemAdded(item) | Self::OutputItemDone(item) => {
                // Plain reasoning content can be retained locally while omitted
                // from request serialization. Include that otherwise hidden payload.
                let retained_content = match item {
                    codex_protocol::models::ResponseItem::Reasoning { content, .. } => {
                        match content {
                            Some(parts)
                                if !parts.iter().any(|part| {
                                    matches!(part,
                                codex_protocol::models::ReasoningItemContent::ReasoningText { .. })
                                }) =>
                            {
                                encoded_bytes(parts)
                            }
                            _ => 0,
                        }
                    }
                    _ => 0,
                };
                encoded_bytes(item).saturating_add(retained_content)
            }
            Self::Completed {
                response_id,
                token_usage,
                ..
            } => response_id.len().saturating_add(encoded_bytes(token_usage)),
            Self::ModelVerifications(value) => encoded_bytes(value),
            Self::TurnModerationMetadata(value) => encoded_bytes(value),
            Self::RateLimits(value) => encoded_bytes(value),
            Self::SafetyBuffering(value) => value
                .use_cases
                .iter()
                .chain(&value.reasons)
                .map(String::len)
                .chain(value.faster_model.iter().map(String::len))
                .fold(0_usize, usize::saturating_add),
            Self::Created
            | Self::ServerReasoningIncluded(_)
            | Self::ReasoningSummaryPartAdded { .. } => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[test]
    fn locally_retained_reasoning_content_is_charged_even_when_not_serialized() {
        let item = serde_json::from_value(serde_json::json!({
            "type":"reasoning", "summary":[], "content":[{"type":"text","text":"x".repeat(4096)}]
        }))
        .unwrap();
        let event = ResponseEvent::OutputItemDone(item);
        assert!(event.queued_payload_bytes() >= 4096);
    }

    #[tokio::test]
    async fn bytes_bound_large_deltas_and_release_on_drain_cancel_and_drop() {
        let (tx, mut rx) = channel_with_limit::<ApiError>(1600, 8);
        tx.send(Ok(ResponseEvent::OutputTextDelta("12345678".into())))
            .await
            .unwrap();
        let mut blocked = Box::pin(tx.send(Ok(ResponseEvent::OutputTextDelta("x".into()))));
        assert!(blocked.as_mut().now_or_never().is_none());
        assert_eq!(tx.bytes.available_permits(), 0);
        assert!(
            matches!(rx.recv().await, Some(Ok(ResponseEvent::OutputTextDelta(text))) if text == "12345678")
        );
        blocked.await.unwrap();
        assert_eq!(tx.bytes.available_permits(), 7);
        let mut cancelled =
            Box::pin(tx.send(Ok(ResponseEvent::OutputTextDelta("12345678".into()))));
        assert!(cancelled.as_mut().now_or_never().is_none());
        drop(cancelled);
        assert_eq!(tx.bytes.available_permits(), 7);
        let mut blocked = Box::pin(tx.send(Ok(ResponseEvent::OutputTextDelta("12345678".into()))));
        assert!(blocked.as_mut().now_or_never().is_none());
        drop(rx);
        assert!(blocked.await.is_err());
        assert_eq!(tx.bytes.available_permits(), 8);
    }

    #[tokio::test]
    async fn oversized_events_fail_without_waiting_and_count_bounds_empty_events() {
        let (tx, mut rx) = channel_with_limit::<ApiError>(1, 8);
        let error = tx
            .send(Ok(ResponseEvent::OutputTextDelta("123456789".into())))
            .await
            .unwrap_err();
        assert!(!crate::map_api_error(error).is_retryable());
        tx.send(Ok(ResponseEvent::Created)).await.unwrap();
        let mut blocked = Box::pin(tx.send(Ok(ResponseEvent::Created)));
        assert!(blocked.as_mut().now_or_never().is_none());
        drop(blocked);
        assert!(matches!(rx.recv().await, Some(Ok(ResponseEvent::Created))));
        assert_eq!(tx.bytes.available_permits(), 8);
    }
}
