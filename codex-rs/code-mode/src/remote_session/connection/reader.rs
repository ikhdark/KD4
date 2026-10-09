use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::HostToClient;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::driver::DriverEvent;

pub(super) async fn drive_reader<R: AsyncRead + Unpin>(
    mut reader: FramedReader<R>,
    events: mpsc::Sender<DriverEvent>,
    cancellation: CancellationToken,
) -> Result<(), String> {
    loop {
        let message = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = reader.read::<HostToClient>() => result,
        };
        let message = match message {
            Ok(Some(message)) => message,
            Ok(None) => return Err("code-mode host closed its stdout".to_string()),
            Err(err) => return Err(format!("failed to read code-mode host message: {err}")),
        };
        tokio::select! {
            biased;
            // Teardown must not depend on space in the driver's event queue.
            // Its failure path owns notification of every pending operation.
            _ = cancellation.cancelled() => return Ok(()),
            result = events.send(DriverEvent::HostMessage(message)) => {
                result.map_err(|_| "code-mode connection driver closed".to_string())?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_code_mode_protocol::host::DelegateRequestId;
    use codex_code_mode_protocol::host::FramedWriter;
    use std::time::Duration;

    fn message(id: i64) -> HostToClient {
        HostToClient::CancelDelegateRequest {
            id: DelegateRequestId::new(id),
            cause: None,
        }
    }

    #[tokio::test]
    async fn reader_cancellation_releases_backpressured_event_queue() {
        let mut samples = Vec::new();
        for _ in 0..7 {
            let (host, pipe) = tokio::io::duplex(1024);
            let mut host = FramedWriter::new(host);
            host.write(&message(1)).await.unwrap();
            let (events, mut rx) = mpsc::channel(1);
            events.send(DriverEvent::Failed("already queued".into())).await.unwrap();
            let cancellation = CancellationToken::new();
            let pump = drive_reader(FramedReader::new(pipe), events, cancellation.clone());
            tokio::pin!(pump);
            assert!(futures::poll!(&mut pump).is_pending());
            let started = std::time::Instant::now();
            cancellation.cancel();
            tokio::time::timeout(Duration::from_millis(100), &mut pump)
                .await.expect("cancel must not wait for driver queue capacity").unwrap();
            samples.push(started.elapsed().as_micros());
            assert!(matches!(rx.recv().await, Some(DriverEvent::Failed(reason)) if reason == "already queued"));
            assert!(rx.recv().await.is_none(), "no late message after connection retirement");
        }
        samples.sort_unstable();
        eprintln!("reader_cancel_backpressure_us={samples:?} median={}", samples[3]);
    }

    #[tokio::test]
    async fn reader_preserves_messages_and_reports_transport_failure() {
        let (host, pipe) = tokio::io::duplex(1024);
        let mut host = FramedWriter::new(host);
        host.write(&message(1)).await.unwrap();
        host.write(&message(2)).await.unwrap();
        drop(host);
        let (events, mut rx) = mpsc::channel(2);
        let error = drive_reader(FramedReader::new(pipe), events, CancellationToken::new())
            .await.unwrap_err();
        assert!(error.contains("closed its stdout"));
        for id in [1, 2] {
            assert!(matches!(rx.recv().await, Some(DriverEvent::HostMessage(actual)) if actual == message(id)));
        }
        assert!(rx.recv().await.is_none());
    }
}
