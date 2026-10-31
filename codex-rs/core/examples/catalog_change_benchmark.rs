//! KDR discovery --workload catalog-change: bounded real connection-owner observations.
//! The existing static catalog workload cannot emit changes or expire sessions.
//! This inert loopback peer never performs external effects or model calls.
use anyhow::{Result, anyhow, ensure};
use codex_config::{Constrained, McpServerConfig};
use codex_exec_server::EnvironmentManager;
use codex_mcp::{
    CodexAppsToolsCache, EffectiveMcpServer, ElicitationRequestRouter, McpConnectionManager,
    McpRuntimeContext, ToolInfo, ToolPluginProvenance, codex_apps_tools_cache_key,
};
use codex_protocol::{models::PermissionProfile, protocol::AskForApproval};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    version: AtomicU64,
    epoch: AtomicU64,
    lists: AtomicU64,
    calls: AtomicU64,
    held: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
    stopped: AtomicBool,
}

struct Peer {
    state: Arc<State>,
    url: String,
    owner: Option<JoinHandle<Result<()>>>,
}

impl Peer {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let state = Arc::new(State::default());
        state.version.store(1, Ordering::Release);
        state.epoch.store(1, Ordering::Release);
        let shared = Arc::clone(&state);
        let owner = std::thread::spawn(move || {
            let mut requests = Vec::new();
            while !shared.stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        ensure!(requests.len() < 128, "bounded peer request limit");
                        let shared = Arc::clone(&shared);
                        requests.push(std::thread::spawn(move || serve(stream, &shared)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            for request in requests {
                request
                    .join()
                    .map_err(|_| anyhow!("peer request panicked"))??;
            }
            Ok(())
        });
        Ok(Self {
            state,
            url,
            owner: Some(owner),
        })
    }

    fn release(&self) {
        *self.state.released.lock().expect("peer gate") = true;
        self.state.release.notify_all();
    }

    fn finish(&mut self) -> Result<()> {
        self.release();
        self.state.stopped.store(true, Ordering::Release);
        if let Some(owner) = self.owner.take() {
            owner.join().map_err(|_| anyhow!("peer owner panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    session: &str,
    body: &str,
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{session}\r\n{body}",
        body.len()
    )?;
    stream.flush()?;
    Ok(())
}

fn serve(mut stream: TcpStream, state: &State) -> Result<()> {
    // Windows accepted sockets inherit the listener's nonblocking mode.
    // This bounded thread owns blocking buffered I/O, not an async socket.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut input = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    if input.read_line(&mut first)? == 0 {
        return Ok(());
    }
    let mut length = 0;
    let mut session = None;
    let mut header_bytes = first.len();
    loop {
        let mut line = String::new();
        ensure!(input.read_line(&mut line)? != 0, "incomplete HTTP headers");
        header_bytes += line.len();
        ensure!(header_bytes <= 65536, "HTTP header bound");
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>()?;
            }
            if key.eq_ignore_ascii_case("mcp-session-id") {
                session = Some(value.trim().to_owned());
            }
        }
    }
    if !first.starts_with("POST ") {
        return response(&mut stream, "405 Method Not Allowed", "text/plain", "", "");
    }
    ensure!(length <= 65536, "HTTP body bound");
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body)?;
    let method = request["method"].as_str().unwrap_or_default();
    let expected = format!("epoch-{}", state.epoch.load(Ordering::Acquire));
    if method != "initialize" && session.as_deref().is_some_and(|value| value != expected) {
        return response(
            &mut stream,
            "404 Not Found",
            "text/plain",
            "",
            "expired session",
        );
    }
    let Some(id) = request.get("id") else {
        return response(&mut stream, "202 Accepted", "text/plain", "", "");
    };
    let version = state.version.load(Ordering::Acquire);
    let result = match method {
        "initialize" => json!({"protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {"listChanged": true}, "resources": {}},
            "serverInfo": {"name": "inert-catalog-peer", "version": version.to_string()}}),
        "tools/list" => {
            state.lists.fetch_add(1, Ordering::AcqRel);
            let value_type = if version % 2 == 0 {
                "integer"
            } else {
                "string"
            };
            let schema = json!({"type": "object", "properties": {
                "version": {"const": version}, "value": {"type": value_type}}});
            let mut tools = vec![
                json!({"name": "lookup", "description": format!("Catalog {version}"), "inputSchema": schema}),
            ];
            if version > 1 {
                tools.push(json!({"name": "forbidden", "inputSchema": {"type": "object"}}));
            }
            json!({"tools": tools})
        }
        "resources/list" => json!({"resources": []}),
        "tools/call" => {
            ensure!(
                request["params"]["name"] == "lookup",
                "disabled tool reached peer"
            );
            state.calls.fetch_add(1, Ordering::AcqRel);
            if request["params"]["arguments"]["hold"] == true {
                state.held.store(true, Ordering::Release);
                let (released, _) = state
                    .release
                    .wait_timeout_while(
                        state.released.lock().expect("peer gate"),
                        Duration::from_secs(5),
                        |value| !*value,
                    )
                    .expect("peer gate wait");
                ensure!(*released, "bounded in-flight call was not released");
            }
            json!({"content": [{"type": "text", "text": format!("owned-v{version}")}], "isError": false})
        }
        other => return Err(anyhow!("unexpected peer method {other}")),
    };
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
    if request["params"]["arguments"]["notify"] == true {
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        return response(
            &mut stream,
            "200 OK",
            "text/event-stream",
            "",
            &format!("event: message\ndata: {notification}\n\nevent: message\ndata: {reply}\n\n"),
        );
    }
    let session_header = if method == "initialize" {
        format!("Mcp-Session-Id: {expected}\r\n")
    } else {
        String::new()
    };
    response(
        &mut stream,
        "200 OK",
        "application/json",
        &session_header,
        &reply.to_string(),
    )
}

