use crate::config::OtelTlsConfig;
use codex_http_client::BlockingHttpClient;
use codex_http_client::BlockingHttpClientBuilder;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use http::HeaderMap;
use http::Uri;
use http::header::HeaderName;
use http::header::HeaderValue;
use opentelemetry_http::Bytes;
use opentelemetry_http::HttpClient as OtelHttpClient;
use opentelemetry_http::HttpError as OtelHttpError;
use opentelemetry_http::Request;
use opentelemetry_http::Response;
use opentelemetry_otlp::OTEL_EXPORTER_OTLP_TIMEOUT;
use opentelemetry_otlp::OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT;
use opentelemetry_otlp::tonic_types::transport::Certificate as TonicCertificate;
use opentelemetry_otlp::tonic_types::transport::ClientTlsConfig;
use opentelemetry_otlp::tonic_types::transport::Identity as TonicIdentity;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

pub(crate) fn build_header_map(
    headers: &std::collections::HashMap<String, String>,
) -> Result<HeaderMap, Box<dyn Error>> {
    let mut header_map = HeaderMap::new();
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| config_error(format!("invalid OTLP header name {key:?}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| config_error(format!("invalid OTLP header value for {key:?}")))?;
        if header_map.insert(name, value).is_some() {
            return Err(config_error(format!("duplicate OTLP header name {key:?}")));
        }
    }
    Ok(header_map)
}

/// TLS configuration shared by the OTLP gRPC exporters.
///
/// tonic uses the process-wide rustls provider rather than selecting a second
/// provider through exporter features. Install the shared provider first; it
/// supports ECDSA P-521 and respects an embedding host's existing provider.
pub(crate) fn grpc_tls_config(
    endpoint: &str,
    tls: Option<&OtelTlsConfig>,
) -> Result<ClientTlsConfig, Box<dyn Error>> {
    ensure_rustls_crypto_provider();
    let tls_config = ClientTlsConfig::new()
        .with_enabled_roots()
        .assume_http2(true);
    match tls {
        Some(tls) => build_grpc_tls_config(endpoint, tls_config, tls),
        None => Ok(tls_config),
    }
}

fn build_grpc_tls_config(
    endpoint: &str,
    tls_config: ClientTlsConfig,
    tls: &OtelTlsConfig,
) -> Result<ClientTlsConfig, Box<dyn Error>> {
    let uri: Uri = endpoint.parse()?;
    let host = uri.host().ok_or_else(|| {
        config_error(format!(
            "OTLP gRPC endpoint {endpoint} does not include a host"
        ))
    })?;

    let mut config = tls_config.domain_name(host.to_owned());

    if let Some(path) = tls.ca_certificate.as_ref() {
        let (pem, _) = read_bytes(path)?;
        config = config.ca_certificate(TonicCertificate::from_pem(pem));
    }

    match (&tls.client_certificate, &tls.client_private_key) {
        (Some(cert_path), Some(key_path)) => {
            let (cert_pem, _) = read_bytes(cert_path)?;
            let (key_pem, _) = read_bytes(key_path)?;
            config = config.identity(TonicIdentity::from_pem(cert_pem, key_pem));
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(config_error(
                "client_certificate and client_private_key must both be provided for mTLS",
            ));
        }
        (None, None) => {}
    }

    Ok(config)
}

/// Build a blocking HTTP client with TLS configuration for OTLP HTTP exporters.
///
/// We use the shared blocking facade because OTEL exporters run on dedicated
/// OS threads that are not necessarily backed by tokio.
pub(crate) fn build_http_client(
    tls: &OtelTlsConfig,
    timeout_var: &str,
) -> Result<OtlpBlockingHttpClient, Box<dyn Error>> {
    if current_tokio_runtime_is_multi_thread() {
        tokio::task::block_in_place(|| build_http_client_inner(tls, timeout_var))
    } else if tokio::runtime::Handle::try_current().is_ok() {
        let tls = tls.clone();
        let timeout_var = timeout_var.to_string();
        std::thread::spawn(move || {
            build_http_client_inner(&tls, &timeout_var).map_err(|err| err.to_string())
        })
        .join()
        .map_err(|_| config_error("failed to join OTLP blocking HTTP client builder thread"))?
        .map_err(config_error)
    } else {
        build_http_client_inner(tls, timeout_var)
    }
}

pub(crate) fn current_tokio_runtime_is_multi_thread() -> bool {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread,
        Err(_) => false,
    }
}

