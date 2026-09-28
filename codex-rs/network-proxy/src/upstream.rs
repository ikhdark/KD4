use crate::connect_policy::LocalTarget;
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
use rama_http::layer::version_adapter::adapt_request_version;
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

#[path = "upstream_tunnel.rs"]
mod tunnel;
pub(crate) use tunnel::TunnelConnection;
pub(crate) use tunnel::connect_tunnel;

#[path = "upstream_pool.rs"]
mod pool;

#[derive(Clone, Default, PartialEq, Eq)]
struct ProxyConfig {
    http: Option<ProxyAddress>,
    https: Option<ProxyAddress>,
    all: Option<ProxyAddress>,
    no_proxy: Option<String>,
}

impl ProxyConfig {
    fn from_env() -> Self {
        let http = read_proxy_env(&["HTTP_PROXY", "http_proxy"]);
        let https = read_proxy_env(&["HTTPS_PROXY", "https_proxy"]);
        let all = read_proxy_env(&["ALL_PROXY", "all_proxy"]);
        let no_proxy = ["NO_PROXY", "no_proxy"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok());
        Self {
            http,
            https,
            all,
            no_proxy,
        }
    }

    fn proxy_for_target(&self, is_secure: bool, host: &str, port: u16) -> Option<ProxyAddress> {
        if self
            .no_proxy
            .as_deref()
            .is_some_and(|value| no_proxy_matches(value, host, port))
        {
            return None;
        }
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

fn proxy_for_connect(target: &rama_net::address::HostWithPort) -> Option<ProxyAddress> {
    ProxyConfig::from_env().proxy_for_target(true, &target.host.to_string(), target.port)
}

fn no_proxy_matches(value: &str, host: &str, port: u16) -> bool {
    use std::net::IpAddr;
    let host = crate::policy::normalize_host(host);
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            if entry == "*" {
                return true;
            }
            if let Some((network, bits)) = entry.split_once('/') {
                let (Ok(network), Ok(address), Ok(bits)) = (
                    network.parse::<IpAddr>(),
                    host.parse::<IpAddr>(),
                    bits.parse::<u32>(),
                ) else {
                    return false;
                };
                return match (network, address) {
                    (IpAddr::V4(network), IpAddr::V4(address)) if bits <= 32 => {
                        let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
                        u32::from(network) & mask == u32::from(address) & mask
                    }
                    (IpAddr::V6(network), IpAddr::V6(address)) if bits <= 128 => {
                        let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                        u128::from(network) & mask == u128::from(address) & mask
                    }
                    _ => false,
                };
            }
            let (entry, required_port) = if let Some(bracketed) = entry.strip_prefix('[') {
                let Some((address, suffix)) = bracketed.split_once(']') else {
                    return false;
                };
                if suffix.is_empty() {
                    (address, None)
                } else if let Some(port) = suffix
                    .strip_prefix(':')
                    .and_then(|value| value.parse::<u16>().ok())
                {
                    (address, Some(port))
                } else {
                    return false;
                }
            } else if entry.parse::<IpAddr>().is_ok() {
                (entry, None)
            } else if let Some((address, port)) = entry.rsplit_once(':') {
                let Ok(port) = port.parse::<u16>() else {
                    return false;
                };
                (address, Some(port))
            } else {
                (entry, None)
            };
            if required_port.is_some_and(|required| required != port) {
                return false;
            }
            let entry = crate::policy::normalize_host(
                entry.trim_start_matches("*.").trim_start_matches('.'),
            );
            if let Ok(address) = entry.parse::<IpAddr>() {
                return host.parse::<IpAddr>().ok() == Some(address);
            }
            !entry.is_empty()
                && (host == entry
                    || host
                        .strip_suffix(&entry)
                        .is_some_and(|prefix| prefix.ends_with('.')))
        })
}

#[derive(Clone)]
pub(crate) struct UpstreamClient {
    connector: BoxService<
        Request<Body>,
        EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
        BoxError,
    >,
    proxy_config: ProxyConfig,
    allow_local_binding: bool,
    tls_root_store: Arc<rustls::RootCertStore>,
    pool: pool::Pool,
}

impl UpstreamClient {
    pub(crate) fn direct_with_allow_local_binding(
        allow_local_binding: bool,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(ProxyConfig::default(), allow_local_binding, tls_root_store)
    }

    pub(crate) fn from_env_proxy_with_allow_local_binding(
        allow_local_binding: bool,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(ProxyConfig::from_env(), allow_local_binding, tls_root_store)
    }

