//! Opt-in measurements of the app-server review findings, not model-quality benchmarks.

use super::*;
use crate::connection_rpc_gate::ConnectionRpcGate;
use crate::outgoing_message::OutgoingEnvelope;
use crate::outgoing_message::OutgoingMessage;
use crate::request_serialization::QueuedInitializedRequest;
use crate::request_serialization::RequestSerializationAccess;
use crate::request_serialization::RequestSerializationQueueKey;
use crate::request_serialization::RequestSerializationQueues;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadGoalClearedNotification;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Semaphore;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(30);

#[expect(clippy::print_stdout, reason = "emits the opt-in benchmark report")]
fn record(value: serde_json::Value) {
    println!("EFFICIENCY_BENCH {value}");
}

async fn submit(harness: &TracingHarness, method: &str, id: i64, params: serde_json::Value) {
    harness
        .processor
        .process_request(
            TEST_CONNECTION_ID,
            JSONRPCRequest {
                id: RequestId::Integer(id),
                method: method.to_string(),
                params: Some(params),
                trace: None,
            },
            &AppServerTransport::Stdio,
            Arc::clone(&harness.session),
        )
        .await;
}

async fn next_message(harness: &mut TracingHarness) -> Result<OutgoingMessage> {
    let envelope = timeout(DEADLINE, harness.outgoing_rx.recv())
        .await?
        .ok_or_else(|| anyhow::anyhow!("outgoing channel closed"))?;
    Ok(match envelope {
        OutgoingEnvelope::ToConnection { message, .. }
        | OutgoingEnvelope::Broadcast { message } => message,
    })
}

async fn rpc(
    harness: &mut TracingHarness,
    method: &str,
    id: i64,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    submit(harness, method, id, params).await;
    timeout(DEADLINE, async {
        loop {
            match next_message(harness).await? {
                OutgoingMessage::Response(response) if response.id == RequestId::Integer(id) => {
                    return Ok(response.result);
                }
                OutgoingMessage::Error(error) => anyhow::bail!("{method}: {error:?}"),
                _ => {}
            }
        }
    })
    .await?
}

async fn fixture(
    extra: &str,
    chatgpt: bool,
    loader: Arc<dyn ThreadConfigLoader>,
) -> Result<TracingHarness> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"mock-model\"\nmodel_provider = \"mock_provider\"\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\nchatgpt_base_url = \"{}\"\ncli_auth_credentials_store = \"file\"\n[features]\nplugins = false\nshell_snapshot = false\n[model_providers.mock_provider]\nname = \"Benchmark\"\nbase_url = \"{}/v1\"\nwire_api = \"responses\"\nrequest_max_retries = 0\nstream_max_retries = 0\nrequires_openai_auth = {chatgpt}\n{extra}",
            server.uri(),
            server.uri()
        ),
    )?;
    app_test_support::write_models_cache(home.path())?;
    if chatgpt {
        app_test_support::write_chatgpt_auth(
            home.path(),
            app_test_support::ChatGptAuthFixture::new("bench-token")
                .account_id("bench-account")
                .chatgpt_account_id("bench-account")
                .chatgpt_user_id("bench-user")
                .email("bench@example.invalid")
                .plan_type("pro"),
            codex_config::types::AuthCredentialsStoreMode::File,
        )?;
    }
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .fallback_cwd(Some(home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await?,
    );
    let (processor, outgoing_rx) = build_test_processor(config, loader, None).await;
    let mut harness = TracingHarness {
        _server: server,
        _codex_home: home,
        processor,
        outgoing_rx,
        session: Arc::new(ConnectionSessionState::new()),
        tracing: init_test_tracing(),
    };
    let initialized = Arc::new(AtomicBool::new(false));
    harness
        .processor
        .outgoing
        .connection_opened(TEST_CONNECTION_ID, Arc::clone(&initialized))
        .await;
    rpc(
        &mut harness,
        "initialize",
        900_000,
        json!({
            "clientInfo": {"name": "codex-app-server-tests", "version": "0.1.0"},
            "capabilities": {"experimentalApi": true}
        }),
    )
    .await?;
    harness
        .processor
        .thread_processor
        .thread_state_manager
        .connection_initialized(
            TEST_CONNECTION_ID,
            ConnectionCapabilities {
                experimental_api: true,
                ..Default::default()
            },
        )
        .await;
    initialized.store(true, Ordering::Release);
    Ok(harness)
}

