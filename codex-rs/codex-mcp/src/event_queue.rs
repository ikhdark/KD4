//! Admission shared by core and MCP producers of the core event stream.
use async_channel::Sender;
use codex_protocol::protocol::Event;

pub const EVENT_QUEUE_CAPACITY: usize = 1024;
pub const MAX_EVENT_BYTES: usize = 256 * 1024;

/// Count serialized payload without allocating another representation.
pub fn event_fits_budget(event: &Event) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > MAX_EVENT_BYTES {
                return Err(std::io::Error::other(
                    "event exceeds live publication budget",
                ));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), event).is_ok()
}

/// A missing required event ends the stream explicitly, never as a successful
/// terminal receipt. No disk spill or unbounded reply task is needed to fail.
/// The unsent event is boxed so only failures pay for carrying it back.
pub fn try_send_event(
    sender: &Sender<Event>,
    event: Event,
) -> Result<(), Box<async_channel::TrySendError<Event>>> {
    if !event_fits_budget(&event) {
        sender.close();
        return Err(Box::new(async_channel::TrySendError::Closed(event)));
    }
    sender
        .try_send(event)
        .inspect_err(|_| {
            sender.close();
        })
        .map_err(Box::new)
}

/// Ordinary required events may backpressure briefly; a frozen consumer cannot
/// hold pipe drains, startup or cancellation progress indefinitely. Required
/// events are never refused for size: resumed-session startup history, image
/// results and large diffs legitimately exceed the live-publication budget.
pub async fn send_event(
    sender: &Sender<Event>,
    event: Event,
) -> Result<(), Box<async_channel::SendError<Event>>> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(async_channel::TrySendError::Closed(event)) => {
            Err(Box::new(async_channel::SendError(event)))
        }
        Err(async_channel::TrySendError::Full(event)) => {
            // Keep the original for an honest failure result if the bounded wait expires.
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sender.send(event.clone()),
            )
            .await
            {
                Ok(result) => result.map_err(Box::new),
                Err(_) => {
                    sender.close();
                    Err(Box::new(async_channel::SendError(event)))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::WarningEvent;
    fn event(text: String) -> Event {
        Event {
            id: "test".into(),
            msg: EventMsg::Warning(WarningEvent { message: text }),
        }
    }

    #[tokio::test]
    async fn full_or_oversized_stream_fails_without_silent_loss_or_unbounded_wait() {
        let (tx, rx) = async_channel::bounded(1);
        send_event(&tx, event("accepted".into())).await.unwrap();
        assert!(try_send_event(&tx, event("overflow".into())).is_err());
        assert!(
            matches!(rx.recv().await.unwrap().msg, EventMsg::Warning(w) if w.message == "accepted")
        );
        assert!(rx.recv().await.is_err());
        let (tx, rx) = async_channel::bounded(1);
        assert!(try_send_event(&tx, event("x".repeat(MAX_EVENT_BYTES))).is_err());
        assert!(rx.recv().await.is_err());
    }

    #[tokio::test]
    async fn oversized_required_event_is_delivered_without_closing_the_stream() {
        // e.g. resumed-session startup history or a base64 image result.
        let (tx, rx) = async_channel::bounded(2);
        let large = "x".repeat(MAX_EVENT_BYTES * 2);
        send_event(&tx, event(large.clone())).await.unwrap();
        send_event(&tx, event("after".into())).await.unwrap();
        assert!(matches!(rx.recv().await.unwrap().msg, EventMsg::Warning(w) if w.message == large));
        assert!(
            matches!(rx.recv().await.unwrap().msg, EventMsg::Warning(w) if w.message == "after")
        );
        assert!(!tx.is_closed());
    }
}
