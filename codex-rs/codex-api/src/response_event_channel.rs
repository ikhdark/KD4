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
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::Instant;

use codex_protocol::timing::ResponseQueueTiming;

pub const RESPONSE_EVENT_QUEUE_BYTES: usize = 32 * 1024 * 1024;

struct QueuedEvent<E> {
    event: Result<ResponseEvent, E>,
    payload_bytes: usize,
    _bytes: OwnedSemaphorePermit,
    enqueued_at: Instant,
}

/// Immutable payload and its charge travel together across queue boundaries.
pub struct ChargedResponseEvent<E> {
    event: Result<ResponseEvent, E>,
    payload_bytes: usize,
}

impl<E> From<Result<ResponseEvent, E>> for ChargedResponseEvent<E> {
    fn from(event: Result<ResponseEvent, E>) -> Self {
        let payload_bytes = payload_bytes(&event);
        Self { event, payload_bytes }
    }
}

impl<E> ChargedResponseEvent<E> {
    pub fn event(&self) -> &Result<ResponseEvent, E> { &self.event }
    pub fn payload_bytes(&self) -> usize { self.payload_bytes }
    pub fn into_event(self) -> Result<ResponseEvent, E> { self.event }

    /// Error conversion cannot change a successful payload. Errors carry no
    /// payload charge, so crossing an error-domain boundary needs no rescan.
    pub fn map_err<F>(self, transform: impl FnOnce(E) -> F) -> ChargedResponseEvent<F> {
        ChargedResponseEvent {
            event: self.event.map_err(transform),
            payload_bytes: self.payload_bytes,
        }
    }

    /// Transformations invalidate the charge; unchanged events never call this.
    pub fn map(self, transform: impl FnOnce(Result<ResponseEvent, E>) -> Result<ResponseEvent, E>) -> Self {
        transform(self.event).into()
    }
}

#[derive(Default)]
struct EnqueueTiming {
    events: AtomicU64,
    wait_ns: AtomicU64,
    max_wait_ns: AtomicU64,
}

impl EnqueueTiming {
    fn record(&self, elapsed_ns: u64) {
        self.events.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .wait_ns
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(elapsed_ns))
            });
        self.max_wait_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
    }
}

pub struct ResponseEventSender<E> {
    tx: mpsc::Sender<QueuedEvent<E>>,
    bytes: Arc<Semaphore>,
    limit: usize,
    timing: Arc<EnqueueTiming>,
}

pub struct ResponseEventReceiver<E> {
    rx: mpsc::Receiver<QueuedEvent<E>>,
    bytes: Arc<Semaphore>,
    enqueue_timing: Arc<EnqueueTiming>,
    timing: ResponseQueueTiming,
    timing_observer: Option<Box<dyn FnOnce(ResponseQueueTiming) + Send + Sync>>,
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
    let timing = Arc::new(EnqueueTiming::default());
    (
        ResponseEventSender {
            tx,
            bytes: bytes.clone(),
            limit,
            timing: Arc::clone(&timing),
        },
        ResponseEventReceiver {
            rx,
            bytes,
            enqueue_timing: timing,
            timing: ResponseQueueTiming::default(),
            timing_observer: None,
        },
    )
}

impl<E> Clone for ResponseEventSender<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            bytes: self.bytes.clone(),
            limit: self.limit,
            timing: Arc::clone(&self.timing),
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
        self.send_charged(event.into()).await
    }

    pub async fn send_charged(&self, charged: ChargedResponseEvent<E>) -> Result<(), ApiError> {
        let ChargedResponseEvent { event, payload_bytes: size } = charged;
        if size > self.limit {
            return Err(ApiError::invalid_response(format!(
                "decoded response event exceeds the {}-byte payload limit",
                self.limit,
            )));
        }
        let started = Instant::now();
        // Always acquire bytes before a count slot. Drop closes the semaphore,
        // and cancellation drops either permit, including while count-blocked.
        let permit = tokio::select! {
            biased;
            _ = self.tx.closed() => return Err(consumer_closed()),
            permit = self.bytes.clone().acquire_many_owned(size as u32) => {
                permit.map_err(|_| consumer_closed())?
            }
        };
        let slot = self.tx.reserve().await.map_err(|_| consumer_closed())?;
        let enqueued_at = Instant::now();
        self.timing
            .record(nanos(enqueued_at.duration_since(started)));
        slot.send(QueuedEvent {
            event,
            payload_bytes: size,
            _bytes: permit,
            enqueued_at,
        });
        Ok(())
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
        let slot = self
            .tx
            .try_reserve()
            .map_err(|_| ApiError::Stream("response event queue is full or closed".into()))?;
        let enqueued_at = Instant::now();
        self.timing.record(0);
        slot.send(QueuedEvent {
            event,
            payload_bytes: size,
            _bytes: permit,
            enqueued_at,
        });
        Ok(())
    }
}

