//! Real-transport regressions and timings for the daily-use audit fixes.
#![cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_config::Constrained;
use codex_config::McpServerConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::EnvironmentManager;
use codex_mcp::*;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use rmcp::model::ElicitationCapability;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

struct Peer {
    url: String,
    log: Arc<Mutex<Vec<String>>>,
    release_init: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Peer {
    async fn new(hold_init: bool, hold_tools: bool, discovery_delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let release_init = CancellationToken::new();
        if !hold_init {
            release_init.cancel();
        }
        let task = tokio::spawn({
            let log = log.clone();
            let release = release_init.clone();
            async move {
                let mut children = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (socket, _) = accepted.unwrap();
                            children.spawn(serve(socket, log.clone(), release.clone(), hold_tools, discovery_delay));
                        }
                        done = children.join_next(), if !children.is_empty() => { done.unwrap().unwrap(); }
                    }
                }
            }
        });
        Self {
            url,
            log,
            release_init,
            task,
        }
    }

    fn count(&self, name: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.as_str() == name)
            .count()
    }

    fn server(&self, bearer: bool) -> McpServerConfig {
        let mut config = json!({"url":self.url,"startup_timeout_sec":10,"tool_timeout_sec":10});
        if bearer {
            config["http_headers"] = json!({"Authorization":"Bearer local-fixture"});
        }
        serde_json::from_value(config).unwrap()
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    release: CancellationToken,
    hold_tools: bool,
    discovery_delay: Duration,
) {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break i + 4;
        }
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        assert!(bytes.len() < 65536);
    };
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let post = header.starts_with("POST ");
    let path = header.lines().next().unwrap().to_owned();
    let len = header
        .lines()
        .find_map(|line| {
            let (name, val) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| val.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(len < 65536);
    while bytes.len() < header_end + len {
        let mut buf = [0; 4096];
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let (status, body) = if post {
        let req: Value = serde_json::from_slice(&bytes[header_end..header_end + len]).unwrap();
        let method = req["method"].as_str().unwrap();
        log.lock().unwrap().push(method.to_owned());
        let result = match method {
            "initialize" => {
                release.cancelled().await;
                Some(
                    json!({"protocolVersion":req["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"audit","version":"1"}}),
                )
            }
            "notifications/initialized" => None,
            "tools/list" => {
                if hold_tools {
                    std::future::pending::<()>().await;
                }
                Some(
                    json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{}}}]}),
                )
            }
            "tools/call" => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Some(json!({"content":[{"type":"text","text":"ok"}],"isError":false}))
            }
            other => panic!("unexpected method {other}"),
        };
        result.map_or((202, String::new()), |result| {
            (
                200,
                json!({"jsonrpc":"2.0","id":req["id"],"result":result}).to_string(),
            )
        })
    } else {
        log.lock().unwrap().push(path.clone());
        if path.contains(".well-known") {
            tokio::time::sleep(discovery_delay).await;
        }
        (404, String::new())
    };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    // Deadline probes intentionally close their connection before the response.
    let _ = socket.write_all(response.as_bytes()).await;
}

struct Fixture {
    home: tempfile::TempDir,
    context: McpRuntimeContext,
    cache: CodexAppsToolsCache,
    config: McpConfig,
}

impl Fixture {
    fn new(servers: Vec<(&str, McpServerConfig)>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            home.path().to_path_buf(),
        );
        let mut catalog = ResolvedMcpCatalog::builder();
        for (name, config) in servers {
            catalog.register(McpServerRegistration::from_config(name.to_owned(), config));
        }
        let config = McpConfig {
            chatgpt_base_url: "http://127.0.0.1".into(),
            apps_mcp_product_sku: None,
            codex_home: home.path().to_path_buf(),
            mcp_oauth_credentials_store_mode: OAuthCredentialsStoreMode::File,
            auth_keyring_backend_kind: AuthKeyringBackendKind::default(),
            mcp_oauth_callback_port: None,
            mcp_oauth_callback_url: None,
            skill_mcp_dependency_install_enabled: false,
            approval_policy: Constrained::allow_any(AskForApproval::Never),
            apps_enabled: false,
            prefix_mcp_tool_names: true,
            client_elicitation_capability: ElicitationCapability::default(),
            mcp_server_catalog: catalog.build(),
            connector_snapshot: Default::default(),
        };
        Self {
            home,
            context,
            cache: CodexAppsToolsCache::default(),
            config,
        }
    }

    async fn manager(&self) -> McpConnectionManager {
        self.manager_for(&effective_mcp_servers(&self.config, None))
            .await
    }

    async fn manager_for(
        &self,
        servers: &HashMap<String, EffectiveMcpServer>,
    ) -> McpConnectionManager {
        let (events, _receiver) = async_channel::unbounded();
        McpConnectionManager::new(
            servers,
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
            HashMap::new(),
            &self.config.approval_policy,
            "audit".into(),
            events,
            CancellationToken::new(),
            PermissionProfile::Disabled,
            self.context.clone(),
            self.home.path().to_path_buf(),
            self.cache.clone(),
            self.config.codex_apps_tools_cache_key(None),
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
        .await
    }

    async fn status(&self, manager: &McpConnectionManager, name: &str) -> McpServerStatusSnapshot {
        collect_mcp_server_status_snapshot_for_servers_with_detail(
            &self.config,
            None,
            "audit-status".into(),
            self.context.clone(),
            self.cache.clone(),
            McpSnapshotDetail::ToolsAndAuthOnly,
            &[name.to_owned()],
            Some(manager),
        )
        .await
    }
}

