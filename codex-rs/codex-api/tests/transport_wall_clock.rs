#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Loopback wall-clock benchmarks for the Responses HTTP (SSE) and WebSocket transports.
//!
//! Each benchmark drives the public client through the production reqwest or
//! tungstenite stack over real loopback sockets. A scripted server runs on its own
//! thread and runtime and never contacts a provider, so the numbers isolate local
//! transport and processing overhead from server inference latency. Timing is
//! descriptive rather than asserted, so the benchmarks are ignored by default:
//!
//! cargo test -p codex-api --test transport_wall_clock -- --ignored --nocapture --test-threads=1

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use codex_api::AuthProvider;
use codex_api::Compression;
use codex_api::Provider;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient;
use codex_api::ResponsesOptions;
use codex_api::ResponsesWebsocketClient;
use codex_api::ResponsesWsRequest;
use codex_api::RetryConfig;
use codex_client::ClientRouteClass;
use codex_client::HttpClientFactory;
use codex_client::OutboundProxyPolicy;
use codex_client::ReqwestTransport;
use codex_client::RouteAwareClientPool;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::SinkExt;
use futures::StreamExt;
use http::HeaderMap;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::ReadBuf;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tungstenite::extensions::ExtensionsConfig;
use tungstenite::extensions::compression::deflate::DeflateConfig;
use tungstenite::protocol::WebSocketConfig;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const WARMUP: usize = 2;
const SAMPLES: usize = 9;

#[derive(Clone, Copy, Debug)]
enum Script {
    /// Completion only: isolates request dispatch.
    Complete,
    /// Text deltas separated by idle gaps: per-event latency to a parked client.
    Paced { events: usize, gap: Duration },
    /// Text deltas written back to back: per-event processing throughput.
    Burst { events: usize },
    /// One large output item; SSE writes it as 16 KiB TLS/h2-sized pieces.
    LargeItem { bytes: usize },
}

#[derive(Default)]
struct Observations {
    scripts: VecDeque<Script>,
    /// Instant the complete request was read, and its body bytes on the wire.
    requests: Vec<(Instant, u64)>,
    /// Paced: one mark per flushed event. Burst and large item: write start and end.
    marks: Vec<Instant>,
}

#[derive(Clone, Default)]
struct Server(Arc<Mutex<Observations>>);

impl Server {
    fn script(&self, script: Script) {
        self.0.lock().unwrap().scripts.push_back(script);
    }

    fn request_read(&self, wire_bytes: u64) -> Script {
        let read_at = Instant::now();
        let mut observations = self.0.lock().unwrap();
        observations.requests.push((read_at, wire_bytes));
        observations.marks.clear();
        observations.scripts.pop_front().unwrap_or(Script::Complete)
    }

    fn mark(&self) {
        self.0.lock().unwrap().marks.push(Instant::now());
    }

    fn last_request(&self) -> (Instant, u64) {
        *self.0.lock().unwrap().requests.last().unwrap()
    }

    fn marks(&self) -> Vec<Instant> {
        self.0.lock().unwrap().marks.clone()
    }
}

fn spawn_server<F, Fut>(serve: F) -> SocketAddr
where
    F: Fn(TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            while let Ok((socket, _)) = listener.accept().await {
                socket.set_nodelay(true).unwrap();
                tokio::spawn(serve(socket));
            }
        });
    });
    address
}

/// Counts bytes read from the client so WebSocket results report compressed wire size.
struct Counted {
    inner: TcpStream,
    read: Arc<AtomicU64>,
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        let read = buf.filled().len() - before;
        self.read.fetch_add(read as u64, Ordering::Relaxed);
        poll
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn server_ws_config(deflate: bool) -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    if deflate {
        let mut extensions = ExtensionsConfig::default();
        extensions.permessage_deflate = Some(DeflateConfig::default());
        config.extensions = extensions;
    }
    config
}

/// Accepting or declining permessage-deflate leaves the client configuration untouched.
async fn serve_websocket(socket: TcpStream, server: Server, deflate: bool) {
    let read = Arc::new(AtomicU64::new(0));
    let socket = Counted {
        inner: socket,
        read: Arc::clone(&read),
    };
    let mut ws = tokio_tungstenite::accept_async_with_config(socket, Some(server_ws_config(deflate)))
        .await
        .unwrap();
    let mut consumed = 0;
    while let Some(Ok(message)) = ws.next().await {
        let Message::Text(_) = message else {
            continue;
        };
        let total = read.load(Ordering::Relaxed);
        let script = server.request_read(total - consumed);
        consumed = total;
        match script {
            Script::Complete => {}
            Script::Paced { events, gap } => {
                for index in 0..events {
                    ws.send(Message::Text(delta(index).into())).await.unwrap();
                    server.mark();
                    tokio::time::sleep(gap).await;
                }
            }
            Script::Burst { events } => {
                server.mark();
                for index in 0..events {
                    ws.feed(Message::Text(delta(index).into())).await.unwrap();
                }
                ws.flush().await.unwrap();
                server.mark();
            }
            Script::LargeItem { bytes } => {
                let item = large_item(bytes);
                server.mark();
                ws.send(Message::Text(item.into())).await.unwrap();
                server.mark();
            }
        }
        ws.send(Message::Text(completed().into())).await.unwrap();
    }
}

