use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::ORIGIN;
use axum::http::header::AUTHORIZATION;
use axum::middleware;
use axum::middleware::Next;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::any;
use axum::routing::get;
use std::io::Write as _;
use std::net::SocketAddr;
use tokio::io;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::TcpListener;
use tracing::info;
use tracing::warn;

use crate::ExecServerRuntimePaths;
use crate::ExecServerTelemetry;
use crate::connection::JsonRpcConnection;
use crate::connection::MAX_JSONRPC_MESSAGE_LEN;
use crate::server::processor::ConnectionProcessor;
use crate::telemetry::ConnectionTransport;

pub const DEFAULT_LISTEN_URL: &str = "ws://127.0.0.1:0";
/// Per-launch capability passed privately by the launcher to the server and its loopback clients.
pub const EXEC_SERVER_AUTH_TOKEN_ENV_VAR: &str = "CODEX_EXEC_SERVER_AUTH_TOKEN";

fn listener_authorization() -> io::Result<http::HeaderValue> {
    let token = std::env::var(EXEC_SERVER_AUTH_TOKEN_ENV_VAR).map_err(|_| {
        io::Error::new(io::ErrorKind::PermissionDenied,
            "exec-server WebSocket mode requires a private per-launch CODEX_EXEC_SERVER_AUTH_TOKEN; use stdio for a private parent-child transport")
    })?;
    if token.len() < 32 || !token.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "CODEX_EXEC_SERVER_AUTH_TOKEN must contain at least 32 URL-safe ASCII characters"));
    }
    let mut authorization = http::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid exec-server authorization"))?;
    authorization.set_sensitive(true);
    Ok(authorization)
}

pub(crate) fn loopback_authorization(url: &str) -> Result<Option<http::HeaderValue>, tokio_tungstenite::tungstenite::Error> {
    let is_loopback = url::Url::parse(url).ok().and_then(|url| url.host().map(|host| match host {
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
        url::Host::Domain(_) => false,
    })).unwrap_or(false);
    if is_loopback && std::env::var_os(EXEC_SERVER_AUTH_TOKEN_ENV_VAR).is_some() {
        return listener_authorization().map(Some).map_err(tokio_tungstenite::tungstenite::Error::Io);
    }
    Ok(None)
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ExecServerListenTransport {
    WebSocket(SocketAddr),
    Stdio,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ExecServerListenUrlParseError {
    UnsupportedListenUrl(String),
    InvalidWebSocketListenUrl(String),
}

impl std::fmt::Display for ExecServerListenUrlParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecServerListenUrlParseError::UnsupportedListenUrl(listen_url) => write!(
                f,
                "unsupported --listen URL `{listen_url}`; expected `ws://IP:PORT` or `stdio`"
            ),
            ExecServerListenUrlParseError::InvalidWebSocketListenUrl(listen_url) => write!(
                f,
                "invalid websocket --listen URL `{listen_url}`; expected `ws://IP:PORT`"
            ),
        }
    }
}

impl std::error::Error for ExecServerListenUrlParseError {}

pub(crate) fn parse_listen_url(
    listen_url: &str,
) -> Result<ExecServerListenTransport, ExecServerListenUrlParseError> {
    if matches!(listen_url, "stdio" | "stdio://") {
        return Ok(ExecServerListenTransport::Stdio);
    }

    if let Some(socket_addr) = listen_url.strip_prefix("ws://") {
        return socket_addr
            .parse::<SocketAddr>()
            .map(ExecServerListenTransport::WebSocket)
            .map_err(|_| {
                ExecServerListenUrlParseError::InvalidWebSocketListenUrl(listen_url.to_string())
            });
    }

    Err(ExecServerListenUrlParseError::UnsupportedListenUrl(
        listen_url.to_string(),
    ))
}

pub(crate) async fn run_transport(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_transport_with_shutdown(
        listen_url,
        runtime_paths,
        telemetry,
        crate::ExecServerShutdown::new(),
    )
    .await
}

pub(crate) async fn run_transport_with_shutdown(
    listen_url: &str,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    shutdown: crate::ExecServerShutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match parse_listen_url(listen_url)? {
        ExecServerListenTransport::WebSocket(bind_address) => {
            run_websocket_listener(bind_address, runtime_paths, telemetry, shutdown).await
        }
        ExecServerListenTransport::Stdio => {
            run_stdio_connection(runtime_paths, telemetry, shutdown).await
        }
    }
}