#[test]
#[ignore = "opt-in wall-clock probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_shared_read_arrival() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_shared_read_arrival", async {
        for mode in ["prequeued", "staggered", "different_key"] {
            for sample in 0..5 {
                let queues = RequestSerializationQueues::default();
                let gate = Arc::new(ConnectionRpcGate::new());
                let key = RequestSerializationQueueKey::Global("efficiency-bench");
                let (block_tx, block_rx) = tokio::sync::oneshot::channel();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                if mode == "prequeued" {
                    queues
                        .enqueue(
                            key.clone(),
                            RequestSerializationAccess::Exclusive,
                            QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                                let _ = entered_tx.send(());
                                let _ = block_rx.await;
                            }),
                        )
                        .await;
                    timeout(DEADLINE, entered_rx).await??;
                }
                let done = Arc::new(AtomicBool::new(false));
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (done_tx, done_rx) = tokio::sync::oneshot::channel();
                let first_done = Arc::clone(&done);
                queues
                    .enqueue(
                        key.clone(),
                        RequestSerializationAccess::SharedRead,
                        QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                            let _ = started_tx.send(());
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            first_done.store(true, Ordering::Release);
                            let _ = done_tx.send(());
                        }),
                    )
                    .await;
                if mode != "prequeued" {
                    timeout(DEADLINE, started_rx).await??;
                }
                let start = Instant::now();
                let (fast_tx, fast_rx) = tokio::sync::oneshot::channel();
                let fast_key = if mode == "different_key" {
                    RequestSerializationQueueKey::Global("efficiency-bench-other")
                } else {
                    key
                };
                queues
                    .enqueue(
                        fast_key,
                        RequestSerializationAccess::SharedRead,
                        QueuedInitializedRequest::new(Arc::clone(&gate), async move {
                            let _ = fast_tx.send((start.elapsed(), done.load(Ordering::Acquire)));
                        }),
                    )
                    .await;
                if mode == "prequeued" {
                    let _ = block_tx.send(());
                }
                let (wait, first_finished) = timeout(DEADLINE, fast_rx).await??;
                assert!(
                    !first_finished,
                    "a spare read slot must admit staggered readers"
                );
                timeout(DEADLINE, done_rx).await??;
                gate.shutdown().await;
                record(json!({"probe":"shared_read", "mode":mode, "sample":sample,
                    "injected_read_ms":200, "fast_wait_ms":wait.as_secs_f64()*1000.0,
                    "first_finished_before_fast":first_finished}));
            }
        }
        Ok(())
    })
}

struct GatedLoader {
    armed: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}

impl ThreadConfigLoader for GatedLoader {
    fn load(
        &self,
        _context: ThreadConfigContext,
    ) -> ThreadConfigLoaderFuture<'_, Vec<ThreadConfigSource>> {
        let block = self.armed.swap(false, Ordering::AcqRel);
        Box::pin(async move {
            if block {
                self.entered.add_permits(1);
                self.release
                    .acquire()
                    .await
                    .expect("release stays open")
                    .forget();
            }
            Ok(Vec::new())
        })
    }
}