async fn serve_http(socket: TcpStream, server: Server) {
    let mut socket = BufReader::new(socket);
    let mut line = String::new();
    let mut discard = vec![0; 64 * KIB];
    loop {
        let mut content_length = 0;
        loop {
            line.clear();
            match socket.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut remaining = content_length;
        while remaining > 0 {
            let read = remaining.min(discard.len());
            socket.read_exact(&mut discard[..read]).await.unwrap();
            remaining -= read;
        }
        let script = server.request_read(content_length as u64);
        let stream = socket.get_mut();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        match script {
            Script::Complete => {}
            Script::Paced { events, gap } => {
                for index in 0..events {
                    write_chunk(stream, sse(&delta(index)).as_bytes()).await;
                    server.mark();
                    tokio::time::sleep(gap).await;
                }
            }
            Script::Burst { events } => {
                let mut body = Vec::new();
                for index in 0..events {
                    push_chunk(&mut body, sse(&delta(index)).as_bytes());
                }
                server.mark();
                stream.write_all(&body).await.unwrap();
                server.mark();
            }
            Script::LargeItem { bytes } => {
                let event = sse(&large_item(bytes));
                server.mark();
                for piece in event.as_bytes().chunks(16 * KIB) {
                    write_chunk(stream, piece).await;
                }
                server.mark();
            }
        }
        write_chunk(stream, sse(&completed()).as_bytes()).await;
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    }
}

fn push_chunk(body: &mut Vec<u8>, data: &[u8]) {
    body.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
}

async fn write_chunk(stream: &mut TcpStream, data: &[u8]) {
    let mut chunk = Vec::with_capacity(data.len() + 16);
    push_chunk(&mut chunk, data);
    stream.write_all(&chunk).await.unwrap();
}

fn sse(data: &str) -> String {
    format!("data: {data}\n\n")
}

fn delta(index: usize) -> String {
    json!({"type": "response.output_text.delta", "delta": index.to_string()}).to_string()
}

fn large_item(bytes: usize) -> String {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": corpus_slice(0, bytes)}],
        },
    })
    .to_string()
}

fn completed() -> String {
    json!({"type": "response.completed", "response": {"id": "resp-loopback"}}).to_string()
}

/// Non-repeating workspace source text, so compression ratios resemble real requests.
fn corpus() -> &'static str {
    static CORPUS: OnceLock<String> = OnceLock::new();
    CORPUS.get_or_init(|| {
        fn collect(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect(&path, files);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    files.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut files = Vec::new();
        for crate_dir in ["core", "tui", "app-server", "protocol"] {
            collect(&root.join(crate_dir).join("src"), &mut files);
        }
        files.sort();
        let mut text = String::new();
        for file in files {
            if text.len() >= 8 * MIB {
                break;
            }
            text.push_str(&std::fs::read_to_string(file).unwrap_or_default());
        }
        assert!(text.len() >= 8 * MIB, "benchmark corpus is too small");
        text
    })
}

fn corpus_slice(offset: usize, bytes: usize) -> &'static str {
    let text = corpus();
    let boundary = |mut index: usize| {
        while !text.is_char_boundary(index) {
            index -= 1;
        }
        index
    };
    let start = boundary(offset % (text.len() - bytes));
    &text[start..boundary(start + bytes)]
}

/// Conversation-shaped input of about `bytes` text bytes, in 8 KiB messages.
fn input(bytes: usize, offset: usize) -> Vec<ResponseItem> {
    let text = corpus_slice(offset, bytes);
    let mut items = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut end = rest.len().min(8 * KIB);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        items.push(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: chunk.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
        rest = tail;
    }
    items
}

fn http_request(bytes: usize, offset: usize) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "gpt-loopback".to_string(),
        instructions: String::new(),
        input: input(bytes, offset).into(),
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
    }
}

fn ws_request(bytes: usize, offset: usize) -> ResponsesWsRequest {
    ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
        model: "gpt-loopback".to_string(),
        instructions: String::new(),
        previous_response_id: None,
        input: input(bytes, offset).into(),
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: true,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        generate: None,
        client_metadata: None,
    })
}

struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

fn provider(address: SocketAddr) -> Provider {
    Provider {
        name: "loopback-benchmark".to_string(),
        base_url: format!("http://{address}"),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_retries: 0,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(60),
    }
}

