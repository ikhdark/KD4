//! Proxy-aware WebSocket connection setup shared by Codex API clients.

mod dialer;

use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use codex_http_client::BuildCustomCaTransportError;
use codex_http_client::HttpClientFactory;
use codex_http_client::build_rustls_client_config_with_custom_ca;
use futures::Sink;
use futures::Stream;
use rustls::ClientConfig;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream as TungsteniteStream;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::handshake::client::Response;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Connects WebSockets using the outbound proxy policy resolved by application configuration.
///
/// Construct this from the effective [`HttpClientFactory`] rather than selecting proxy behavior at
/// individual call sites. Each connection resolves its destination through that factory before
/// opening a socket.
#[derive(Clone)]
pub struct WebSocketConnector {
    http_client_factory: HttpClientFactory,
    tls_config: Arc<ClientConfig>,
    tcp_nodelay: TcpNodelay,
    direct_route_only: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TcpNodelay {
    Default,
    Enabled,
}

impl WebSocketConnector {
    /// Creates a connector using native roots and any configured Codex custom CA bundle.
    ///
    /// The same TLS configuration secures both HTTPS proxy tunnels and `wss` targets, so every
    /// route honors the Codex custom-CA policy.
    pub fn new(
        http_client_factory: &HttpClientFactory,
    ) -> Result<Self, BuildCustomCaTransportError> {
        Ok(Self {
            http_client_factory: http_client_factory.clone(),
            tls_config: build_rustls_client_config_with_custom_ca()?,
            tcp_nodelay: TcpNodelay::Default,
            direct_route_only: false,
        })
    }

    /// Disables Nagle's algorithm for latency-sensitive WebSocket connections.
    pub fn with_tcp_nodelay(mut self) -> Self {
        self.tcp_nodelay = TcpNodelay::Enabled;
        self
    }

    /// Refuses a proxy route before dispatch instead of bypassing configured policy.
    /// Use for credentials whose authority is confined to a direct local peer.
    pub fn with_direct_route_only(mut self) -> Self {
        self.direct_route_only = true;
        self
    }

    /// Connects a WebSocket after resolving the request destination through the configured proxy
    /// policy.
    pub async fn connect(
        &self,
        request: Request,
        config: WebSocketConfig,
    ) -> Result<(WebSocketConnection, Response), WebSocketError> {
        let route_url = request.uri().to_string();
        let proxy_route = self
            .http_client_factory
            .resolve_proxy_route_async(route_url.clone())
            .await
            .map_err(WebSocketError::Io)?;
        tracing::debug!(
            target: "codex_websocket_client::routing", {
            event.name = "codex.websocket.route_selected",
            policy = ?self.http_client_factory.outbound_proxy_policy(),
            route = ?proxy_route,
            destination_port = request.uri().port_u16(),
            direct_route_only = self.direct_route_only,
            },
            "WebSocket route selected without endpoint or credential content"
        );
        let result = codex_http_client::admit_dispatch(dialer::connect(
            request,
            config,
            Arc::clone(&self.tls_config),
            proxy_route,
            self.tcp_nodelay,
            self.direct_route_only,
        ))
        .await
        .map_err(|error| {
            WebSocketError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                DispatchRejected(error),
            ))
        })?;
        let (inner, response) = match result {
            Ok(connected) => connected,
            Err(error) => {
                // Connection/upgrade setup has not sent a model request. Retire
                // cached OS routing after failure so the owning setup retry can
                // observe VPN/proxy changes, without replaying anything here.
                self.http_client_factory.invalidate_cached_proxy_route(&route_url);
                return Err(error);
            }
        };
        Ok((WebSocketConnection { inner }, response))
    }
}

/// The connection was refused before its first network poll because the request lost its
/// dispatch authority (for example, an account change). Nothing was sent, so callers can tell
/// it apart from a socket failure that should count against the transport.
#[derive(Debug)]
pub struct DispatchRejected(String);

impl std::fmt::Display for DispatchRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DispatchRejected {}

/// An established WebSocket independent of its direct, proxy, and TLS transport layers.
///
/// This implements [`Stream`] and [`Sink`] so protocol clients can process Tungstenite messages
/// without knowing which concrete network stream route selection produced.
pub struct WebSocketConnection {
    inner: ConnectionInner,
}

impl Stream for WebSocketConnection {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut self.get_mut().inner {
            ConnectionInner::Routed(stream) => Pin::new(stream).poll_next(context),
        }
    }
}

impl Sink<Message> for WebSocketConnection {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match &mut self.get_mut().inner {
            ConnectionInner::Routed(stream) => Pin::new(stream).poll_ready(context),
        }
    }

    fn start_send(self: Pin<&mut Self>, message: Message) -> Result<(), Self::Error> {
        match &mut self.get_mut().inner {
            ConnectionInner::Routed(stream) => Pin::new(stream).start_send(message),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match &mut self.get_mut().inner {
            ConnectionInner::Routed(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match &mut self.get_mut().inner {
            ConnectionInner::Routed(stream) => Pin::new(stream).poll_close(context),
        }
    }
}

pub(crate) enum ConnectionInner {
    Routed(TungsteniteStream<MaybeTlsStream<Box<dyn AsyncIo>>>),
}

/// Async network I/O carried through optional proxy and target TLS handshakes.
pub(crate) trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