#[test]
#[ignore = "opt-in wall-clock probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_resume_interference() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_resume_interference", async {
        for sample in 0..3 {
            let loader = Arc::new(GatedLoader {
                armed: AtomicBool::new(false),
                entered: Semaphore::new(0),
                release: Semaphore::new(0),
            });
            let mut harness = fixture("", false, loader.clone()).await?;
            let home = harness._codex_home.path().to_path_buf();
            let a = app_test_support::create_fake_rollout(
                &home,
                "2025-01-01T00-00-00",
                "2025-01-01T00:00:00Z",
                "resume",
                Some("mock_provider"),
                None,
            )?;
            let b = app_test_support::create_fake_rollout(
                &home,
                "2025-01-01T00-01-00",
                "2025-01-01T00:01:00Z",
                "rename",
                Some("mock_provider"),
                None,
            )?;
            let start = Instant::now();
            assert_eq!(
                rpc(
                    &mut harness,
                    "thread/name/set",
                    900_010,
                    json!({"threadId":b, "name":"baseline"})
                )
                .await?,
                json!({})
            );
            let baseline_ms = start.elapsed().as_secs_f64() * 1000.0;
            loader.armed.store(true, Ordering::Release);
            submit(
                &harness,
                "thread/resume",
                900_011,
                json!({"threadId":a, "cwd":home, "excludeTurns":true}),
            )
            .await;
            timeout(DEADLINE, loader.entered.acquire())
                .await?
                .expect("loader entered")
                .forget();
            let start = Instant::now();
            submit(
                &harness,
                "thread/name/set",
                900_012,
                json!({"threadId":b, "name":"after blocked resume"}),
            )
            .await;
            let released = Arc::new(AtomicBool::new(false));
            let release_loader = Arc::clone(&loader);
            let release_flag = Arc::clone(&released);
            let release = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                release_flag.store(true, Ordering::Release);
                release_loader.release.add_permits(1);
            });
            let mut pending = true;
            let mut resume_done = false;
            let mut rename_ms = None;
            while !resume_done || rename_ms.is_none() {
                match next_message(&mut harness).await? {
                    OutgoingMessage::Response(response)
                        if response.id == RequestId::Integer(900_011) =>
                    {
                        assert_eq!(response.result["thread"]["id"], a);
                        resume_done = true;
                    }
                    OutgoingMessage::Response(response)
                        if response.id == RequestId::Integer(900_012) =>
                    {
                        assert_eq!(response.result, json!({}));
                        rename_ms = Some(start.elapsed().as_secs_f64() * 1000.0);
                        pending = released.load(Ordering::Acquire);
                    }
                    OutgoingMessage::Error(error) => anyhow::bail!("resume probe: {error:?}"),
                    _ => {}
                }
            }
            let read = rpc(
                &mut harness,
                "thread/read",
                900_013,
                json!({"threadId":b,"includeTurns":false}),
            )
            .await?;
            assert_eq!(read["thread"]["name"], "after blocked resume");
            release.await?;
            assert!(
                !pending,
                "unrelated rename must complete while resume is blocked"
            );
            record(
                json!({"probe":"resume_interference","sample":sample,"baseline_rename_ms":baseline_ms,
                "injected_config_ms":200,"blocked_rename_ms":rename_ms,"pending_before_release":pending}),
            );
            harness.shutdown().await;
        }
        Ok(())
    })
}

#[derive(Clone)]
struct CountingMcp {
    initializations: Arc<AtomicUsize>,
    lists: Arc<AtomicUsize>,
}

impl rmcp::handler::server::ServerHandler for CountingMcp {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        assert_eq!(request.name, "probe");
        Ok(rmcp::model::CallToolResult::success(vec![
            rmcp::model::Content::text("probe-ok"),
        ]))
    }

    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }
    async fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParams,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> std::result::Result<rmcp::model::InitializeResult, rmcp::ErrorData> {
        self.initializations.fetch_add(1, Ordering::AcqRel);
        context.peer.set_peer_info(request);
        tokio::time::sleep(Duration::from_millis(120)).await;
        Ok(self.get_info())
    }
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> std::result::Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
        self.lists.fetch_add(1, Ordering::AcqRel);
        Ok(rmcp::model::ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: vec![rmcp::model::Tool::new(
                "probe",
                "Benchmark tool",
                Arc::new(serde_json::Map::new()),
            )],
        })
    }
}