fn http_client(address: SocketAddr) -> ResponsesClient<ReqwestTransport> {
    let pool = RouteAwareClientPool::new(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ClientRouteClass::Api,
    );
    ResponsesClient::new(
        ReqwestTransport::from_client_pool(pool),
        provider(address),
        Arc::new(NoAuth),
    )
}

async fn ws_connection(address: SocketAddr) -> codex_api::ResponsesWebsocketConnection {
    ResponsesWebsocketClient::new(provider(address), Arc::new(NoAuth))
        .connect(
            &HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            HeaderMap::new(),
            HeaderMap::new(),
            None,
            None,
        )
        .await
        .unwrap()
}

#[derive(Clone, Default)]
struct Mark(Arc<OnceLock<Instant>>);

impl Mark {
    fn set(&self) {
        let _ = self.0.set(Instant::now());
    }

    fn get(&self) -> Instant {
        *self.0.get().expect("mark must be set")
    }
}

struct Delivery {
    deltas: Vec<Instant>,
    item: Option<Instant>,
}

/// Collects delivery instants and checks that deltas arrive complete and in order.
async fn consume(mut stream: ResponseStream, expected_deltas: usize) -> Delivery {
    let mut deltas = Vec::with_capacity(expected_deltas);
    let mut item = None;
    let mut completions = 0;
    while let Some(event) = stream.next().await {
        let delivered_at = Instant::now();
        match event.expect("loopback stream must not fail") {
            ResponseEvent::OutputTextDelta(delta) => {
                assert_eq!(delta.parse::<usize>().unwrap(), deltas.len(), "delta order");
                deltas.push(delivered_at);
            }
            ResponseEvent::OutputItemDone(_) => item = Some(delivered_at),
            ResponseEvent::Completed { .. } => completions += 1,
            _ => {}
        }
    }
    assert_eq!(deltas.len(), expected_deltas);
    assert_eq!(completions, 1);
    Delivery { deltas, item }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn summary(mut samples: Vec<Duration>) -> String {
    samples.sort();
    let at = |quantile: f64| {
        let index = ((samples.len() - 1) as f64 * quantile).round() as usize;
        millis(samples[index])
    };
    format!(
        "p50={:.3}ms p90={:.3}ms max={:.3}ms",
        at(0.5),
        at(0.9),
        at(1.0)
    )
}

fn report(line: String) {
    eprintln!("{line}");
    if let Some(path) = std::env::var_os("KD4_TRANSPORT_BENCHMARK_OUTPUT") {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }
}

const DISPATCH_SIZES: [usize; 3] = [64 * KIB, 768 * KIB, 3 * MIB];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "loopback wall-clock benchmark"]
async fn http_dispatch_wall_clock() {
    let server = Server::default();
    let address = spawn_server({
        let server = server.clone();
        move |socket| serve_http(socket, server.clone())
    });
    let client = http_client(address);
    for compression in [Compression::None, Compression::Zstd] {
        for bytes in DISPATCH_SIZES {
            let (mut build, mut to_server, mut to_headers) = (Vec::new(), Vec::new(), Vec::new());
            let mut wire = 0;
            for iteration in 0..WARMUP + SAMPLES {
                let request = http_request(bytes, iteration * 97 * KIB);
                server.script(Script::Complete);
                let dispatched = Mark::default();
                let started = Instant::now();
                let stream = client
                    .stream_request_with_dispatch_ready(
                        &request,
                        ResponsesOptions {
                            compression,
                            ..Default::default()
                        },
                        |_| dispatched.set(),
                    )
                    .await
                    .unwrap();
                let headers_at = Instant::now();
                consume(stream, 0).await;
                let (read_at, wire_bytes) = server.last_request();
                if iteration >= WARMUP {
                    build.push(dispatched.get() - started);
                    to_server.push(read_at - dispatched.get());
                    to_headers.push(headers_at - dispatched.get());
                    wire = wire_bytes;
                }
            }
            report(format!(
                "http dispatch compression={compression:?} json_kib={} wire_kib={}: encode[{}] dispatch->server_read[{}] dispatch->headers[{}]",
                serde_json::to_vec(&http_request(bytes, 0)).unwrap().len() / KIB,
                wire / KIB as u64,
                summary(build),
                summary(to_server),
                summary(to_headers),
            ));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "loopback wall-clock benchmark"]
async fn websocket_dispatch_wall_clock() {
    for deflate in [true, false] {
        let server = Server::default();
        let address = spawn_server({
            let server = server.clone();
            move |socket| serve_websocket(socket, server.clone(), deflate)
        });
        let connection = ws_connection(address).await;
        for bytes in DISPATCH_SIZES {
            let (mut encode, mut queue, mut send, mut to_server) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut wire = 0;
            for iteration in 0..WARMUP + SAMPLES {
                let request = ws_request(bytes, iteration * 97 * KIB);
                server.script(Script::Complete);
                let (queued, dispatched) = (Mark::default(), Mark::default());
                let started = Instant::now();
                let stream = connection
                    .stream_request_with_dispatch_ready(
                        &request,
                        iteration > 0,
                        None,
                        {
                            let queued = queued.clone();
                            move || queued.set()
                        },
                        {
                            let dispatched = dispatched.clone();
                            move |_| dispatched.set()
                        },
                        || {},
                    )
                    .await
                    .unwrap();
                let sent_at = Instant::now();
                consume(stream, 0).await;
                let (read_at, wire_bytes) = server.last_request();
                if iteration >= WARMUP {
                    encode.push(queued.get() - started);
                    queue.push(dispatched.get() - queued.get());
                    send.push(sent_at - dispatched.get());
                    to_server.push(read_at - dispatched.get());
                    wire = wire_bytes;
                }
            }
            report(format!(
                "websocket dispatch deflate={deflate} json_kib={} wire_kib={}: encode[{}] queue[{}] dispatch->send_complete[{}] dispatch->server_read[{}]",
                serde_json::to_vec(&ws_request(bytes, 0)).unwrap().len() / KIB,
                wire / KIB as u64,
                summary(encode),
                summary(queue),
                summary(send),
                summary(to_server),
            ));
        }
    }
}

/// Runs one scripted response and returns server marks with client delivery instants.
async fn scripted<F, Fut>(server: &Server, script: Script, start: F) -> (Vec<Instant>, Delivery)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ResponseStream>,
{
    server.script(script);
    let expected = match script {
        Script::Paced { events, .. } | Script::Burst { events } => events,
        Script::Complete | Script::LargeItem { .. } => 0,
    };
    let delivery = consume(start().await, expected).await;
    (server.marks(), delivery)
}

async fn delivery_benchmarks<F, Fut>(transport: &str, server: &Server, mut start: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ResponseStream>,
{
    // Warm the connection and both code paths.
    scripted(server, Script::Burst { events: 100 }, &mut start).await;

    let (marks, delivery) = scripted(
        server,
        Script::Paced {
            events: 100,
            gap: Duration::from_millis(5),
        },
        &mut start,
    )
    .await;
    let latencies = marks
        .iter()
        .zip(&delivery.deltas)
        .map(|(sent, delivered)| delivered.saturating_duration_since(*sent))
        .collect();
    report(format!(
        "{transport} delivery paced events=100 gap_ms=5: flushed->delivered[{}]",
        summary(latencies)
    ));

    let mut per_event = Vec::new();
    // Stay below the WebSocket ingress queue's 1600-message capacity.
    let events = 1500;
    for _ in 0..5 {
        let (marks, delivery) = scripted(server, Script::Burst { events }, &mut start).await;
        let elapsed = *delivery.deltas.last().unwrap() - marks[0];
        per_event.push(elapsed / events as u32);
    }
    report(format!(
        "{transport} delivery burst events={events}: written->delivered_per_event[{}]",
        summary(per_event)
    ));

    for bytes in [256 * KIB, MIB, 4 * MIB] {
        let (mut after_last_byte, mut total) = (Vec::new(), Vec::new());
        for _ in 0..5 {
            let (marks, delivery) =
                scripted(server, Script::LargeItem { bytes }, &mut start).await;
            let delivered = delivery.item.expect("large item delivered");
            after_last_byte.push(delivered.saturating_duration_since(marks[1]));
            total.push(delivered - marks[0]);
        }
        report(format!(
            "{transport} delivery large_item text_kib={}: last_byte_written->delivered[{}] first_byte->delivered[{}]",
            bytes / KIB,
            summary(after_last_byte),
            summary(total)
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "loopback wall-clock benchmark"]
async fn http_delivery_wall_clock() {
    let server = Server::default();
    let address = spawn_server({
        let server = server.clone();
        move |socket| serve_http(socket, server.clone())
    });
    let client = Arc::new(http_client(address));
    delivery_benchmarks("http", &server, || {
        let client = Arc::clone(&client);
        async move {
            client
                .stream_request(http_request(4 * KIB, 0), ResponsesOptions::default())
                .await
                .unwrap()
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "loopback wall-clock benchmark"]
async fn websocket_delivery_wall_clock() {
    for deflate in [true, false] {
        let server = Server::default();
        let address = spawn_server({
            let server = server.clone();
            move |socket| serve_websocket(socket, server.clone(), deflate)
        });
        let connection = Arc::new(ws_connection(address).await);
        delivery_benchmarks(&format!("websocket deflate={deflate}"), &server, || {
            let connection = Arc::clone(&connection);
            async move {
                connection
                    .stream_request(ws_request(4 * KIB, 0), true, None)
                    .await
                    .unwrap()
            }
        })
        .await;
    }
}