fn consumer_closed() -> ApiError {
    ApiError::Stream("response event consumer dropped".into())
}

impl<E> ResponseEventReceiver<E> {
    /// One bounded receipt at terminal dequeue or consumer drop. Installing an
    /// observer never changes channel capacity, polling, or cancellation.
    pub fn observe_timing(
        &mut self,
        observer: impl FnOnce(ResponseQueueTiming) + Send + Sync + 'static,
    ) {
        assert!(
            self.timing_observer.is_none(),
            "queue observer already installed"
        );
        self.timing_observer = Some(Box::new(observer));
    }

    fn receive(&mut self, queued: QueuedEvent<E>) -> ChargedResponseEvent<E> {
        let elapsed = nanos(queued.enqueued_at.elapsed());
        self.timing.dequeued_events = self.timing.dequeued_events.saturating_add(1);
        self.timing.queue_residence_ns = self.timing.queue_residence_ns.saturating_add(elapsed);
        self.timing.max_queue_residence_ns = self.timing.max_queue_residence_ns.max(elapsed);
        if matches!(&queued.event, Ok(ResponseEvent::Completed { .. }) | Err(_)) {
            self.timing.terminal_received = true;
            self.finish_timing();
        }
        ChargedResponseEvent { event: queued.event, payload_bytes: queued.payload_bytes }
    }

    fn finish_timing(&mut self) {
        if let Some(observer) = self.timing_observer.take() {
            self.timing.enqueued_events = self.enqueue_timing.events.load(Ordering::Relaxed);
            self.timing.enqueue_wait_ns = self.enqueue_timing.wait_ns.load(Ordering::Relaxed);
            self.timing.max_enqueue_wait_ns =
                self.enqueue_timing.max_wait_ns.load(Ordering::Relaxed);
            observer(self.timing.clone());
        }
    }

    pub fn len(&self) -> usize {
        self.rx.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }

    pub async fn recv(&mut self) -> Option<Result<ResponseEvent, E>> {
        let queued = self.rx.recv().await?;
        Some(self.receive(queued).into_event())
    }