#[tokio::test]
async fn selected_status_reuses_healthy_peer_while_other_is_pending() {
    let ready = Peer::new(false, false, Duration::ZERO).await;
    let slow = Peer::new(true, false, Duration::ZERO).await;
    let mut f = Fixture::new(vec![
        ("ready", ready.server(true)),
        ("slow", slow.server(true)),
    ]);
    let manager = f.manager().await;
    assert!(
        manager
            .wait_for_server_ready("ready", Duration::from_secs(2))
            .await
    );
    let initial = ready.count("initialize");
    let start = Instant::now();
    for _ in 0..3 {
        let status = f.status(&manager, "ready").await;
        assert_eq!(status.tools_by_server["ready"].len(), 1);
        assert_eq!(status.server_names, ["ready"]);
    }
    let elapsed = start.elapsed();
    assert_eq!(ready.count("initialize"), initial);
    assert_eq!(ready.count("tools/list"), 1);
    assert_eq!(slow.count("initialize"), 1);
    slow.release_init.cancel();
    assert!(
        manager
            .wait_for_server_ready("slow", Duration::from_secs(2))
            .await
    );
    let before_control = ready.count("initialize");
    for _ in 0..3 {
        assert_eq!(
            f.status(&manager, "ready").await.tools_by_server["ready"].len(),
            1
        );
    }
    assert_eq!(ready.count("initialize"), before_control);
    println!(
        "AUDIT {}",
        json!({"case":"selected_status", "extra_initializations":0,"requests":3,"wall_ms":elapsed.as_secs_f64()*1000.,"all_ready_extra_initializations":0})
    );
    // A changed selected-server policy must not inherit the live client's tools.
    let mut changed = ready.server(true);
    changed.enabled_tools = Some(vec![]);
    let mut catalog = ResolvedMcpCatalog::builder();
    catalog.register(McpServerRegistration::from_config("ready".into(), changed));
    f.config.mcp_server_catalog = catalog.build();
    let status = f.status(&manager, "ready").await;
    assert!(status.tools_by_server.is_empty());
    assert_eq!(ready.count("initialize"), before_control + 1);
    assert_eq!(ready.count("tools/list"), 2);
    assert_eq!(
        manager.list_ready_tools_snapshot().await.tools.len(),
        2,
        "status must not change the original manager's tool policy"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn repeated_status_reuses_negative_oauth_discovery_for_ready_public_peer() {
    let ready = Peer::new(false, false, Duration::from_millis(200)).await;
    let f = Fixture::new(vec![("ready", ready.server(false))]);
    let manager = f.manager().await;
    assert!(
        manager
            .wait_for_server_ready("ready", Duration::from_secs(2))
            .await
    );
    ready.log.lock().unwrap().clear();
    let mut samples = Vec::new();
    let mut first_discovery_requests = None;
    for _ in 0..3 {
        let start = Instant::now();
        assert_eq!(
            f.status(&manager, "ready").await.tools_by_server["ready"].len(),
            1
        );
        samples.push(start.elapsed().as_secs_f64() * 1000.);
        let count = ready
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains(".well-known"))
            .count();
        assert!(count > 0);
        assert_eq!(*first_discovery_requests.get_or_insert(count), count);
    }
    let log = ready.log.lock().unwrap().clone();
    let gets = log.iter().filter(|s| s.contains(".well-known")).count();
    assert_eq!(gets, first_discovery_requests.unwrap());
    assert_eq!(
        ready.count("initialize"),
        0,
        "ready manager must actually be reused"
    );
    println!(
        "AUDIT {}",
        json!({"case":"auth_discovery", "well_known_requests":gets,"status_calls":3,"wall_ms":samples,"requests":log})
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn apps_refresh_does_not_wait_for_unrelated_initialization() {
    let apps = Peer::new(false, false, Duration::ZERO).await;
    let slow = Peer::new(true, false, Duration::ZERO).await;
    let f = Fixture::new(vec![
        (CODEX_APPS_MCP_SERVER_NAME, apps.server(true)),
        ("slow", slow.server(true)),
    ]);
    // Exercise the manager after auth gating without contacting a real Apps account.
    let servers = HashMap::from([
        (
            CODEX_APPS_MCP_SERVER_NAME.to_owned(),
            EffectiveMcpServer::configured(apps.server(true)),
        ),
        (
            "slow".to_owned(),
            EffectiveMcpServer::configured(slow.server(true)),
        ),
    ]);
    let manager = f.manager_for(&servers).await;
    assert!(
        manager
            .wait_for_server_ready(CODEX_APPS_MCP_SERVER_NAME, Duration::from_secs(2))
            .await
    );
    let before = apps.count("tools/list");
    let started = Instant::now();
    let tools = tokio::time::timeout(
        Duration::from_millis(800),
        manager.hard_refresh_codex_apps_tools_cache(),
    )
    .await
    .expect("Apps refresh must not await the unrelated server")
    .unwrap();
    assert_eq!(apps.count("tools/list"), before + 1);
    let ready = manager.list_ready_tools_snapshot().await;
    assert_eq!(ready.tools.len(), 1);
    assert_eq!(ready.pending_servers, ["slow"]);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].server_name, CODEX_APPS_MCP_SERVER_NAME);
    println!(
        "AUDIT {}",
        json!({"case":"apps_refresh", "wall_ms":started.elapsed().as_secs_f64()*1000.,"unrelated_release_required":false})
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn initial_tools_list_obeys_startup_budget() {
    let peer = Peer::new(false, true, Duration::ZERO).await;
    let mut server = peer.server(true);
    server.startup_timeout_sec = Some(Duration::from_millis(100));
    server.tool_timeout_sec = Some(Duration::from_millis(1200));
    server.required = true;
    let f = Fixture::new(vec![("required", server)]);
    let started = Instant::now();
    let manager = f.manager().await;
    let error = tokio::time::timeout(
        Duration::from_millis(800),
        manager.validate_required_servers(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("startup_timeout_sec"), "{error}");
    assert_eq!(peer.count("initialize"), 1);
    assert_eq!(peer.count("tools/list"), 1);
    println!(
        "AUDIT {}",
        json!({"case":"initial_discovery_budget","startup_budget_ms":100,"tool_budget_ms":1200,"wall_ms":started.elapsed().as_secs_f64()*1000.,"error":error.to_string()})
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn empty_scopes_roundtrip_preserves_login_selection() {
    let home = tempfile::tempdir().unwrap();
    let server: McpServerConfig = serde_json::from_value(
        json!({"url":"https://example.invalid/mcp","scopes":[],"enabled_tools":[]}),
    )
    .unwrap();
    let discovered = Some(vec!["read".to_owned(), "write".to_owned()]);
    assert!(
        resolve_oauth_scopes(None, server.scopes.clone(), discovered.clone())
            .scopes
            .is_empty()
    );
    let started = Instant::now();
    codex_config::ConfigEditsBuilder::new(home.path())
        .merge_mcp_servers(&BTreeMap::from([("peer".to_owned(), server)]))
        .apply()
        .await
        .unwrap();
    let loaded = codex_config::load_global_mcp_servers(home.path())
        .await
        .unwrap();
    assert_eq!(loaded["peer"].enabled_tools, Some(vec![]));
    assert_eq!(loaded["peer"].scopes, Some(vec![]));
    let scopes = resolve_oauth_scopes(None, loaded["peer"].scopes.clone(), discovered).scopes;
    assert!(scopes.is_empty());
    println!(
        "AUDIT {}",
        json!({"case":"empty_scopes","wall_ms":started.elapsed().as_secs_f64()*1000.,"requested_scope_count_before":0,"requested_scope_count_after":scopes.len()})
    );
}

#[tokio::test]
async fn steady_tool_call_retains_its_separate_timeout() {
    let peer = Peer::new(false, false, Duration::ZERO).await;
    let mut server = peer.server(true);
    server.startup_timeout_sec = Some(Duration::from_millis(100));
    server.tool_timeout_sec = Some(Duration::from_secs(1));
    server.required = true;
    let f = Fixture::new(vec![("ready", server)]);
    let manager = f.manager().await;
    manager.validate_required_servers().await.unwrap();
    let started = Instant::now();
    let result = manager
        .call_tool("ready", "echo", Some(json!({})), None)
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content, [json!({"type":"text","text":"ok"})]);
    assert!(started.elapsed() >= Duration::from_millis(200));
    manager.shutdown().await;
}