#[test]
#[ignore = "opt-in local-network probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_mcp_status_reconnect() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_mcp_status_reconnect", async {
        let counts = CountingMcp {
            initializations: Arc::new(AtomicUsize::new(0)),
            lists: Arc::new(AtomicUsize::new(0)),
        };
        let factory = counts.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let service = rmcp::transport::StreamableHttpService::new(move || Ok(factory.clone()),
            Arc::new(rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default()),
            rmcp::transport::StreamableHttpServerConfig::default());
        let server = tokio::spawn(async move {
            axum::serve(listener, axum::Router::new().nest_service("/mcp", service)).await
        });
        let mut harness = fixture(
            &format!("\n[mcp_servers.bench]\nurl = \"http://{address}/mcp\"\nrequired = true\n"),
            false,
            Arc::new(codex_config::NoopThreadConfigLoader),
        )
        .await?;
        let thread = rpc(&mut harness, "thread/start", 900_020, json!({})).await?;
        assert_eq!(counts.initializations.load(Ordering::Acquire), 1);
        let live_thread = harness
            .processor
            .thread_manager
            .get_thread(ThreadId::from_string(
                thread["thread"]["id"].as_str().expect("thread id"),
            )?)
            .await?;
        let live_runtime = live_thread.current_mcp_runtime().await;
        for sample in 0..5 {
            let before = counts.initializations.load(Ordering::Acquire);
            let before_lists = counts.lists.load(Ordering::Acquire);
            let start = Instant::now();
            let response = rpc(
                &mut harness,
                "mcpServerStatus/list",
                900_021 + sample,
                json!({
                    "threadId":thread["thread"]["id"], "detail":"toolsAndAuthOnly"
                }),
            )
            .await?;
            let elapsed = start.elapsed();
            assert_eq!(response["data"][0]["name"], "bench");
            assert!(response["data"][0]["tools"]["probe"].is_object());
            let new_connections = counts.initializations.load(Ordering::Acquire) - before;
            assert_eq!(
                new_connections, 0,
                "status must reuse the live MCP connection"
            );
            assert_eq!(counts.lists.load(Ordering::Acquire), before_lists);
            record(
                json!({"probe":"mcp_status", "sample":sample,"injected_initialize_ms":120,
                "rpc_ms":elapsed.as_secs_f64()*1000.0,"new_initializations":new_connections,
                "new_tools_lists":counts.lists.load(Ordering::Acquire)-before_lists}),
            );
        }
        // Changed or disabled servers must not expose the old catalog. A status
        // snapshot must not shut down a connection still leased by a live turn.
        // Use the mutation owner so ConfigManager's load cache is invalidated.
        rpc(&mut harness, "config/value/write", 900_026, json!({
            "keyPath":"mcp_servers.bench.url", "value":format!("http://{address}/mcp?changed=1"),
            "mergeStrategy":"replace"
        })).await?;
        let before = counts.initializations.load(Ordering::Acquire);
        rpc(
            &mut harness,
            "mcpServerStatus/list",
            900_028,
            json!({"threadId":thread["thread"]["id"],"detail":"toolsAndAuthOnly"}),
        )
        .await?;
        assert_eq!(counts.initializations.load(Ordering::Acquire) - before, 1);
        rpc(
            &mut harness,
            "config/value/write",
            900_027,
            json!({
                "keyPath":"mcp_servers.bench.enabled", "value":false, "mergeStrategy":"replace"
            }),
        )
        .await?;
        let before = counts.initializations.load(Ordering::Acquire);
        let disabled = rpc(
            &mut harness,
            "mcpServerStatus/list",
            900_029,
            json!({"threadId":thread["thread"]["id"],"detail":"toolsAndAuthOnly"}),
        )
        .await?;
        assert_eq!(counts.initializations.load(Ordering::Acquire), before);
        assert_eq!(disabled["data"][0]["tools"], json!({}));
        let result = live_runtime
            .manager()
            .call_tool("bench", "probe", None, None)
            .await?;
        assert_eq!(
            serde_json::to_value(result)?["content"][0]["text"],
            "probe-ok"
        );
        harness.shutdown().await;
        server.abort();
        let _ = server.await;
        Ok(())
    })
}

