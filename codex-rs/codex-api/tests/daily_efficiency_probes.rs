//! Local regression probes for the codex-api efficiency fixes. No upstream requests.
//! Run with `cargo test -p codex-api --test daily_efficiency_probes -- --nocapture --test-threads=1`.
#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use bytes::Bytes;
use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::OpenAiFileError;
use codex_api::Provider;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponsesClient;
use codex_api::ResponsesWebsocketClient;
use codex_api::ResponsesWsRequest;
use codex_api::RetryConfig;
use codex_api::upload_openai_file;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::ReqwestTransport;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _: &mut HeaderMap) {}
}

fn provider(base_url: String) -> Provider {
    Provider {
        name: "local-efficiency-probe".into(),
        base_url,
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_retries: 0,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_millis(100),
    }
}

fn factory() -> HttpClientFactory {
    HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
}

async fn read_headers(stream: &mut TcpStream) -> (usize, usize) {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let n = stream.read(&mut chunk).await.expect("request bytes");
        assert_ne!(n, 0, "client closed before headers");
        request.extend_from_slice(&chunk[..n]);
        if let Some(end) = request.windows(4).position(|x| x == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&request[..end]).expect("HTTP headers");
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("request content length");
            return (length, request.len() - end - 4);
        }
        assert!(request.len() < 64 * 1024, "bounded test headers");
    }
}

async fn http_sample(header_delay: Duration, slow_stream: bool) -> f64 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (length, mut received) = read_headers(&mut socket).await;
        while received < length {
            let mut chunk = [0_u8; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            received += n;
        }
        tokio::time::sleep(header_delay).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        if slow_stream {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(40)).await;
                socket
                    .write_all(
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
                    )
                    .await
                    .unwrap();
            }
        }
        socket
            .write_all(
                b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"probe-done\"}}\n\n",
            )
            .await
            .unwrap();
    });
    let transport =
        ReqwestTransport::from_http_client(HttpClientBuilder::new().build_direct().unwrap());
    let client = ResponsesClient::new(
        transport,
        provider(format!("http://{address}")),
        Arc::new(NoAuth),
    );
    let started = Instant::now();
    let request = client.stream(
        json!({"model":"probe"}),
        HeaderMap::new(),
        Compression::None,
        None,
    );
    let result = tokio::time::timeout(Duration::from_secs(3), request).await;
    if !header_delay.is_zero() {
        assert!(
            matches!(
                result,
                Ok(Err(codex_api::ApiError::Transport(
                    codex_api::TransportError::Timeout
                )))
            ),
            "header deadline must reject a stalled establishment"
        );
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        return elapsed;
    }
    let mut response = result.expect("request establishment").expect("HTTP stream");
    let mut completions = 0;
    let mut deltas = 0;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(3), response.next())
        .await
        .unwrap()
    {
        match event.expect("valid SSE event") {
            ResponseEvent::Completed { response_id, .. } => {
                assert_eq!(response_id, "probe-done");
                completions += 1;
            }
            ResponseEvent::OutputTextDelta(text) => {
                assert_eq!(text, "x");
                deltas += 1;
            }
            _ => {}
        }
    }
    server.await.unwrap();
    assert_eq!(completions, 1);
    assert_eq!(deltas, if slow_stream { 5 } else { 0 });
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    if slow_stream {
        assert!(
            elapsed >= 200.0,
            "healthy streaming must outlive the establishment deadline"
        );
    }
    elapsed
}

#[tokio::test]
async fn probe_http_establishment_deadline() {
    let mut timeouts = Vec::new();
    for _ in 0..5 {
        timeouts.push(http_sample(Duration::from_millis(350), false).await);
    }
    let healthy = http_sample(Duration::ZERO, false).await;
    let healthy_stream = http_sample(Duration::ZERO, true).await;
    println!(
        "PROBE {}",
        json!({"case":"http_establishment", "configured_idle_ms":100,
        "server_header_delay_ms":350, "establishment_timeout_ms":timeouts,
        "healthy_ms":healthy,
        "healthy_stream_longer_than_guard_ms":healthy_stream})
    );
}