fn build_http_client_inner(
    tls: &OtelTlsConfig,
    timeout_var: &str,
) -> Result<OtlpBlockingHttpClient, Box<dyn Error>> {
    let timeout = resolve_otlp_timeout(timeout_var);
    let mut builder = BlockingHttpClientBuilder::new().timeout(timeout);
    let HttpTlsMaterial {
        ca_certificate,
        client_identity,
    } = load_http_tls_material(tls)?;

    if let Some(PemMaterial { bytes, location }) = ca_certificate {
        builder = builder.tls_certs_only_pem(&bytes).map_err(|error| {
            config_error(format!(
                "failed to parse certificate {}: {error}",
                location.display()
            ))
        })?;
    }
    if let Some(ClientIdentityPem {
        bytes,
        certificate_location,
        key_location,
    }) = client_identity
    {
        builder = builder.identity_pem(&bytes).map_err(|error| {
            config_error(format!(
                "failed to parse client identity using {} and {}: {error}",
                certificate_location.display(),
                key_location.display()
            ))
        })?;
        builder = builder.https_only(true);
    }

    builder
        .build_with_transport_default_proxy()
        .map(|client| OtlpBlockingHttpClient(client, timeout))
        .map_err(|error| Box::new(error) as Box<dyn Error>)
}

pub(crate) fn build_async_http_client(
    tls: Option<&OtelTlsConfig>,
    timeout_var: &str,
) -> Result<OtlpAsyncHttpClient, Box<dyn Error>> {
    let mut builder = HttpClientBuilder::new().timeout(resolve_otlp_timeout(timeout_var));

    if let Some(tls) = tls {
        let HttpTlsMaterial {
            ca_certificate,
            client_identity,
        } = load_http_tls_material(tls)?;
        if let Some(PemMaterial { bytes, location }) = ca_certificate {
            builder = builder.tls_certs_only_pem(&bytes).map_err(|error| {
                config_error(format!(
                    "failed to parse certificate {}: {error}",
                    location.display()
                ))
            })?;
        }
        if let Some(ClientIdentityPem {
            bytes,
            certificate_location,
            key_location,
        }) = client_identity
        {
            builder = builder.identity_pem(&bytes).map_err(|error| {
                config_error(format!(
                    "failed to parse client identity using {} and {}: {error}",
                    certificate_location.display(),
                    key_location.display()
                ))
            })?;
            builder = builder.https_only(true);
        }
    }

    builder
        .build_with_transport_default_proxy()
        .map(OtlpAsyncHttpClient)
        .map_err(|error| Box::new(error) as Box<dyn Error>)
}

#[derive(Clone, Debug)]
pub(crate) struct OtlpAsyncHttpClient(HttpClient);

pub(crate) struct OtlpBlockingHttpClient(BlockingHttpClient, Duration);

const MAX_OTLP_RESPONSE_BODY_BYTES: usize = 4 * 1024 * 1024;

impl fmt::Debug for OtlpBlockingHttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OtlpBlockingHttpClient")
    }
}

impl OtelHttpClient for OtlpAsyncHttpClient {
    fn send_bytes<'life0, 'async_trait>(
        &'life0 self,
        request: Request<Bytes>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Bytes>, OtelHttpError>> + Send + 'async_trait>>
    where
        Self: 'async_trait,
        'life0: 'async_trait,
    {
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let mut response = self
                .0
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            let status = response.status();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if chunk.len() > MAX_OTLP_RESPONSE_BODY_BYTES - body.len() {
                    return Err(
                        Box::new(opentelemetry_http::ResponseBodyTooLarge::new()) as OtelHttpError
                    );
                }
                body.extend_from_slice(&chunk);
            }
            let mut response = Response::builder().status(status).body(Bytes::from(body))?;
            *response.headers_mut() = headers;
            Ok(response)
        })
    }
}