fn version(tools: &[ToolInfo]) -> Option<u64> {
    tools
        .iter()
        .find(|tool| tool.tool.name == "lookup")?
        .tool
        .input_schema
        .get("properties")?
        .get("version")?
        .get("const")?
        .as_u64()
}

fn declaration(tools: &[ToolInfo]) -> Result<String> {
    let tool = tools
        .iter()
        .find(|tool| tool.tool.name == "lookup")
        .ok_or_else(|| anyhow!("lookup schema missing"))?;
    let identity = tool.canonical_tool_name();
    let member = codex_tools::mcp_tool_to_responses_api_tool(&identity, &tool.tool)?;
    let spec = if let Some(namespace) = &identity.namespace {
        codex_tools::ToolSpec::Namespace(codex_tools::ResponsesApiNamespace {
            name: namespace.clone(),
            description: String::new(),
            tools: vec![codex_tools::ResponsesApiNamespaceTool::Function(member)],
        })
    } else {
        codex_tools::ToolSpec::Function(member)
    };
    let definitions = codex_tools::collect_code_mode_tool_definitions_with_input_schemas(
        [&spec],
        &HashMap::from([(
            identity,
            Value::Object(tool.tool.input_schema.as_ref().clone()),
        )]),
    );
    ensure!(
        definitions.len() == 1,
        "exactly one refreshed declaration required"
    );
    Ok(definitions[0].description.to_string())
}