#[tokio::test]
async fn probe_websocket_handshake_etag_replay() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, held) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut config = tungstenite::protocol::WebSocketConfig::default();
        config.extensions.permessage_deflate =
            Some(tungstenite::extensions::compression::deflate::DeflateConfig::default());
        let mut socket = tokio_tungstenite::accept_hdr_async_with_config(socket,
            |_: &tokio_tungstenite::tungstenite::handshake::server::Request,
             mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                response.headers_mut().insert("x-models-etag", "handshake-A".parse().unwrap());
                Ok(response)
            }, Some(config)).await.unwrap();
        for index in 0..5 {
            let request = socket.next().await.unwrap().unwrap();
            assert!(matches!(request, Message::Text(_)));
            socket
                .send(Message::Text(
                    json!({"type":"response.completed",
                "response":{"id":format!("probe-{index}")}})
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
        }
        let _ = held.await;
    });
    let connection =
        ResponsesWebsocketClient::new(provider(format!("http://{address}")), Arc::new(NoAuth))
            .connect(&factory(), HeaderMap::new(), HeaderMap::new(), None, None)
            .await
            .unwrap();
    let mut etags_per_request = Vec::new();
    for index in 0..5 {
        let request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            model: "probe".into(),
            instructions: String::new(),
            previous_response_id: None,
            input: Vec::new().into(),
            tools: None,
            tool_choice: "auto".into(),
            parallel_tool_calls: true,
            reasoning: None,
            store: false,
            stream: true,
            stream_options: None,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: None,
            text: None,
            generate: Some(false),
            client_metadata: None,
        });
        let mut response = connection
            .stream_request(request, index > 0, None)
            .await
            .unwrap();
        let mut etags = 0;
        let mut completed = 0;
        while let Some(event) = tokio::time::timeout(Duration::from_secs(3), response.next())
            .await
            .unwrap()
        {
            match event.unwrap() {
                ResponseEvent::ModelsEtag(etag) => {
                    assert_eq!(etag, "handshake-A");
                    etags += 1;
                }
                ResponseEvent::Completed { response_id, .. } => {
                    assert_eq!(response_id, format!("probe-{index}"));
                    completed += 1;
                }
                _ => {}
            }
        }
        assert_eq!(completed, 1);
        etags_per_request.push(etags);
    }
    assert_eq!(etags_per_request, vec![1, 0, 0, 0, 0]);
    drop(connection);
    drop(release);
    server.await.unwrap();
    println!(
        "PROBE {}",
        json!({"case":"websocket_metadata", "connections":1,
        "requests":5, "handshake_etag_events_per_request":etags_per_request})
    );
}

#[tokio::test]
async fn probe_upload_stall_deadline() {
    for mib in [100_u64, 512] {
        let wall_started = Instant::now();
        let control = MockServer::start().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upload_url = format!("http://{}/blob", listener.local_addr().unwrap());
        Mock::given(method("POST"))
            .and(path("/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"file_id":"probe-file", "upload_url":upload_url})),
            )
            .expect(1)
            .mount(&control)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/files/probe-file"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&control)
            .await;
        let (stalled, observed_stall) = oneshot::channel();
        let (release, held) = oneshot::channel::<()>();
        let size = mib * 1024 * 1024;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (length, received) = read_headers(&mut socket).await;
            assert_eq!(length as u64, size);
            stalled.send(received).unwrap();
            // Keep a real TCP connection open without draining the request body.
            let _ = held.await;
            drop(socket);
        });
        let base_url = control.uri();
        let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
        let upload = tokio::spawn(async move {
            upload_openai_file(
                &base_url,
                &NoAuth,
                &factory(),
                "probe.bin".into(),
                size,
                futures::stream::iter((0..mib).map(move |_| Ok(chunk.clone()))),
            )
            .await
        });
        let received = tokio::time::timeout(Duration::from_secs(5), observed_stall)
            .await
            .unwrap()
            .unwrap();
        assert!((received as u64) < size);
        // Let the local send buffers fill before advancing the idle clock.
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Prevent paused time from auto-advancing while real network I/O completes.
        tokio::time::pause();
        let clock_keeper = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        tokio::time::advance(Duration::from_secs(59)).await;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert!(
            !upload.is_finished(),
            "an upload must retain its full inactivity allowance"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        let delivery_started = Instant::now();
        while !upload.is_finished() {
            assert!(
                delivery_started.elapsed() < Duration::from_secs(5),
                "timeout and rollback must finish"
            );
            tokio::task::yield_now().await;
        }
        let result = upload.await.unwrap();
        tokio::time::resume();
        clock_keeper.abort();
        assert!(clock_keeper.await.unwrap_err().is_cancelled());
        assert!(
            matches!(result, Err(OpenAiFileError::Request { source, .. }) if source.is_timeout())
        );
        drop(release);
        server.await.unwrap();
        let requests = control.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one create and one rollback; no finalize or replay"
        );
        assert_eq!(requests[1].method.as_str(), "DELETE");
        println!(
            "PROBE {}",
            json!({"case":"upload_stall", "declared_mib":mib,
            "still_pending_after_virtual_seconds":59, "idle_timeout_seconds":60,
            "timeout_and_rollback_delivered_by_virtual_seconds":61,
            "wall_ms":wall_started.elapsed().as_secs_f64()*1000.0, "rollback_requests":1})
        );
    }
}

#[tokio::test]
async fn probe_upload_progress_control() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "file_id":"healthy-file", "upload_url":format!("{}/blob", server.uri())})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/blob"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/files/healthy-file/uploaded"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status":"success", "download_url":format!("{}/download", server.uri())})))
        .expect(1)
        .mount(&server)
        .await;
    let contents = futures::stream::unfold(0, |index| async move {
        if index == 16 {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        Some((Ok(Bytes::from(vec![b'x'; 32 * 1024])), index + 1))
    });
    let started = Instant::now();
    let uploaded = tokio::time::timeout(
        Duration::from_secs(5),
        upload_openai_file(
            &server.uri(),
            &NoAuth,
            &factory(),
            "healthy.bin".into(),
            512 * 1024,
            contents,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(elapsed_ms >= 400.0);
    assert_eq!(uploaded.file_id, "healthy-file");
    assert_eq!(uploaded.file_size_bytes, 512 * 1024);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        3,
        "create, PUT, finalize; no rollback or replay"
    );
    assert_eq!(requests[1].body.len(), 512 * 1024);
    println!(
        "PROBE {}",
        json!({"case":"upload_progress_control", "wall_ms":elapsed_ms,
        "chunks":16, "inter_chunk_delay_ms":25, "uploaded_bytes":uploaded.file_size_bytes})
    );
}
