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
    // Endpoint admission is independent of CODEX_HOME. Lock ordering is home
    // (caller), then endpoint; no path takes these in the opposite order.
    let parent = socket_path
        .parent()
        .ok_or_else(|| std::io::Error::other("socket has no parent"))?;
    codex_uds::prepare_private_socket_directory(parent.clone()).await?;
    let mut lock_name = socket_path
        .as_path()
        .file_name()
        .ok_or_else(|| std::io::Error::other("socket has no filename"))?
        .to_os_string();
    lock_name.push(".startup.lock");
    let endpoint_lock_path =
        AbsolutePathBuf::from_absolute_path(parent.canonicalize()?.join(lock_name))?;
    let _endpoint_lock = acquire_app_server_startup_lock(endpoint_lock_path).await?;
    prepare_control_socket_path(socket_path.as_path()).await?;
    let listener = UnixListener::bind(socket_path.as_path()).await?;
    let endpoint = codex_uds::SocketFile::open(socket_path.as_path())?;
    let socket_guard = ControlSocketFileGuard {
        socket_path,
        endpoint: Some(endpoint),
    };
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
                        tokio::select! {
                            biased;
                            _ = shutdown_token.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
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
    drop(listener);
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

    let endpoint = match codex_uds::SocketFile::open(socket_path) {
        Ok(endpoint) => endpoint,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    // Recheck liveness while the inspected generation is protected. The first
    // probe alone cannot authorize deletion: its pathname may have changed.
    match UnixStream::connect(socket_path).await {
        Ok(_) => Err(std::io::Error::new(
            ErrorKind::AddrInUse,
            "control socket became active during stale recovery",
        )),
        Err(err) if err.kind() == ErrorKind::ConnectionRefused => endpoint.remove(),
        Err(err) => Err(err),
    }
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(AppServerStartupLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        "app-server startup lock contention exceeded 10 seconds",
                    ));
                }
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
    endpoint: Option<codex_uds::SocketFile>,
}

impl Drop for ControlSocketFileGuard {
    fn drop(&mut self) {
        if let Some(endpoint) = self.endpoint.take()
            && let Err(err) = endpoint.remove()
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
