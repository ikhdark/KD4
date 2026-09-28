use super::*;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

#[tokio::test]
async fn enabled_request_logging_emits_transport_url_and_body() {
    let logs = capture_transport_logs(HttpClient::new(test_reqwest_client())).await;

    assert!(logs.contains("log capture sentinel"));
    assert!(logs.contains("url-secret"));
    assert!(logs.contains("body-secret"));
}

#[tokio::test]
async fn disabled_request_logging_suppresses_transport_url_and_body() {
    let logs = capture_transport_logs(HttpClient::new_without_request_logging(
        test_reqwest_client(),
    ))
    .await;

    assert!(logs.contains("log capture sentinel"));
    assert!(!logs.contains("url-secret"));
    assert!(!logs.contains("body-secret"));
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
    let TransportError::Network(message) = error else {
        panic!("expected network error: {error}");
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
