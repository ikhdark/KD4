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

