use codex_http_client::ByteStream;
use codex_http_client::StreamError;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::timeout;

/// Minimal SSE helper that forwards raw `data:` frames as UTF-8 strings.
///
/// Parsing errors and idle timeouts are sent as `Err(StreamError)` before the
/// task exits. Clean end-of-stream closes the output channel without an error;
/// protocol-specific completion requirements belong in the typed consumer.
/// The timeout measures time without a complete event, including comment-only
/// heartbeats or partial events. Dropping the receiver stops the producer.
pub fn sse_stream(
    stream: ByteStream,
    idle_timeout: Duration,
    tx: mpsc::Sender<Result<String, StreamError>>,
) {
    tokio::spawn(async move {
        let mut stream = stream
            .map(|res| res.map_err(|e| StreamError::Stream(e.to_string())))
            .eventsource();

        loop {
            let next = tokio::select! {
                _ = tx.closed() => return,
                next = timeout(idle_timeout, stream.next()) => next,
            };
            match next {
                Ok(Some(Ok(ev))) => {
                    if tx.send(Ok(ev.data)).await.is_err() {
                        return;
                    }
                }
                Ok(Some(Err(e))) => {
                    let _ = tx.send(Err(StreamError::Stream(e.to_string()))).await;
                    return;
                }
                Ok(None) => return,
                Err(_) => {
                    let _ = tx.send(Err(StreamError::Timeout)).await;
                    return;
                }
            }
        }
    });
}
