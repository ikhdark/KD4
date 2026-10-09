use super::*;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[tokio::test]
async fn transport_timing_scopes_are_isolated_and_cancel_safe() {
    let first = capture_transport_timing(async {
        record_response_header_time(Duration::from_secs(2));
        tokio::task::yield_now().await;
    });
    let second = capture_transport_timing(async {
        tokio::task::yield_now().await;
        record_response_header_time(Duration::from_secs(7));
    });
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.1.response_headers, Some(Duration::from_secs(2)));
    assert_eq!(second.1.response_headers, Some(Duration::from_secs(7)));
    let cancelled = tokio::time::timeout(Duration::from_millis(1), capture_transport_timing(async {
        record_response_header_time(Duration::from_secs(99));
        std::future::pending::<()>().await;
    })).await;
    assert!(cancelled.is_err());
    assert!(capture_transport_timing(async {}).await.1.response_headers.is_none());
}

#[tokio::test]
async fn request_logging_obeys_policy_without_exposing_body() {
    for enabled in [true, false] {
        let client = if enabled {
            HttpClient::new(test_reqwest_client())
        } else {
            HttpClient::new_without_request_logging(test_reqwest_client())
        };
        let logs = capture_transport_logs(client).await;

        assert!(logs.contains("log capture sentinel"));
        assert_eq!(logs.contains("url-secret"), enabled);
        assert_eq!(logs.contains("<JSON body: 23 bytes>"), enabled);
        assert!(!logs.contains("body-secret"));
    }
}

#[test]
fn request_body_trace_contains_only_sizes_for_all_body_representations() {
    let request = Request::new(Method::POST, "https://example.com".into())
        .with_json(&json!({"token": "body-secret"}));
    assert_eq!(request_body_for_trace(&request), "<JSON body: 23 bytes>");
    let encoded = request.clone().into_prepared().unwrap();
    assert_eq!(request_body_for_trace(&encoded), "<encoded JSON body: 23 bytes>");
    let compressed = request
        .with_compression(crate::request::RequestCompression::Zstd)
        .into_prepared()
        .unwrap();
    assert_eq!(
        request_body_for_trace(&compressed),
        format!("<encoded JSON body: {} bytes>", compressed.prepared_body_len().unwrap())
    );
    let mut raw = Request::new(Method::POST, "https://example.com".into())
        .with_raw_body("body-secret");
    assert_eq!(request_body_for_trace(&raw), "<raw body: 11 bytes>");
    raw.body = Some(RequestBody::InvalidJson("body-secret".into()));
    assert_eq!(request_body_for_trace(&raw), "<invalid JSON body>");
    raw.body = None;
    assert_eq!(request_body_for_trace(&raw), "");
}

#[tokio::test]
async fn connection_failures_are_classified_without_exposing_request_urls() {
    let unavailable_server =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("server port should bind");
    let server_addr = unavailable_server
        .local_addr()
        .expect("server listener should have an address");
    drop(unavailable_server);
    // Windows may retry a refused connection past the common two-second test deadline.
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build connection-failure client");
    let transport = ReqwestTransport::from_http_client(HttpClient::new(client));
    let request = Request::new(
        Method::POST,
        format!("http://{server_addr}/responses?token=url-secret"),
    );

    let error = match transport.stream(request).await {
        Err(TransportError::Connection(error)) => error,
        Err(error) => panic!("expected a connection failure, got {error}"),
        Ok(_) => panic!("an unavailable server should not return a response"),
    };
    assert!(!error.to_string().contains("url-secret"));
}