#[test]
#[ignore = "opt-in local-network probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_account_read() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_account_read", async {
        for delay_ms in [0, 200, 2000] {
            let mut harness =
                fixture("", true, Arc::new(codex_config::NoopThreadConfigLoader)).await?;
            harness._server.reset().await;
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/api/codex/accounts/check"))
                .respond_with(
                    wiremock::ResponseTemplate::new(503).set_delay(Duration::from_millis(delay_ms)),
                )
                .mount(&harness._server)
                .await;
            for sample in 0..5 {
                let start = Instant::now();
                let response = rpc(
                    &mut harness,
                    "account/read",
                    900_030 + sample,
                    json!({"refreshToken":false}),
                )
                .await?;
                assert_eq!(response["account"]["type"], "chatgpt");
                assert!(response["workspaceRouting"].is_null());
                record(
                    json!({"probe":"account_read","sample":sample,"injected_route_ms":delay_ms,
                    "rpc_ms":start.elapsed().as_secs_f64()*1000.0}),
                );
            }
            let requests = harness
                ._server
                .received_requests()
                .await
                .expect("request recording");
            let route_calls = requests
                .iter()
                .filter(|request| request.url.path() == "/api/codex/accounts/check")
                .count();
            assert_eq!(
                route_calls, 1,
                "unchanged reads reuse the bounded discovery result"
            );
            record(
                json!({"probe":"account_read_count","injected_route_ms":delay_ms,"reads":5,"route_calls":route_calls}),
            );
            harness.shutdown().await;
        }
        Ok(())
    })
}

#[test]
#[serial(app_server_tracing)]
fn efficiency_routing_cache_regression() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_routing_cache_regression", async {
        let mut harness = fixture("", true, Arc::new(codex_config::NoopThreadConfigLoader)).await?;
        harness._server.reset().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/codex/accounts/check"))
            .respond_with(wiremock::ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(50))
                .set_body_json(json!({"accounts":[
                    {"id":"bench-account","workspace_backend_origin":"https://chatgpt.com","account_routing_override":"us"},
                    {"id":"other-account","workspace_backend_origin":"https://other.example","account_routing_override":"us"}
                ]})))
            .mount(&harness._server).await;
        for id in 900_100..900_104 {
            submit(&harness, "account/read", id, json!({"refreshToken":false})).await;
        }
        let mut completed = std::collections::HashSet::new();
        while completed.len() < 4 {
            match next_message(&mut harness).await? {
                OutgoingMessage::Response(response) => {
                    assert_eq!(
                        response.result["workspaceRouting"]["chatgptAccountId"],
                        "bench-account"
                    );
                    assert!(completed.insert(response.id));
                }
                OutgoingMessage::Error(error) => anyhow::bail!("cache probe: {error:?}"),
                _ => {}
            }
        }
        assert_eq!(harness._server.received_requests().await.unwrap().len(), 1);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        rpc(
            &mut harness,
            "account/read",
            900_104,
            json!({"refreshToken":false}),
        )
        .await?;
        assert_eq!(
            harness._server.received_requests().await.unwrap().len(),
            2,
            "expiry refreshes routing"
        );
        for (token, account, user, expected_count) in [
            ("new-token", "bench-account", "bench-user", 3),
            ("new-token", "other-account", "other-user", 4),
        ] {
            app_test_support::write_chatgpt_auth(
                harness._codex_home.path(),
                app_test_support::ChatGptAuthFixture::new(token)
                    .account_id(account)
                    .chatgpt_account_id(account)
                    .chatgpt_user_id(user)
                    .email("bench@example.invalid")
                    .plan_type("pro"),
                codex_config::types::AuthCredentialsStoreMode::File,
            )?;
            harness
                .processor
                .thread_manager
                .auth_manager()
                .reload()
                .await;
            let response = rpc(
                &mut harness,
                "account/read",
                900_105 + expected_count,
                json!({"refreshToken":false}),
            )
            .await?;
            assert_eq!(response["workspaceRouting"]["chatgptAccountId"], account);
            assert_eq!(
                harness._server.received_requests().await.unwrap().len(),
                expected_count as usize
            );
        }
        std::fs::remove_file(harness._codex_home.path().join("auth.json"))?;
        harness
            .processor
            .thread_manager
            .auth_manager()
            .reload()
            .await;
        let response = rpc(
            &mut harness,
            "account/read",
            900_110,
            json!({"refreshToken":false}),
        )
        .await?;
        assert!(response["account"].is_null());
        assert!(response["workspaceRouting"].is_null());
        assert_eq!(
            harness._server.received_requests().await.unwrap().len(),
            4,
            "signed-out reads do not discover routing"
        );
        harness.shutdown().await;
        Ok(())
    })
}