    pub fn poll_recv_charged(&mut self, cx: &mut Context<'_>) -> Poll<Option<ChargedResponseEvent<E>>> {
        match Pin::new(&mut self.rx).poll_recv(cx) {
            Poll::Ready(Some(queued)) => Poll::Ready(Some(self.receive(queued))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<ResponseEvent, E>>> {
        self.poll_recv_charged(cx).map(|event| event.map(ChargedResponseEvent::into_event))
    }
}

impl<E> Drop for ResponseEventReceiver<E> {
    fn drop(&mut self) {
        self.bytes.close();
        self.finish_timing();
    }
}

fn nanos(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn payload_bytes<E>(event: &Result<ResponseEvent, E>) -> usize {
    #[cfg(test)]
    CHARGE_CALCULATIONS.with(|count| count.set(count.get() + 1));
    event
        .as_ref()
        .map_or(0, ResponseEvent::queued_payload_bytes)
}

#[cfg(test)]
thread_local! { static CHARGE_CALCULATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

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
                response_item_payload_bytes(item)
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

/// Serialized logical charge plus otherwise-hidden retained reasoning content.
pub fn response_item_payload_bytes(item: &codex_protocol::models::ResponseItem) -> usize {
    let retained_content = match item {
        codex_protocol::models::ResponseItem::Reasoning { content: Some(parts), .. }
            if !parts.iter().any(|part| matches!(part,
                codex_protocol::models::ReasoningItemContent::ReasoningText { .. })) => encoded_bytes(parts),
        _ => 0,
    };
    encoded_bytes(item).saturating_add(retained_content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn immutable_charge_survives_two_queues_and_changed_payload_is_recharged() {
        let (tx, mut rx) = channel_with_limit::<ApiError>(1, 1024 * 1024);
        let (next_tx, mut next_rx) = channel_with_limit::<String>(1, 1024 * 1024);
        let item = serde_json::from_value(serde_json::json!({
            "type":"function_call", "name":"read_file", "call_id":"c", "arguments":"\"\\\n".repeat(32768)
        })).unwrap();
        let expected = response_item_payload_bytes(&item);
        let before = CHARGE_CALCULATIONS.with(std::cell::Cell::get);
        tx.send(Ok(ResponseEvent::OutputItemDone(item))).await.unwrap();
        let envelope = futures::future::poll_fn(|cx| rx.poll_recv_charged(cx)).await.unwrap();
        assert_eq!(envelope.payload_bytes(), expected);
        assert_eq!(tx.bytes.available_permits(), 1024 * 1024);
        next_tx.send_charged(envelope.map_err(|error| error.to_string())).await.unwrap();
        assert_eq!(next_tx.queued_payload_bytes(), expected);
        let envelope = futures::future::poll_fn(|cx| next_rx.poll_recv_charged(cx)).await.unwrap();
        assert_eq!(CHARGE_CALCULATIONS.with(std::cell::Cell::get) - before, 1);
        let changed = envelope.map(|_| Ok(ResponseEvent::OutputTextDelta("x".repeat(1024 * 1024 + 1))));
        assert!(next_tx.send_charged(changed).await.is_err());
        assert_eq!(next_tx.bytes.available_permits(), 1024 * 1024);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_timing_separates_capacity_wait_from_residence() {
        let (tx, mut rx) = channel_with_limit::<ApiError>(1, 8);
        let (receipt_tx, receipt_rx) = tokio::sync::oneshot::channel();
        rx.observe_timing(move |receipt| {
            let _ = receipt_tx.send(receipt);
        });
        tx.send(Ok(ResponseEvent::Created)).await.unwrap();
        let mut blocked = Box::pin(tx.send(Ok(ResponseEvent::Completed {
            response_id: "r".into(),
            token_usage: None,
            end_turn: None,
        })));
        assert!(blocked.as_mut().now_or_never().is_none());
        tokio::time::advance(std::time::Duration::from_millis(7)).await;
        assert!(matches!(rx.recv().await, Some(Ok(ResponseEvent::Created))));
        blocked.await.unwrap();
        tokio::time::advance(std::time::Duration::from_millis(11)).await;
        assert!(matches!(
            rx.recv().await,
            Some(Ok(ResponseEvent::Completed { .. }))
        ));
        let receipt = receipt_rx.await.unwrap();
        assert_eq!((receipt.enqueued_events, receipt.dequeued_events), (2, 2));
        assert_eq!(receipt.enqueue_wait_ns, 7_000_000);
        assert_eq!(receipt.queue_residence_ns, 18_000_000);
        assert_eq!(receipt.max_enqueue_wait_ns, 7_000_000);
        assert_eq!(receipt.max_queue_residence_ns, 11_000_000);
        assert!(receipt.terminal_received);
        assert_eq!(tx.bytes.available_permits(), 8);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_timing_keeps_silence_and_cancelled_delivery_partial() {
        let (tx, mut rx) = channel_with_limit::<ApiError>(1, 8);
        let (receipt_tx, receipt_rx) = tokio::sync::oneshot::channel();
        rx.observe_timing(move |receipt| {
            let _ = receipt_tx.send(receipt);
        });
        // Provider silence before enqueue is not host queue residence.
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tx.send(Ok(ResponseEvent::Created)).await.unwrap();
        assert!(matches!(rx.recv().await, Some(Ok(ResponseEvent::Created))));
        tx.try_send(Ok(ResponseEvent::Created)).unwrap();
        let mut blocked = Box::pin(tx.send(Ok(ResponseEvent::Created)));
        assert!(blocked.as_mut().now_or_never().is_none());
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        drop(blocked);
        drop(rx);
        let receipt = receipt_rx.await.unwrap();
        assert_eq!((receipt.enqueued_events, receipt.dequeued_events), (2, 1));
        assert_eq!(receipt.enqueue_wait_ns, 0);
        assert_eq!(receipt.queue_residence_ns, 0);
        assert!(!receipt.terminal_received);
        assert_eq!(tx.bytes.available_permits(), 8);
    }

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