#[tokio::test]
async fn permanent_certificate_failure_is_not_connection_recovery() {
    codex_utils_rustls_provider::ensure_rustls_crypto_provider();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = rustls::ServerConfig::builder().with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()],
            rustls_pki_types::PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into())).unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "TLS request must arrive");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accept TLS request: {error}"),
            }
        };
        // Accepted Windows sockets can inherit the listener's nonblocking mode.
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        while tls.is_handshaking() {
            if tls.complete_io(&mut socket).is_err() { break; }
        }
    });
    let client = reqwest::Client::builder().no_proxy().use_rustls_tls().timeout(Duration::from_secs(5)).build().unwrap();
    let result = ReqwestTransport::new(client).stream(Request::new(Method::GET,
        format!("https://{address}/?secret=hidden"))).await;
    server.join().unwrap();
    let error = result.err().expect("self-signed certificate must fail");
    assert!(matches!(&error, TransportError::Build(_)), "{error:?}");
    assert!(!error.to_string().contains("hidden"));
}

#[test]
fn remote_tls_alerts_and_socket_outages_remain_recoverable() {
    assert!(!is_permanent_connection_error(&rustls::Error::AlertReceived(
        rustls::AlertDescription::InternalError,
    )));
    assert!(!is_permanent_connection_error(&std::io::Error::from(
        std::io::ErrorKind::ConnectionRefused,
    )));
    assert!(is_permanent_connection_error(&rustls::Error::InvalidCertificate(
        rustls::CertificateError::UnknownIssuer,
    )));
}

#[tokio::test]
async fn invalid_json_body_is_rejected_before_network_dispatch() {
    struct InvalidJson;
    impl serde::Serialize for InvalidJson {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("test JSON serialization failure"))
        }
    }

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
    listener.set_nonblocking(true).expect("set nonblocking");
    let address = listener.local_addr().expect("listener address");
    let transport = ReqwestTransport::from_http_client(HttpClient::new(test_reqwest_client()));
    let mut request =
        Request::new(Method::POST, format!("http://{address}/responses")).with_json(&InvalidJson);
    request.timeout = Some(Duration::from_millis(500));
    assert_eq!(
        request
            .clone()
            .into_prepared()
            .expect_err("preparation must fail"),
        "test JSON serialization failure"
    );
    let error = transport
        .execute(request.clone())
        .await
        .expect_err("execute must fail");
    assert!(
        matches!(error, TransportError::Build(message) if message == "test JSON serialization failure")
    );
    let error = match transport.stream(request).await {
        Err(error) => error,
        Ok(_) => panic!("stream must reject invalid JSON"),
    };
    assert!(
        matches!(error, TransportError::Build(message) if message == "test JSON serialization failure")
    );
    assert_eq!(
        listener
            .accept()
            .expect_err("invalid JSON must never connect")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
}

fn test_reqwest_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("HTTP client should build")
}

#[tokio::test(start_paused = true)]
async fn error_body_deadline_retains_prefix_without_waiting_for_eof() {
    let stream = futures::stream::once(async { Ok(Bytes::from_static(b"diagnostic")) })
        .chain(futures::stream::pending());
    let started = tokio::time::Instant::now();
    let body = collect_error_body(stream).await.expect("diagnostic prefix");
    assert_eq!(started.elapsed(), ERROR_BODY_TIMEOUT);
    assert!(body.starts_with("diagnostic"));
    assert!(body.contains("incomplete"));
}

#[tokio::test]
async fn error_body_bytes_are_bounded_even_when_always_ready() {
    let stream = futures::stream::repeat_with(|| Ok(Bytes::from_static(b"0123456789")));
    let body = collect_error_body(stream).await.expect("diagnostic prefix");
    assert!(body.contains("incomplete"));
    assert!(body.len() < MAX_ERROR_BODY_BYTES + 100);
    assert_eq!(&body[..20], "01234567890123456789");
}

