use crate::config::OtelTlsConfig;
use codex_http_client::BlockingHttpClient;
use codex_http_client::BlockingHttpClientBuilder;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientBuilder;
use codex_utils_absolute_path::AbsolutePathBuf;
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

pub(crate) fn build_header_map(headers: &std::collections::HashMap<String, String>) -> HeaderMap {
    let mut header_map = HeaderMap::new();
    for (key, value) in headers {
        if let Ok(name) = HeaderName::from_bytes(key.as_bytes())
            && let Ok(val) = HeaderValue::from_str(value)
        {
            header_map.insert(name, val);
        }
    }
    header_map
}

pub(crate) fn build_grpc_tls_config(
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
    let mut builder = BlockingHttpClientBuilder::new().timeout(resolve_otlp_timeout(timeout_var));
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
        .map(OtlpBlockingHttpClient)
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

pub(crate) struct OtlpBlockingHttpClient(BlockingHttpClient);

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
            let response = self
                .0
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body)
                .send()
                .await?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("request failed with status {status}").into());
            }
            let headers = response.headers().clone();
            let body = response.bytes().await?;
            let mut response = Response::builder().status(status).body(body)?;
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
            let response = self
                .0
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(body.to_vec())
                .send()?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!("request failed with status {status}").into());
            }
            let headers = response.headers().clone();
            let body = response.bytes()?;
            let mut response = Response::builder().status(status).body(body)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use tokio::runtime::Builder;

    fn spawn_http_response_server() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener.local_addr().expect("test listener address");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("test request should connect");
            let mut request = [0_u8; 4096];
            let bytes_read = stream.read(&mut request).expect("test request should read");
            let request = String::from_utf8_lossy(&request[..bytes_read]);
            assert!(request.starts_with("POST /v1/traces HTTP/1.1"));
            assert!(request.to_ascii_lowercase().contains("x-otel-test: shared"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nx-otel-response: ok\r\n\r\nok")
                .expect("test response should write");
        });
        (format!("http://{address}/v1/traces"), server)
    }

    #[tokio::test]
    async fn async_otlp_adapter_sends_through_shared_client() {
        let (url, server) = spawn_http_response_server();
        let client = OtlpAsyncHttpClient(
            HttpClientBuilder::new()
                .build_direct()
                .expect("shared async client"),
        );
        let request = Request::post(url)
            .header("x-otel-test", "shared")
            .body(Bytes::from_static(b"payload"))
            .expect("OTLP request");

        let response = client.send_bytes(request).await.expect("OTLP response");

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.headers()["x-otel-response"], "ok");
        assert_eq!(response.body(), &Bytes::from_static(b"ok"));
        server.join().expect("test server should finish");
    }

    #[tokio::test]
    async fn blocking_otlp_adapter_sends_through_shared_client() {
        let (url, server) = spawn_http_response_server();
        let client = OtlpBlockingHttpClient(
            BlockingHttpClientBuilder::new()
                .build_direct()
                .expect("shared blocking client"),
        );
        let request = Request::post(url)
            .header("x-otel-test", "shared")
            .body(Bytes::from_static(b"payload"))
            .expect("OTLP request");

        let response = client.send_bytes(request).await.expect("OTLP response");

        assert_eq!(response.status(), http::StatusCode::OK);
        assert_eq!(response.headers()["x-otel-response"], "ok");
        assert_eq!(response.body(), &Bytes::from_static(b"ok"));
        server.join().expect("test server should finish");
    }

    #[test]
    fn current_tokio_runtime_is_multi_thread_detects_runtime_flavor() {
        assert!(!current_tokio_runtime_is_multi_thread());

        let current_thread_runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        assert_eq!(
            current_thread_runtime.block_on(async { current_tokio_runtime_is_multi_thread() }),
            false
        );

        let multi_thread_runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("multi-thread runtime");
        assert_eq!(
            multi_thread_runtime.block_on(async { current_tokio_runtime_is_multi_thread() }),
            true
        );
    }

    #[test]
    fn build_http_client_works_in_current_thread_runtime() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");

        let client = runtime.block_on(async {
            build_http_client(&OtelTlsConfig::default(), OTEL_EXPORTER_OTLP_TIMEOUT)
        });

        assert!(client.is_ok());
    }

    #[test]
    fn build_http_client_accepts_custom_ca_certificate() {
        let ca_certificate = AbsolutePathBuf::try_from(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../http-client/tests/fixtures/test-ca.pem"),
        )
        .expect("absolute CA certificate path");
        let tls = OtelTlsConfig {
            ca_certificate: Some(ca_certificate),
            ..OtelTlsConfig::default()
        };

        let client = build_http_client(&tls, OTEL_EXPORTER_OTLP_TIMEOUT);

        assert!(client.is_ok());
    }

    #[test]
    fn build_async_http_client_accepts_custom_ca_certificate() {
        let ca_certificate = AbsolutePathBuf::try_from(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../http-client/tests/fixtures/test-ca.pem"),
        )
        .expect("absolute CA certificate path");
        let tls = OtelTlsConfig {
            ca_certificate: Some(ca_certificate),
            ..OtelTlsConfig::default()
        };

        let client = build_async_http_client(Some(&tls), OTEL_EXPORTER_OTLP_TIMEOUT);

        assert!(client.is_ok());
    }

    #[test]
    fn http_tls_material_rejects_unpaired_client_credentials() {
        let client_certificate = AbsolutePathBuf::try_from(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("unused-client-cert.pem"),
        )
        .expect("absolute client certificate path");
        let tls = OtelTlsConfig {
            client_certificate: Some(client_certificate),
            ..OtelTlsConfig::default()
        };

        let error = load_http_tls_material(&tls)
            .err()
            .expect("unpaired client credentials should fail");

        assert_eq!(
            error.to_string(),
            "client_certificate and client_private_key must both be provided for mTLS"
        );
    }
}