fn fill_notifications(outgoing: &OutgoingMessageSender) -> usize {
    let mut count = 0;
    while outgoing.try_send_server_notification(ServerNotification::ThreadGoalCleared(
        ThreadGoalClearedNotification {
            thread_id: "benchmark-filler".to_string(),
        },
    )) {
        count += 1;
    }
    assert!(count > 0, "must saturate the real outgoing queue");
    count
}

async fn wait_file(path: &Path) -> Result<()> {
    timeout(DEADLINE, async {
        while !path.is_file() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}

#[test]
#[cfg(windows)]
#[ignore = "opt-in real child-process and backpressure probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_process_output() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_process_output", async {
        const TAIL_BYTES: usize = 512 * 1024;
        for saturated in [false, true, true, true] {
            let mut harness =
                fixture("", false, Arc::new(codex_config::NoopThreadConfigLoader)).await?;
            let dir = TempDir::new()?;
            let gate = dir.path().join("release");
            let finished = dir.path().join("finished");
            let quote = |p: &Path| p.display().to_string().replace('\'', "''");
            let script = format!(
                "$out=[Console]::OpenStandardOutput(); $out.WriteByte(80); $out.Flush(); while (-not [IO.File]::Exists('{}')) {{ Start-Sleep -Milliseconds 10 }}; $out.WriteByte(65); $out.Flush(); Start-Sleep -Milliseconds 200; $bytes=[Text.Encoding]::ASCII.GetBytes(('z' * {TAIL_BYTES})); $out.Write($bytes,0,$bytes.Length); $out.Flush(); [IO.File]::WriteAllText('{}','finished')",
                quote(&gate),
                quote(&finished)
            );
            let response = rpc(&mut harness, "process/spawn", 900_040, json!({
                "command":["powershell.exe","-NoLogo","-NoProfile","-NonInteractive","-Command",script],
                "processHandle":"probe", "cwd":dir.path(), "streamStdoutStderr":true,
                "outputBytesCap":null, "timeoutMs":30000
            })).await?;
            assert_eq!(response, json!({}));
            let mut output = Vec::new();
            while output.is_empty() {
                if let OutgoingMessage::AppServerNotification(
                    ServerNotification::ProcessOutputDelta(delta),
                ) = next_message(&mut harness).await?
                {
                    output.extend(STANDARD.decode(delta.delta_base64)?);
                }
            }
            assert_eq!(output, b"P");
            if saturated {
                fill_notifications(&harness.processor.outgoing);
            }
            let start = Instant::now();
            std::fs::write(&gate, "go")?;
            if saturated {
                wait_file(&finished).await?;
                tokio::time::sleep(Duration::from_secs(6)).await;
                assert_eq!(harness.outgoing_rx.capacity(), 0);
            }
            let exited = loop {
                match next_message(&mut harness).await? {
                    OutgoingMessage::AppServerNotification(
                        ServerNotification::ProcessOutputDelta(delta),
                    ) => output.extend(STANDARD.decode(delta.delta_base64)?),
                    OutgoingMessage::AppServerNotification(ServerNotification::ProcessExited(
                        exited,
                    )) => break exited,
                    OutgoingMessage::Error(error) => anyhow::bail!("process probe: {error:?}"),
                    _ => {}
                }
            };
            assert_eq!(exited.exit_code, 0);
            assert!(!exited.stdout_cap_reached);
            assert!(exited.stderr.is_empty());
            assert_eq!(std::fs::read_to_string(&finished)?, "finished");
            output.extend(exited.stdout.as_bytes());
            assert!(output.starts_with(b"PA"));
            assert!(output[2..].iter().all(|&byte| byte == b'z'));
            assert!(output.len() <= TAIL_BYTES + 2);
            assert_eq!(
                output.len(),
                TAIL_BYTES + 2,
                "backpressure must not lose output"
            );
            record(json!({"probe":"process_output","saturated":saturated,
                "expected_bytes":TAIL_BYTES+2,"received_bytes":output.len(),"missing_bytes":TAIL_BYTES+2-output.len(),
                "reported_cap_reached":exited.stdout_cap_reached,"release_to_exit_delivery_ms":start.elapsed().as_secs_f64()*1000.0}));
            harness.shutdown().await;
        }
        Ok(())
    })
}