impl OtelHttpClient for OtlpBlockingHttpClient {
    fn send_bytes<'life0, 'async_trait>(
        &'life0 self,
        request: Request<Bytes>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Bytes>, OtelHttpError>> + Send + 'async_trait>>
    where
        Self: 'async_trait,
        'life0: 'async_trait,
    {
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let mut response = self
                .0
                .request(parts.method, parts.uri.to_string())
                // Preserve one async-body deadline across repeated blocking reads.
                .timeout(self.1)
                .headers(parts.headers)
                .body_bytes(body)
                .send()?;
            let status = response.status();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                let count = std::io::Read::read(&mut response, &mut buffer)?;
                if count == 0 {
                    break;
                }
                if count > MAX_OTLP_RESPONSE_BODY_BYTES - body.len() {
                    return Err(
                        Box::new(opentelemetry_http::ResponseBodyTooLarge::new()) as OtelHttpError
                    );
                }
                body.extend_from_slice(&buffer[..count]);
            }
            let mut response = Response::builder().status(status).body(Bytes::from(body))?;
            *response.headers_mut() = headers;
            Ok(response)
        })
    }
}

struct HttpTlsMaterial {
    ca_certificate: Option<PemMaterial>,
    client_identity: Option<ClientIdentityPem>,
}

struct PemMaterial {
    bytes: Vec<u8>,
    location: PathBuf,
}

struct ClientIdentityPem {
    bytes: Vec<u8>,
    certificate_location: PathBuf,
    key_location: PathBuf,
}

fn load_http_tls_material(tls: &OtelTlsConfig) -> Result<HttpTlsMaterial, Box<dyn Error>> {
    let ca_certificate = tls
        .ca_certificate
        .as_ref()
        .map(|path| {
            let (bytes, location) = read_bytes(path)?;
            Ok::<_, Box<dyn Error>>(PemMaterial { bytes, location })
        })
        .transpose()?;

    let client_identity = match (&tls.client_certificate, &tls.client_private_key) {
        (Some(cert_path), Some(key_path)) => {
            let (mut cert_pem, cert_location) = read_bytes(cert_path)?;
            let (key_pem, key_location) = read_bytes(key_path)?;
            // Separate independently valid PEM files even when the certificate's
            // final END boundary has no trailing newline.
            cert_pem.push(b'\n');
            cert_pem.extend_from_slice(key_pem.as_slice());
            Some(ClientIdentityPem {
                bytes: cert_pem,
                certificate_location: cert_location,
                key_location,
            })
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(config_error(
                "client_certificate and client_private_key must both be provided for mTLS",
            ));
        }
        (None, None) => None,
    };

    Ok(HttpTlsMaterial {
        ca_certificate,
        client_identity,
    })
}

pub(crate) fn resolve_otlp_timeout(signal_var: &str) -> Duration {
    if let Some(timeout) = read_timeout_env(signal_var) {
        return timeout;
    }
    if let Some(timeout) = read_timeout_env(OTEL_EXPORTER_OTLP_TIMEOUT) {
        return timeout;
    }
    OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT
}

fn read_timeout_env(var: &str) -> Option<Duration> {
    let value = env::var(var).ok()?;
    let parsed = value.parse::<i64>().ok()?;
    if parsed < 0 {
        return None;
    }
    Some(Duration::from_millis(parsed as u64))
}

fn read_bytes(path: &AbsolutePathBuf) -> Result<(Vec<u8>, PathBuf), Box<dyn Error>> {
    match fs::read(path) {
        Ok(bytes) => Ok((bytes, path.to_path_buf())),
        Err(error) => Err(Box::new(io::Error::new(
            error.kind(),
            format!("failed to read {}: {error}", path.display()),
        ))),
    }
}

fn config_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(ErrorKind::InvalidData, message.into()))
}

