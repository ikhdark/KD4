use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::path::Path;

use super::TransportEvent;
use crate::transport::websocket::run_websocket_connection;
use codex_uds::UnixListener;
use codex_uds::UnixStream;
use codex_utils_absolute_path::AbsolutePathBuf;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use tokio_tungstenite::accept_async;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::error;
use tracing::info;
use tracing::warn;

// Local clients must finish setup promptly; idle partial HTTP requests retain a socket/task.
pub(super) const CONTROL_SOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn start_control_socket_acceptor(
    socket_path: AbsolutePathBuf,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
) -> IoResult<JoinHandle<()>> {
    prepare_control_socket_path(socket_path.as_path()).await?;
    let listener = UnixListener::bind(socket_path.as_path()).await?;
    let socket_guard = ControlSocketFileGuard { socket_path };
    set_control_socket_permissions(socket_guard.socket_path.as_path()).await?;
    info!(
        socket_path = %socket_guard.socket_path.display(),
        "app-server control socket listening"
    );

    Ok(tokio::spawn(run_control_socket_acceptor(
        listener,
        transport_event_tx,
        shutdown_token,
        socket_guard,
    )))
}

async fn run_control_socket_acceptor(
    mut listener: UnixListener,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
    socket_guard: ControlSocketFileGuard,
) {
    let _socket_guard = socket_guard;
    let connection_tasks = TaskTracker::new();
    loop {
        let stream = tokio::select! {
            _ = shutdown_token.cancelled() => {
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok(stream) => stream,
                    Err(err) => {
                        if matches!(
                            err.kind(),
                            ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
                        ) {
                            warn!("recoverable control socket accept error: {err}");
                            continue;
                        }
                        error!("control socket accept error: {err}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
        };

        let transport_event_tx = transport_event_tx.clone();
        let connection_shutdown = shutdown_token.clone();
        connection_tasks.spawn(run_control_socket_connection(
            stream,
            transport_event_tx,
            connection_shutdown,
        ));
    }
    connection_tasks.close();
    connection_tasks.wait().await;
    info!("control socket acceptor shutting down");
}

async fn run_control_socket_connection(
    stream: UnixStream,
    transport_event_tx: mpsc::Sender<TransportEvent>,
    shutdown_token: CancellationToken,
) {
    let handshake = tokio::select! {
        biased;
        _ = shutdown_token.cancelled() => return,
        result = tokio::time::timeout(CONTROL_SOCKET_HANDSHAKE_TIMEOUT, accept_async(stream)) => result,
    };
    let websocket_stream = match handshake {
        Ok(Ok(websocket_stream)) => websocket_stream,
        Ok(Err(err)) => {
            warn!("failed to upgrade control socket websocket connection: {err}");
            return;
        }
        Err(_) => {
            warn!("control socket websocket handshake timed out");
            return;
        }
    };
    let (websocket_writer, websocket_reader) = websocket_stream.split();
    run_websocket_connection(
        websocket_writer,
        websocket_reader,
        transport_event_tx,
        shutdown_token,
    )
    .await;
}

pub async fn prepare_control_socket_path(socket_path: &Path) -> IoResult<()> {
    if let Some(parent) = socket_path.parent() {
        codex_uds::prepare_private_socket_directory(parent).await?;
    }

    match UnixStream::connect(socket_path).await {
        Ok(_stream) => {
            return Err(std::io::Error::new(
                ErrorKind::AddrInUse,
                format!(
                    "app-server control socket is already in use at {}",
                    socket_path.display()
                ),
            ));
        }
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => {}
        Err(err) => {
            if !tokio::fs::try_exists(socket_path).await.unwrap_or(false) {
                return Ok(());
            }
            return Err(err);
        }
    }

    if !tokio::fs::try_exists(socket_path).await? {
        return Ok(());
    }

    if !codex_uds::is_stale_socket_path(socket_path).await? {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!(
                "app-server control socket path exists and is not a socket: {}",
                socket_path.display()
            ),
        ));
    }
    tokio::fs::remove_file(socket_path).await
}

pub struct AppServerStartupLock {
    _file: std::fs::File,
}

pub async fn acquire_app_server_startup_lock(
    startup_lock_path: AbsolutePathBuf,
) -> IoResult<AppServerStartupLock> {
    if let Some(parent) = startup_lock_path.as_path().parent() {
        codex_uds::prepare_private_socket_directory(parent).await?;
    }
    let file = tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(startup_lock_path.as_path())
    })
    .await
    .map_err(|err| std::io::Error::other(format!("startup lock task failed: {err}")))??;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(AppServerStartupLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err),
        }
    }
}

async fn set_control_socket_permissions(_socket_path: &Path) -> IoResult<()> {
    Ok(())
}

struct ControlSocketFileGuard {
    socket_path: AbsolutePathBuf,
}

impl Drop for ControlSocketFileGuard {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(self.socket_path.as_path())
            && err.kind() != ErrorKind::NotFound
        {
            warn!(
                socket_path = %self.socket_path.display(),
                %err,
                "failed to remove app-server control socket file"
            );
        }
    }
}
