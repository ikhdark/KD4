use super::proxy_for_connect;
use crate::connect_policy::TargetCheckedTcpConnector;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rama_core::Layer;
use rama_core::error::BoxError;
use rama_core::extensions::Extensions;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use rama_core::stream::Stream;
use rama_http_backend::client::proxy::layer::HttpProxyConnector;
use rama_net::Protocol;
use rama_net::client::ConnectorService;
use rama_net::client::EstablishedClientConnection;
use rama_net::stream::ClientSocketInfo;
use rama_net::stream::Socket;
use rama_tcp::client::Request;
use rama_tls_rustls::client::TlsConnectorDataBuilder;
use rama_tls_rustls::client::TlsConnectorLayer;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;

pub(crate) struct TunnelConnection {
    // Rama's upgraded stream is Send, not Sync. Only &mut self accesses it;
    // Mutex supplies Socket's Sync bound without locking in the I/O path.
    stream: Mutex<Pin<Box<dyn Stream>>>,
    local: SocketAddr,
    peer: SocketAddr,
    extensions: Extensions,
}

impl std::fmt::Debug for TunnelConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TunnelConnection").finish_non_exhaustive()
    }
}

// Both HTTP CONNECT and opaque SOCKS traffic use the same upstream route and
// destination-checked direct dial. TLS here protects an HTTPS proxy, not the
// opaque destination stream.
pub(crate) async fn connect_tunnel(
    mut req: Request,
    allow_upstream_proxy: bool,
    allow_local_binding: bool,
) -> Result<EstablishedClientConnection<TunnelConnection, Request>, BoxError> {
    ensure_rustls_crypto_provider();
    let proxy = allow_upstream_proxy
        .then(|| proxy_for_connect(&req.authority))
        .flatten();
    if let Some(proxy) = proxy {
        req.extensions_mut().insert(proxy);
    } else if req
        .extensions()
        .contains::<rama_net::address::ProxyAddress>()
    {
        return Err(io::Error::other("direct tunnel contains a proxy route").into());
    }
    let req = req.with_protocol(Protocol::HTTPS);
    let connector = HttpProxyConnector::optional(
        TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
    );
    let connector = TlsConnectorLayer::tunnel(None)
        .with_connector_data(
            TlsConnectorDataBuilder::new()
                .with_alpn_protocols_http_auto()
                .build(),
        )
        .into_layer(connector);
    let EstablishedClientConnection { input, conn } = connector.connect(req).await?;
    let socket = conn
        .extensions()
        .get::<ClientSocketInfo>()
        .ok_or_else(|| io::Error::other("tunnel is missing socket metadata"))?;
    let conn = TunnelConnection {
        local: *socket
            .local_addr()
            .ok_or_else(|| io::Error::other("tunnel is missing local address"))?,
        peer: *socket.peer_addr(),
        extensions: conn.extensions().clone(),
        stream: Mutex::new(Box::pin(conn)),
    };
    Ok(EstablishedClientConnection { input, conn })
}

impl AsyncRead for TunnelConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut()
            .stream
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .poll_read(cx, buf)
    }
}
impl AsyncWrite for TunnelConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut()
            .stream
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut()
            .stream
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut()
            .stream
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .poll_shutdown(cx)
    }
}
impl Socket for TunnelConnection {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.peer)
    }
}
impl ExtensionsRef for TunnelConnection {
    fn extensions(&self) -> &Extensions {
        &self.extensions
    }
}
impl ExtensionsMut for TunnelConnection {
    fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}
