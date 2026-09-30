use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

async fn mock_client(
    errors: Vec<i64>,
) -> (ExecServerClient, Arc<AtomicU64>, AbortOnDropHandle<()>) {
    let (client_io, server_io) = tokio::io::duplex(16 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (server_reader, mut server_writer) = tokio::io::split(server_io);
    let calls = Arc::new(AtomicU64::new(0));
    let server_calls = Arc::clone(&calls);
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut lines = BufReader::new(server_reader).lines();
        let mut errors = errors.into_iter();
        while let Some(line) = lines.next_line().await.expect("request line") {
            let request: Value = serde_json::from_str(&line).expect("request JSON");
            let Some(id) = request.get("id") else {
                continue;
            };
            let response = if request["method"] == INITIALIZE_METHOD {
                json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "capability-test"}})
            } else {
                assert_eq!(request["method"], FS_READ_FILE_BOUNDED_METHOD);
                server_calls.fetch_add(1, Ordering::SeqCst);
                match errors.next() {
                    Some(code) => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": code, "message": format!("error {code}")}
                    }),
                    None => json!({"jsonrpc": "2.0", "id": id, "result": {"dataBase64": ""}}),
                }
            };
            let mut bytes = serde_json::to_vec(&response).expect("response JSON");
            bytes.push(b'\n');
            server_writer.write_all(&bytes).await.expect("response");
        }
    }));
    let client = ExecServerClient::connect(
        JsonRpcConnection::from_stdio(client_reader, client_writer, "capability-test".to_string()),
        ExecServerClientConnectOptions::default(),
    )
    .await
    .expect("connect");
    (client, calls, task)
}

fn params() -> FsReadFileBoundedParams {
    FsReadFileBoundedParams {
        path: codex_utils_path_uri::PathUri::from_host_native_path(
            std::env::current_dir().expect("cwd").join("file"),
        )
        .expect("URI"),
        max_bytes: 10,
        confined_root: None,
        sandbox: None,
    }
}

#[tokio::test]
async fn bounded_read_capability_is_cached_only_for_its_connection() {
    let (client, calls, _server) = mock_client(vec![-32601]).await;
    for _ in 0..2 {
        let error = client
            .fs_read_file_bounded(params())
            .await
            .expect_err("unsupported");
        assert!(
            matches!(error, ExecServerError::Server { code: -32601, message }
            if message == "error -32601")
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Recovery retains Inner but replaces its RPC transport.
    let (replacement, replacement_calls, _replacement_server) = mock_client(Vec::new()).await;
    let replacement_rpc = replacement.rpc_client().await.expect("replacement RPC");
    client.inner.connection.lock().unwrap().status = ConnectionStatus::Connected(replacement_rpc);
    assert_eq!(
        client
            .fs_read_file_bounded(params())
            .await
            .expect("new server")
            .data_base64,
        Some(String::new())
    );
    assert_eq!(replacement_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bounded_read_capability_does_not_cache_other_errors() {
    let (client, calls, _server) = mock_client(vec![-32603]).await;
    assert!(matches!(
        client.fs_read_file_bounded(params()).await,
        Err(ExecServerError::Server { code: -32603, .. })
    ));
    assert!(client.fs_read_file_bounded(params()).await.is_ok());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
