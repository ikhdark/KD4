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
