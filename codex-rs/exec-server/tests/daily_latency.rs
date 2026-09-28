//! Loopback regressions and opt-in probes using production timeout constants.
#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_exec_server::ExecServerClient;
use codex_exec_server::ExecServerError;
use codex_exec_server::RemoteExecServerConnectArgs;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

async fn initialize(socket: TcpStream) -> WebSocketStream<TcpStream> {
    let mut websocket = accept_async(socket).await.unwrap();
    let request = timeout(Duration::from_secs(5), websocket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let request: serde_json::Value = serde_json::from_slice(&request.into_data()).unwrap();
    assert_eq!(request["method"], "initialize");
    websocket
        .send(Message::Text(
            json!({"id": request["id"], "result": {"sessionId": "latency-probe"}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let notification = timeout(Duration::from_secs(5), websocket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let notification: serde_json::Value =
        serde_json::from_slice(&notification.into_data()).unwrap();
    assert_eq!(notification["method"], "initialized");
    websocket
}

async fn reject(mut socket: TcpStream) -> bool {
    let mut header = Vec::new();
    timeout(Duration::from_secs(5), async {
        while !header.ends_with(b"\r\n\r\n") {
            assert!(header.len() < 16 * 1024);
            match socket.read_u8().await {
                Ok(byte) => header.push(byte),
                // Recovery can cancel a final TCP connection at its absolute deadline.
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return false,
                Err(error) => panic!("failed to read handshake: {error}"),
            }
        }
        socket
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
        true
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latency_probe_permanent_websocket_rejection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let rejected = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&rejected);
    let (disconnect_tx, disconnect_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let websocket = initialize(socket).await;
        disconnect_rx.await.unwrap();
        drop(websocket);
        dropped_tx.send(()).unwrap();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            if reject(socket).await {
                count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    let client = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs::new(
        url.clone(),
        "latency".into(),
    ))
    .await
    .unwrap();
    let before = Instant::now();
    disconnect_tx.send(()).unwrap();
    dropped_rx.await.unwrap();
    // Wait for the first actual rejected reconnect, not a guessed scheduling delay.
    timeout(Duration::from_secs(5), async {
        while rejected.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let error = timeout(Duration::from_secs(2), client.environment_info())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, ExecServerError::Disconnected(_)), "{error}");
    let baseline_ms = before.elapsed().as_secs_f64() * 1000.0;
    let baseline_attempts = rejected.load(Ordering::SeqCst);
    assert_eq!(baseline_attempts, 1, "permanent rejection must not retry");
    assert!(error.to_string().contains("401"), "{error}");
    let mut controls = Vec::new();
    for _ in 0..5 {
        let before = Instant::now();
        let attempt = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs::new(
            url.clone(),
            "classification-control".into(),
        ))
        .await;
        let error = attempt.err().expect("401 must not connect");
        assert!(
            matches!(&error, ExecServerError::WebSocketConnect { source, .. }
            if matches!(source, tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status().as_u16() == 401)),
            "control must receive actual HTTP 401: {error}"
        );
        assert!(
            !error.is_retryable_preparation_error(),
            "typed classifier should reject permanent authentication failure"
        );
        controls.push(before.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "LATENCY_PROBE={}",
        json!({"probe": "permanent_websocket_rejection", "baseline_ms": baseline_ms,
        "baseline_rejected_reconnects": baseline_attempts, "single_attempt_control_ms": controls,
        "error": error.to_string()})
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in production watchdog probe: permits at most 92 real seconds"]
async fn latency_probe_silent_websocket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut websocket = initialize(socket).await;
        let request = timeout(Duration::from_secs(5), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&request.into_data()).unwrap();
        assert_eq!(request["method"], "fs/getMetadata");
        seen_tx.send(()).unwrap();
        // Stop polling the peer entirely: no Pong, response, or EOF is emitted.
        release_rx.await.unwrap();
        drop(websocket);
    });
    let client = ExecServerClient::connect_websocket(RemoteExecServerConnectArgs::new(
        url,
        "latency".into(),
    ))
    .await
    .unwrap();
    let mut request = tokio::spawn(async move {
        client
            .fs_get_metadata(codex_exec_server::FsGetMetadataParams {
                path: codex_utils_path_uri::PathUri::parse("file:///C:/probe").unwrap(),
                sandbox: None,
            })
            .await
    });
    seen_rx.await.unwrap();
    let before = Instant::now();
    let result = timeout(Duration::from_secs(92), &mut request).await;
    let observed_ms = before.elapsed().as_secs_f64() * 1000.0;
    release_tx.send(()).unwrap();
    server.await.unwrap();
    let error = result
        .expect("watchdog must end the silent RPC without peer EOF")
        .unwrap()
        .expect_err("silent peer never supplied file metadata");
    assert!(matches!(error, ExecServerError::Disconnected(_)), "{error}");
    println!(
        "LATENCY_PROBE={}",
        json!({"probe": "silent_websocket", "observed_ms": observed_ms,
        "disconnected_without_peer_eof": true})
    );
}
