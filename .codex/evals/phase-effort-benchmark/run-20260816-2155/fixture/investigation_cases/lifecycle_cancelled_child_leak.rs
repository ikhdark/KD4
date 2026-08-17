use std::io;
use std::process::ExitStatus;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

pub async fn run_until_cancelled(
    mut child: Child,
    cancellation: CancellationToken,
) -> io::Result<ExitStatus> {
    tokio::select! {
        status = child.wait() => status,
        _ = cancellation.cancelled() => {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        }
    }
}