async fn run_stdio_connection(
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    shutdown: crate::ExecServerShutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_stdio_connection_with_io_and_shutdown(
        io::stdin(),
        io::stdout(),
        runtime_paths,
        telemetry,
        shutdown,
    )
    .await
}


async fn run_stdio_connection_with_io_and_shutdown<R, W>(
    reader: R,
    writer: W,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    shutdown: crate::ExecServerShutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let processor =
        ConnectionProcessor::new_with_telemetry(runtime_paths, telemetry).with_shutdown(shutdown);
    tracing::info!("codex-exec-server listening on stdio");
    processor
        .run_connection(
            JsonRpcConnection::from_stdio(reader, writer, "exec-server stdio".to_string()),
            ConnectionTransport::Stdio,
        )
        .await;
    // Stdio serves exactly one connection, so detached sessions cannot be resumed.
    processor.shutdown().await;
    Ok(())
}

async fn run_websocket_listener(
    bind_address: SocketAddr,
    runtime_paths: ExecServerRuntimePaths,
    telemetry: ExecServerTelemetry,
    shutdown: crate::ExecServerShutdown,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !bind_address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the exec-server WebSocket listener requires a loopback address",
        )
        .into());
    }
    // Fail closed before opening a port. Never print the capability with the endpoint.
    let authorization = listener_authorization()?;
    let listener = TcpListener::bind(bind_address).await?;
    let local_addr = listener.local_addr()?;
    let processor = ConnectionProcessor::new_with_telemetry(runtime_paths, telemetry)
        .with_shutdown(shutdown.clone());
    info!("codex-exec-server listening on ws://{local_addr}");
    println!("ws://{local_addr}");
    std::io::stdout().flush()?;

    let router = Router::new()
        .route("/", any(websocket_upgrade_handler))
        .route("/readyz", get(readiness_handler))
        .layer(middleware::from_fn(reject_requests_with_origin_header))
        .with_state(ExecServerWebSocketState {
            authorization,
            processor: processor.clone(),
            connections: std::sync::Arc::new(tokio::sync::Semaphore::new(128)),
        });
    let result = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.cancelled().await })
    .await;
    processor.shutdown().await;
    result?;
    Ok(())
}

#[derive(Clone)]
struct ExecServerWebSocketState {
    authorization: http::HeaderValue,
    processor: ConnectionProcessor,
    connections: std::sync::Arc<tokio::sync::Semaphore>,
}

async fn readiness_handler() -> StatusCode {
    StatusCode::OK
}

async fn reject_requests_with_origin_header(
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.headers().contains_key(ORIGIN) {
        warn!(
            method = %request.method(),
            uri = %request.uri(),
            "rejecting exec-server websocket listener request with Origin header"
        );
        Err(StatusCode::FORBIDDEN)
    } else {
        Ok(next.run(request).await)
    }
}

async fn websocket_upgrade_handler(
    websocket: WebSocketUpgrade,
    headers: http::HeaderMap,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    State(state): State<ExecServerWebSocketState>,
) -> impl IntoResponse {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let authorized = values.next().is_some_and(|value| {
        constant_time_eq::constant_time_eq(value.as_bytes(), state.authorization.as_bytes())
    }) && values.next().is_none();
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(connection_permit) = state.connections.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "1")],
            "websocket connection limit reached",
        )
            .into_response();
    };
    info!(%peer_addr, "exec-server websocket client connected");
    // Clients send each JSON-RPC message as one frame; accept the same ceiling
    // as the stdio and Noise transports instead of the 16 MiB frame default.
    websocket
        .max_frame_size(MAX_JSONRPC_MESSAGE_LEN)
        .max_message_size(MAX_JSONRPC_MESSAGE_LEN)
        .on_upgrade(move |stream| async move {
            let _connection_permit = connection_permit;
            state
                .processor
                .run_connection(
                    JsonRpcConnection::from_axum_websocket(
                        stream,
                        format!("exec-server websocket {peer_addr}"),
                    ),
                    ConnectionTransport::WebSocket,
                )
                .await;
        })
        .into_response()
}

