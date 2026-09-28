//! Production catalog startup regressions and opt-in timing over real HTTP.

use super::*;
use codex_config::McpServerConfig;
use codex_exec_server::EnvironmentManager;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

struct LocalServer {
    address: std::net::SocketAddr,
    release: CancellationToken,
    started: Arc<tokio::sync::Notify>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl LocalServer {
    async fn start(hold_startup: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = CancellationToken::new();
        if !hold_startup {
            release.cancel();
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let stop = CancellationToken::new();
        let task = tokio::spawn({
            let release = release.clone();
            let started = Arc::clone(&started);
            let stop = stop.clone();
            async move {
                let mut connections = JoinSet::<std::io::Result<()>>::new();
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => {
                            connections.shutdown().await;
                            return;
                        }
                        completed = connections.join_next(), if !connections.is_empty() => {
                            completed.unwrap().unwrap().unwrap();
                        }
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            connections.spawn(serve(stream, release.clone(), Arc::clone(&started)));
                        }
                    }
                }
            }
        });
        Self {
            address,
            release,
            started,
            stop,
            task,
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

// A bounded stateless JSON-RPC/HTTP fixture uses the real RMCP HTTP transport.
async fn serve(
    mut stream: tokio::net::TcpStream,
    release: CancellationToken,
    started: Arc<tokio::sync::Notify>,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
        assert!(request.len() <= 65536);
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let post = headers.starts_with("POST ");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(content_length <= 65536);
    while request.len() < header_end + content_length {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
    }
    let (status, body) = if post {
        let message: JsonValue =
            serde_json::from_slice(&request[header_end..header_end + content_length])?;
        let result = match message["method"].as_str().unwrap() {
            "initialize" => {
                started.notify_one();
                release.cancelled().await;
                Some(
                    json!({"protocolVersion":message["params"]["protocolVersion"], "capabilities":{"tools":{},"resources":{}},"serverInfo":{"name":"benchmark","version":"1"}}),
                )
            }
            "notifications/initialized" => None,
            "tools/list" => Some(
                json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{}}}]}),
            ),
            "tools/call" => Some(
                json!({"content":[{"type":"text","text":"ready tool executed"}],"isError":false}),
            ),
            method => panic!("unexpected fixture method {method}"),
        };
        result.map_or((202, String::new()), |result| {
            (
                200,
                json!({"jsonrpc":"2.0","id":message["id"],"result":result}).to_string(),
            )
        })
    } else {
        (405, String::new())
    };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_catalog_preserves_healthy_tools_and_invalidates_on_late_startup() {
    optional_mcp_startup(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in real HTTP MCP startup benchmark"]
async fn optional_mcp_startup_efficiency_benchmark() {
    optional_mcp_startup(true).await;
}

async fn optional_mcp_startup(measure: bool) {
    let mut ready = LocalServer::start(false).await;
    let mut slow = LocalServer::start(true).await;
    let home = tempfile::tempdir().unwrap();
    let servers = [("ready", ready.address), ("slow", slow.address)].into_iter().map(|(name, address)| {
        let config: McpServerConfig = serde_json::from_value(json!({"url":format!("http://{address}/mcp"),"required":false,"startup_timeout_sec":30})).unwrap();
        (name.to_string(), EffectiveMcpServer::configured(config))
    }).collect();
    let (events, _receiver) = async_channel::unbounded();
    let manager = McpConnectionManager::new(
        &servers,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        HashMap::new(),
        &Constrained::allow_any(AskForApproval::Never),
        "benchmark".into(),
        events,
        CancellationToken::new(),
        PermissionProfile::Disabled,
        McpRuntimeContext::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            home.path().to_path_buf(),
        ),
        home.path().to_path_buf(),
        CodexAppsToolsCache::default(),
        crate::codex_apps_cache::codex_apps_tools_cache_key(None, "http://127.0.0.1", None),
        true,
        ElicitationCapability::default(),
        false,
        ToolPluginProvenance::default(),
        None,
        None,
        None,
        ElicitationRequestRouter::default(),
        None,
    )
    .await;
    assert!(
        manager
            .wait_for_server_ready("ready", Duration::from_secs(5))
            .await
    );
    tokio::time::timeout(Duration::from_secs(5), slow.started.notified())
        .await
        .unwrap();
    manager.validate_required_servers().await.unwrap();
    let mut baseline_ms = Vec::new();
    let mut candidate_ms = Vec::new();
    let mut captured = None;
    for _ in 0..if measure { 3 } else { 1 } {
        let start = Instant::now();
        let snapshot =
            tokio::time::timeout(Duration::from_secs(1), manager.list_ready_tools_snapshot())
                .await
                .unwrap();
        candidate_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(snapshot.tools.len(), 1);
        assert_eq!(snapshot.tools[0].server_name, "ready");
        assert_eq!(snapshot.pending_servers, ["slow"]);
        let reused = manager.list_ready_tools_snapshot().await;
        assert!(Arc::ptr_eq(&snapshot.tools, &reused.tools));
        captured = Some(snapshot);
        assert!(manager.has_ready_server_with_resources().await);
        let start = Instant::now();
        // This is the session owner's two-second aggregate-capture boundary.
        let baseline = tokio::time::timeout(
            if measure {
                Duration::from_secs(2)
            } else {
                Duration::from_millis(30)
            },
            async {
                tokio::join!(
                    manager.list_all_tools_snapshot(),
                    manager.has_ready_server_with_resources()
                )
            },
        )
        .await;
        baseline_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(
            baseline.is_err(),
            "baseline unexpectedly exposed a coherent catalog"
        );
        let executed = manager
            .call_tool("ready", "echo", Some(json!({})), None)
            .await
            .unwrap();
        assert_eq!(executed.is_error, Some(false));
        assert_eq!(
            executed.content,
            vec![json!({"type":"text","text":"ready tool executed"})]
        );
    }
    let frozen = captured.unwrap();
    slow.release.cancel();
    assert!(
        manager
            .wait_for_server_ready("slow", Duration::from_secs(5))
            .await
    );
    let current = tokio::time::timeout(Duration::from_secs(1), manager.list_ready_tools_snapshot())
        .await
        .unwrap();
    assert_eq!(current.tools.len(), 2);
    assert!(current.pending_servers.is_empty());
    assert!(
        current.revision > frozen.revision,
        "newly available tools must change snapshot identity"
    );
    assert_eq!(
        frozen.tools.len(),
        1,
        "an advertised snapshot must stay immutable"
    );
    let started = Instant::now();
    let all_ready = manager.list_all_tools_snapshot().await;
    let all_ready_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(all_ready.len(), 2);
    assert!(Arc::ptr_eq(&all_ready, &current.tools));
    manager.shutdown().await;
    ready.stop.cancel();
    slow.stop.cancel();
    (&mut ready.task).await.unwrap();
    (&mut slow.task).await.unwrap();
    if !measure {
        return;
    }
    let output =
        PathBuf::from(std::env::var_os("MCP_CATALOG_BENCH_OUTPUT").expect("benchmark output path"));
    assert!(
        output.is_absolute(),
        "benchmark output must be absolute: {output:?}"
    );
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&json!({
            "baseline_capture_ms": baseline_ms, "candidate_capture_ms": candidate_ms,
            "baseline_fallback_tool_count": 0, "candidate_ready_tool_count": 1,
            "healthy_tool_calls_completed_while_slow_pending": 3,
            "late_startup_tools": current.tools.len(), "all_ready_baseline_ms": all_ready_ms,
            "production_revision_before": frozen.revision, "production_revision_after": current.revision,
            "candidate_identity_changed": current.revision != frozen.revision,
            "real_http_transport": true, "frozen_snapshot_and_revision_controls": "passed",
        }))
        .unwrap(),
    )
    .unwrap();
}