#[test]
#[ignore = "opt-in real filesystem and backpressure probe"]
#[serial(app_server_tracing)]
fn efficiency_benchmark_fs_watch() -> Result<()> {
    run_current_thread_test_with_stack("efficiency_benchmark_fs_watch", async {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let manager = crate::fs_watch::FsWatchManager::new(Arc::clone(&outgoing));
        let gate = ConnectionRpcGate::new();
        let dir = TempDir::new()?;
        let file = dir.path().join("watched");
        std::fs::write(&file, "initial")?;
        let params = codex_app_server_protocol::FsWatchParams {
            watch_id: "original".to_string(),
            path: codex_utils_absolute_path::AbsolutePathBuf::try_from(file.clone())?,
        };
        manager
            .watch_with_gate(TEST_CONNECTION_ID, params.clone(), &gate)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        std::fs::write(&file, "baseline")?;
        let baseline = timeout(DEADLINE, rx.recv()).await?.expect("watch output");
        assert!(matches!(
            baseline,
            OutgoingEnvelope::ToConnection {
                message: OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(_)),
                ..
            }
        ));
        // Let the baseline burst settle before filling the real transport queue.
        while timeout(Duration::from_millis(250), rx.recv()).await.is_ok() {}
        fill_notifications(&outgoing);
        let start = Instant::now();
        std::fs::write(&file, "blocked")?;
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(rx.capacity(), 0);
        let _ = rx.recv().await;
        let recovered = timeout(DEADLINE, rx.recv())
            .await?
            .expect("retained watch update");
        assert!(matches!(recovered, OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(event)), ..
        } if event.watch_id == "original"));
        let duplicate = manager
            .watch_with_gate(TEST_CONNECTION_ID, params.clone(), &gate)
            .await
            .expect_err("live registration remains reserved");
        assert!(duplicate.message.contains("already exists"));
        manager
            .watch_with_gate(
                TEST_CONNECTION_ID,
                codex_app_server_protocol::FsWatchParams {
                    watch_id: "fresh".to_string(),
                    ..params
                },
                &gate,
            )
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        std::fs::write(&file, "after recovery")?;
        let mut ids = Vec::new();
        let first = timeout(DEADLINE, rx.recv())
            .await?
            .expect("fresh watcher output");
        if let OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(event)),
            ..
        } = first
        {
            ids.push(event.watch_id);
        }
        while let Ok(Some(OutgoingEnvelope::ToConnection {
            message: OutgoingMessage::AppServerNotification(ServerNotification::FsChanged(event)),
            ..
        })) = timeout(Duration::from_millis(500), rx.recv()).await
        {
            ids.push(event.watch_id);
        }
        assert!(!ids.is_empty());
        assert!(
            ids.iter().any(|id| id == "original"),
            "original watch survives recovery"
        );
        assert!(ids.iter().any(|id| id == "fresh"));
        record(
            json!({"probe":"fs_watch","transport_stall_ms":6000,"post_recovery_watch_ids":ids,
            "original_watch_alive":true,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0}),
        );
        manager.connection_closed(TEST_CONNECTION_ID).await;
        gate.shutdown().await;
        Ok(())
    })
}
