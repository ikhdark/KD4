use crate::connect_policy::TargetCheckedTcpConnector;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rama_core::Layer;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::error::ErrorExt as _;
use rama_core::error::OpaqueError;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use rama_core::service::BoxService;
use rama_http::Body;
use rama_http::Request;
use rama_http::Response;
use rama_http::layer::version_adapter::RequestVersionAdapter;
use rama_http_backend::client::HttpClientService;
use rama_http_backend::client::HttpConnector;
use rama_http_backend::client::proxy::layer::HttpProxyConnectorLayer;
use rama_net::address::ProxyAddress;
use rama_net::client::EstablishedClientConnection;
use rama_net::http::RequestContext;
use rama_tls_rustls::client::TlsConnectorDataBuilder;
use rama_tls_rustls::client::TlsConnectorLayer;
use rama_tls_rustls::client::client_root_certs;
use rama_tls_rustls::dep::rustls;
use std::sync::Arc;
use std::time::Instant;
use tracing::info;
use tracing::warn;

#[derive(Clone, Default)]
struct ProxyConfig {
    http: Option<ProxyAddress>,
    https: Option<ProxyAddress>,
    all: Option<ProxyAddress>,
}

impl ProxyConfig {
    fn from_env() -> Self {
        let http = read_proxy_env(&["HTTP_PROXY", "http_proxy"]);
        let https = read_proxy_env(&["HTTPS_PROXY", "https_proxy"]);
        let all = read_proxy_env(&["ALL_PROXY", "all_proxy"]);
        Self { http, https, all }
    }

    fn proxy_for_protocol(&self, is_secure: bool) -> Option<ProxyAddress> {
        if is_secure {
            self.https
                .clone()
                .or_else(|| self.http.clone())
                .or_else(|| self.all.clone())
        } else {
            self.http.clone().or_else(|| self.all.clone())
        }
    }
}

fn read_proxy_env(keys: &[&str]) -> Option<ProxyAddress> {
    read_proxy_env_with(keys, |key| std::env::var(key))
}

fn read_proxy_env_with<F>(keys: &[&str], mut read: F) -> Option<ProxyAddress>
where
    F: FnMut(&str) -> Result<String, std::env::VarError>,
{
    for key in keys {
        let value = match read(key) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => continue,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn!("ignoring {key}: proxy address is not valid UTF-8");
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match ProxyAddress::try_from(value) {
            Ok(proxy) => {
                if proxy
                    .protocol
                    .as_ref()
                    .map(rama_net::Protocol::is_http)
                    .unwrap_or(true)
                {
                    return Some(proxy);
                }
                warn!("ignoring {key}: non-http proxy protocol");
                return None;
            }
            Err(err) => {
                warn!("ignoring {key}: invalid proxy address ({err})");
                return None;
            }
        }
    }
    None
}

pub(crate) fn proxy_for_connect() -> Option<ProxyAddress> {
    ProxyConfig::from_env().proxy_for_protocol(/*is_secure*/ true)
}

#[derive(Clone)]
pub(crate) struct UpstreamClient {
    connector: BoxService<
        Request<Body>,
        EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
        BoxError,
    >,
    proxy_config: ProxyConfig,
}

impl UpstreamClient {
    pub(crate) fn direct_with_current_roots(allow_local_binding: bool) -> Self {
        Self::new(
            ProxyConfig::default(),
            TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
            client_root_certs(),
        )
    }

    pub(crate) fn from_env_proxy_with_current_roots(allow_local_binding: bool) -> Self {
        Self::new(
            ProxyConfig::from_env(),
            TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
            client_root_certs(),
        )
    }

    pub(crate) fn direct_with_allow_local_binding(
        allow_local_binding: bool,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig::default(),
            TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
            tls_root_store,
        )
    }

    pub(crate) fn from_env_proxy_with_allow_local_binding(
        allow_local_binding: bool,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig::from_env(),
            TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
            tls_root_store,
        )
    }

    fn new(
        proxy_config: ProxyConfig,
        transport: TargetCheckedTcpConnector,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        let connector = build_http_connector(transport, tls_root_store);
        Self {
            connector,
            proxy_config,
        }
    }
}

impl Service<Request<Body>> for UpstreamClient {
    type Output = Response;
    type Error = OpaqueError;

    async fn serve(&self, mut req: Request<Body>) -> Result<Self::Output, Self::Error> {
        let request_context = RequestContext::try_from(&req).ok();
        let authority = request_context
            .as_ref()
            .map(|ctx| ctx.host_with_port().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let proxy = self.proxy_config.proxy_for_protocol(
            request_context
                .as_ref()
                .map(|ctx| ctx.protocol.is_secure())
                .unwrap_or(false),
        );
        match proxy.as_ref() {
            Some(proxy) => info!(
                "HTTP upstream route selected (target={authority}, route=upstream_proxy, proxy={})",
                proxy.address
            ),
            None => info!("HTTP upstream route selected (target={authority}, route=direct)"),
        }
        if let Some(proxy) = proxy {
            req.extensions_mut().insert(proxy);
        }

        let uri = req.uri().clone();
        let connect_started_at = Instant::now();
        let EstablishedClientConnection {
            input: mut req,
            conn: http_connection,
        } = match self.connector.serve(req).await {
            Ok(connection) => {
                info!(
                    "HTTP upstream connection established (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                connection
            }
            Err(err) => {
                warn!(
                    "HTTP upstream connection failed (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                return Err(OpaqueError::from_boxed(err));
            }
        };

        req.extensions_mut()
            .extend(http_connection.extensions().clone());

        let request_started_at = Instant::now();
        match http_connection.serve(req).await {
            Ok(resp) => {
                info!(
                    "HTTP upstream response headers received (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Ok(resp)
            }
            Err(err) => {
                warn!(
                    "HTTP upstream response headers failed (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Err(OpaqueError::from_boxed(err)
                    .context(format!("http request failure for uri: {uri}")))
            }
        }
    }
}

fn build_http_connector(
    transport: TargetCheckedTcpConnector,
    tls_root_store: Arc<rustls::RootCertStore>,
) -> BoxService<
    Request<Body>,
    EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
    BoxError,
> {
    ensure_rustls_crypto_provider();
    let proxy = HttpProxyConnectorLayer::optional().into_layer(transport);
    let client_config = rustls::ClientConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
        .with_root_certificates(tls_root_store)
        .with_no_client_auth();
    let tls_config = TlsConnectorDataBuilder::from(client_config)
        .with_alpn_protocols_http_auto()
        .build();
    let tls = TlsConnectorLayer::auto()
        .with_connector_data(tls_config)
        .into_layer(proxy);
    let tls = RequestVersionAdapter::new(tls);
    let connector = HttpConnector::new(tls);
    connector.boxed()
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