    fn new(
        proxy_config: ProxyConfig,
        allow_local_binding: bool,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        let connector = build_http_connector(
            TargetCheckedTcpConnector::from_allow_local_binding(allow_local_binding),
            tls_root_store.clone(),
        );
        Self {
            connector,
            proxy_config,
            allow_local_binding,
            tls_root_store,
            pool: pool::Pool::default(),
        }
    }

    pub(crate) fn cached(slot: &mut Option<Self>, allow_upstream: bool, allow_local: bool) -> Self {
        let proxy = if allow_upstream {
            ProxyConfig::from_env()
        } else {
            ProxyConfig::default()
        };
        let roots = client_root_certs();
        if let Some(client) = slot.as_ref()
            && client.proxy_config == proxy
            && client.allow_local_binding == allow_local
            && Arc::ptr_eq(&client.tls_root_store, &roots)
        {
            return client.clone();
        }
        let client = Self::new(proxy, allow_local, roots);
        *slot = Some(client.clone());
        client
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
        let proxy = request_context.as_ref().and_then(|ctx| {
            self.proxy_config.proxy_for_target(
                ctx.protocol.is_secure(),
                &ctx.authority.host.to_string(),
                ctx.host_with_port().port,
            )
        });
        match proxy.as_ref() {
            Some(proxy) => info!(
                "HTTP upstream route selected (target={authority}, route=upstream_proxy, proxy={})",
                proxy.address
            ),
            None => info!("HTTP upstream route selected (target={authority}, route=direct)"),
        }
        if let Some(proxy) = proxy {
            req.extensions_mut().insert(proxy);
        } else if req.extensions().contains::<ProxyAddress>() {
            // Rama's extension store is append-only. Reject an inherited proxy route
            // rather than letting it override the client's direct-routing policy.
            return Err(OpaqueError::from_display(
                "direct upstream request contains a proxy route",
            ));
        }

        let key = pool::PoolKey {
            origin: request_context
                .as_ref()
                .map(|ctx| format!("{}://{authority}", ctx.protocol))
                .unwrap_or_default(),
            local_target: req.extensions().get::<LocalTarget>().copied(),
        };
        let can_reuse = pool::reusable(req.version(), req.headers()) && request_context.is_some();
        // Rama may retain request extensions in connection state. Keep only
        // transport metadata, never execution attribution or our pool's owner.
        {
            let original = std::mem::take(req.extensions_mut());
            if let Some(target) = original.get::<LocalTarget>() {
                req.extensions_mut().insert(*target);
            }
            if let Some(proxy) = original.get::<ProxyAddress>() {
                req.extensions_mut().insert(proxy.clone());
            }
            if let Some(executor) = original.get::<rama_core::rt::Executor>() {
                req.extensions_mut().insert(executor.clone());
            }
            if let Some(context) = request_context {
                req.extensions_mut().insert(context);
            }
        }
        let connect_started_at = Instant::now();
        let reused = can_reuse.then(|| self.pool.take(&key)).flatten();
        let (mut req, http_connection) = if let Some(connection) = reused {
            adapt_request_version(&mut req, connection.version)?;
            (req, connection.connection)
        } else {
            let EstablishedClientConnection {
                input: req,
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
            (req, http_connection)
        };

        req.extensions_mut()
            .extend(http_connection.extensions().clone());

        let version = req.version();
        let request_started_at = Instant::now();
        match http_connection.serve(req).await {
            Ok(resp) => {
                info!(
                    "HTTP upstream response headers received (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                if can_reuse
                    && pool::reusable(resp.version(), resp.headers())
                    && resp.status() != rama_http::StatusCode::SWITCHING_PROTOCOLS
                {
                    Ok(resp.map(|body| {
                        self.pool.body(
                            body,
                            pool::IdleConnection {
                                key,
                                connection: http_connection,
                                version,
                                idle_since: Instant::now(),
                            },
                        )
                    }))
                } else {
                    Ok(resp)
                }
            }
            Err(err) => {
                warn!(
                    "HTTP upstream response headers failed (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Err(OpaqueError::from_boxed(err)
                    .context(format!("http request failure for upstream: {authority}")))
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
    let tls = RequestVersionAdapter::new(tls).boxed();
    let tls = rama_core::service::service_fn(move |req: Request<Body>| {
        let tls = tls.clone();
        async move {
            let EstablishedClientConnection { input, conn } = tls.serve(req).await?;
            Ok::<_, BoxError>(EstablishedClientConnection {
                input,
                conn: pool::LiveStream::new(conn),
            })
        }
    });
    let connector = HttpConnector::new(tls);
    connector.boxed()
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
