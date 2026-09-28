use crate::policy::is_non_public_ip;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::extensions::ExtensionsMut;
use rama_net::address::ProxyAddress;
use rama_net::client::EstablishedClientConnection;
use rama_net::transport::TryRefIntoTransportContext;
use rama_tcp::TcpStream;
use rama_tcp::client::TcpStreamConnector;
use rama_tcp::client::service::TcpConnector;
use std::io;
use std::net::SocketAddr;

/// A local destination explicitly authorized by the request's host policy.
/// The socket check still restricts the actual resolved address to this destination.
#[derive(Clone, Copy, Debug)]
pub(crate) enum LocalTarget {
    Ip(std::net::IpAddr),
    Loopback,
}

impl LocalTarget {
    fn permits(self, ip: std::net::IpAddr) -> bool {
        match self {
            Self::Ip(expected) => ip == expected,
            Self::Loopback => ip.is_loopback(),
        }
    }
}

/// Dials direct targets, rejecting non-public addresses unless the request's policy snapshot
/// allows local binding or explicitly authorized that exact local destination.
#[derive(Clone)]
pub(crate) struct TargetCheckedTcpConnector {
    allow_local_binding: bool,
}

impl TargetCheckedTcpConnector {
    pub(crate) fn from_allow_local_binding(allow_local_binding: bool) -> Self {
        Self {
            allow_local_binding,
        }
    }
}

impl<Input> Service<Input> for TargetCheckedTcpConnector
where
    Input: TryRefIntoTransportContext + Send + ExtensionsMut + 'static,
    Input::Error: Into<BoxError> + Send + Sync + 'static,
{
    type Output = EstablishedClientConnection<TcpStream, Input>;
    type Error = BoxError;

    async fn serve(&self, input: Input) -> Result<Self::Output, Self::Error> {
        if input.extensions().get::<ProxyAddress>().is_some() {
            return TcpConnector::new().serve(input).await;
        }

        TcpConnector::new()
            .with_connector(TargetCheckedStreamConnector {
                allow_local_binding: self.allow_local_binding,
                local_target: input.extensions().get::<LocalTarget>().copied(),
            })
            .serve(input)
            .await
    }
}

#[derive(Clone)]
struct TargetCheckedStreamConnector {
    allow_local_binding: bool,
    local_target: Option<LocalTarget>,
}

impl TcpStreamConnector for TargetCheckedStreamConnector {
    type Error = BoxError;

    async fn connect(&self, addr: SocketAddr) -> Result<TcpStream, Self::Error> {
        if !self.allow_local_binding
            && is_non_public_ip(addr.ip())
            && !self
                .local_target
                .is_some_and(|target| target.permits(addr.ip()))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "network target rejected by policy",
            )
            .into());
        }

        tokio::net::TcpStream::connect(addr)
            .await
            .map(TcpStream::from)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rama_net::address::HostWithPort;
    use std::net::Ipv4Addr;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn explicit_local_target_only_authorizes_the_selected_address() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let target = listener.local_addr().unwrap();
        let connector = TargetCheckedTcpConnector::from_allow_local_binding(false);
        let mut wrong = rama_tcp::client::Request::new(HostWithPort::from(target));
        wrong
            .extensions_mut()
            .insert(LocalTarget::Ip("127.0.0.2".parse().unwrap()));
        let error = connector.serve(wrong).await.unwrap_err();
        assert!(format!("{error:?}").contains("network target rejected by policy"));

        let mut allowed = rama_tcp::client::Request::new(HostWithPort::from(target));
        allowed
            .extensions_mut()
            .insert(LocalTarget::Ip(target.ip()));
        let connected = connector.serve(allowed).await.unwrap();
        let (_, peer) = listener.accept().await.unwrap();
        assert!(peer.ip().is_loopback());
        drop(connected);
        assert!(!LocalTarget::Loopback.permits("10.0.0.1".parse().unwrap()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_rejects_non_public_target_when_local_binding_disabled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let connector = TargetCheckedTcpConnector::from_allow_local_binding(false);

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::from(target));
        let err = Service::serve(&connector, request)
            .await
            .expect_err("local target should be rejected");

        assert!(
            format!("{err:?}").contains("network target rejected by policy"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_connector_allows_non_public_target_when_local_binding_enabled() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind local listener");
        let target = listener.local_addr().expect("local addr");
        let connector = TargetCheckedTcpConnector::from_allow_local_binding(true);

        let request: rama_tcp::client::Request =
            rama_tcp::client::Request::new(HostWithPort::from(target));
        let result = Service::serve(&connector, request).await;

        assert!(result.is_ok(), "local target should be allowed: {result:?}");
    }
}