async fn capture_transport_logs(client: HttpClient) -> String {
    let unavailable_server =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("server port should bind");
    let server_addr = unavailable_server
        .local_addr()
        .expect("server listener should have an address");
    drop(unavailable_server);
    let transport = ReqwestTransport::from_http_client(client);
    let log_buffer = Arc::new(Mutex::new(Vec::new()));
    let writer_buffer = Arc::clone(&log_buffer);
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || TestLogWriter(Arc::clone(&writer_buffer)))
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("codex_http_client::transport", tracing::Level::TRACE),
            ),
    );
    let _guard = tracing::subscriber::set_default(subscriber);
    tracing::trace!(target: "codex_http_client::transport", "log capture sentinel");
    let mut request = Request::new(
        Method::POST,
        format!("http://{server_addr}/request?token=url-secret"),
    )
    .with_json(&json!({"token": "body-secret"}));
    request.timeout = Some(Duration::from_secs(1));

    let _ = transport.execute(request).await;

    String::from_utf8(
        log_buffer
            .lock()
            .expect("log buffer should not be poisoned")
            .clone(),
    )
    .expect("captured logs should be UTF-8")
}

#[derive(Clone)]
struct TestLogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for TestLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("log buffer should not be poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn non_connection_errors_redact_request_urls() {
    let client = reqwest::Client::builder().https_only(true).build().unwrap();
    let transport = ReqwestTransport::from_http_client(HttpClient::new(client));
    let error = transport
        .execute(Request::new(
            Method::GET,
            "http://user:password@private.example/secret-path?sig=secret-query".to_string(),
        ))
        .await
        .expect_err("HTTP prohibited");
    let TransportError::Build(message) = error else {
        panic!("expected a permanent configuration error: {error}");
    };
    for secret in ["password", "private.example", "secret-path", "secret-query"] {
        assert!(!message.contains(secret), "leaked URL: {message}");
    }
}

#[tokio::test]
async fn interrupted_error_body_retains_http_status_and_headers() {
    use std::io::Read;
    for streaming in [false, true] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "request must arrive");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Accepted Windows sockets can inherit the listener's nonblocking mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            stream.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 100\r\nRetry-After: 7\r\nConnection: close\r\n\r\npartial").unwrap();
        });
        let transport = ReqwestTransport::from_http_client(HttpClient::new(test_reqwest_client()));
        let request = Request::new(Method::GET, format!("http://{address}/"));
        let error = if streaming {
            match transport.stream(request).await {
                Err(error) => error,
                Ok(_) => panic!("expected HTTP failure"),
            }
        } else {
            transport.execute(request).await.expect_err("HTTP failure")
        };
        let remaining = error
            .retry_after()
            .expect("retry deadline retained")
            .remaining_delay();
        assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(7));
        server.join().unwrap();
        let TransportError::Http {
            status,
            headers,
            body,
            ..
        } = error
        else {
            panic!("lost HTTP metadata: {error}");
        };
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(headers.expect("HTTP headers retained")["retry-after"], "7");
        assert_eq!(body, None);
    }
}

#[tokio::test]
async fn stalled_failure_body_preserves_status_headers_and_retry_after() {
    use std::io::Read;
    for (streaming, status) in [(false, 401), (true, 401), (false, 429), (true, 429)] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (release, held) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            write!(stream, "HTTP/1.1 {status} Failure\r\nContent-Length: 100000\r\nRetry-After: 7\r\nConnection: close\r\n\r\nprefix").unwrap();
            held.recv_timeout(Duration::from_secs(5)).expect("client must finish before server closes");
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let transport = ReqwestTransport::new(client);
        let request = Request::new(Method::GET, format!("http://{address}/"));
        let started = std::time::Instant::now();
        let error = if streaming {
            transport.stream(request).await.err().expect("HTTP failure")
        } else {
            transport.execute(request).await.unwrap_err()
        };
        let elapsed = started.elapsed();
        release.send(()).unwrap();
        server.join().unwrap();
        assert!(elapsed < Duration::from_secs(4), "diagnostic wait: {elapsed:?}");
        assert!(error.retry_after().is_some());
        let TransportError::Http { status: actual, headers, body, .. } = error else {
            panic!("status lost: {error}");
        };
        assert_eq!(actual.as_u16(), status);
        assert_eq!(headers.unwrap()["retry-after"], "7");
        let body = body.unwrap();
        assert!(body.starts_with("prefix"));
        assert!(body.contains("incomplete"));
    }
}