async fn await_version(manager: &McpConnectionManager, expected: u64) -> Result<f64> {
    let start = Instant::now();
    loop {
        if version(manager.list_all_tools_snapshot().await.as_ref()) == Some(expected) {
            return Ok(start.elapsed().as_secs_f64() * 1000.0);
        }
        ensure!(
            start.elapsed() < Duration::from_secs(5),
            "catalog {expected} did not refresh"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn observe() -> Result<Value> {
    let mut peer = Peer::start()?;
    let home = tempfile::tempdir()?;
    let config: McpServerConfig = serde_json::from_value(json!({"url": peer.url,
        "required": true, "startup_timeout_sec": 5, "tool_timeout_sec": 5,
        "disabled_tools": ["forbidden"]}))?;
    let servers = HashMap::from([("catalog".to_owned(), EffectiveMcpServer::configured(config))]);
    let (events, _receipts) = async_channel::unbounded();
    let manager = Arc::new(
        McpConnectionManager::new(
            &servers,
            Default::default(),
            Default::default(),
            HashMap::new(),
            &Constrained::allow_only(AskForApproval::Never),
            "catalog-change".to_owned(),
            events,
            CancellationToken::new(),
            PermissionProfile::default(),
            McpRuntimeContext::new(
                Arc::new(EnvironmentManager::without_environments()),
                home.path().to_owned(),
            ),
            home.path().to_owned(),
            CodexAppsToolsCache::default(),
            codex_apps_tools_cache_key(None, "http://127.0.0.1", None),
            true,
            Default::default(),
            false,
            ToolPluginProvenance::default(),
            None,
            None,
            None,
            ElicitationRequestRouter::default(),
            None,
        )
        .await,
    );
    manager.wait_for_startup().await;
    manager.validate_required_servers().await?;
    let original = manager.list_all_tools_snapshot().await;
    ensure!(version(original.as_ref()) == Some(1), "initial catalog");
    let held = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move {
            manager
                .call_tool("catalog", "lookup", Some(json!({"hold": true})), None)
                .await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !peer.state.held.load(Ordering::Acquire) {
        ensure!(Instant::now() < deadline, "in-flight call was not admitted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    peer.state.version.store(2, Ordering::Release);
    let before = manager.tool_catalog_revision();
    let initial_lists = peer.state.lists.load(Ordering::Acquire);
    // Exact resource-only invalidation used by the previous runtime refresh path.
    manager.invalidate_resource_caches();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let baseline_stale = version(manager.list_all_tools_snapshot().await.as_ref()) == Some(1)
        && manager.tool_catalog_revision() == before
        && peer.state.lists.load(Ordering::Acquire) == initial_lists;
    manager.invalidate_caches();
    let invalidation_ms = await_version(&manager, 2).await?;
    let invalidated = manager.list_all_tools_snapshot().await;
    let after = manager.tool_catalog_revision();
    let inflight_preserved = !held.is_finished();
    peer.release();
    let held_result = held.await??;
    let old_snapshot_unchanged = version(original.as_ref()) == Some(1);
    let disabled_denied = manager
        .call_tool("catalog", "forbidden", None, None)
        .await
        .is_err_and(|error| error.to_string().contains("disabled"));
    peer.state.version.store(3, Ordering::Release);
    manager
        .call_tool("catalog", "lookup", Some(json!({"notify": true})), None)
        .await?;
    let notification_ms = await_version(&manager, 3).await?;
    let notified = manager.list_all_tools_snapshot().await;
    let notification_revision = manager.tool_catalog_revision();
    peer.state.version.store(4, Ordering::Release);
    peer.state.epoch.store(2, Ordering::Release);
    manager.list_resources("catalog", None).await?;
    let reconnect_ms = await_version(&manager, 4).await?;
    let final_tools = manager.list_all_tools_snapshot().await;
    let row = json!({"baseline_stale": baseline_stale,
        "invalidation_refreshed": after > before,
        "notification_refreshed": notification_revision > after,
        "reconnect_refreshed": manager.tool_catalog_revision() > notification_revision,
        "inflight_preserved": inflight_preserved,
        "owned_result": held_result.content == vec![json!({"type": "text", "text": "owned-v1"})],
        "old_snapshot_unchanged": old_snapshot_unchanged,
        "disabled_denied": disabled_denied && final_tools.len() == 1,
        "declarations": [declaration(&original)?, declaration(&invalidated)?,
            declaration(&notified)?, declaration(&final_tools)?],
        "peer_calls": peer.state.calls.load(Ordering::Acquire),
        "catalog_lists": peer.state.lists.load(Ordering::Acquire),
        "invalidation_ms": invalidation_ms, "notification_ms": notification_ms, "reconnect_ms": reconnect_ms});
    manager.shutdown().await;
    peer.finish()?;
    Ok(row)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut rows = Vec::new();
    for _ in 0..3 {
        rows.push(observe().await?);
    }
    println!(
        "{}",
        json!({"workload": "connection-catalog-change-v1", "rows": rows})
    );
    Ok(())
}
