use super::ConnectionSessionState;
use super::MessageProcessor;
use super::MessageProcessorArgs;
use super::notification_log_metadata;
use super::serialized_request_queue_bytes;
use crate::analytics_utils::analytics_events_client_from_config;
use crate::config_manager::ConfigManager;
use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ConnectionCapabilities;
use crate::transport::AppServerTransport;
use anyhow::Result;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::write_mock_responses_config_toml;
use codex_analytics::AppServerRpcTransport;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::InitializeParams;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_arg0::Arg0DispatchPaths;
use codex_config::CloudConfigBundleLoader;
use codex_config::LoaderOverrides;
use codex_config::ThreadConfigContext;
use codex_config::ThreadConfigLoadError;
use codex_config::ThreadConfigLoader;
use codex_config::ThreadConfigLoaderFuture;
use codex_config::ThreadConfigSource;
use codex_core::config::Config;
use codex_core::config::ConfigBuilder;
use codex_exec_server::EnvironmentManager;
use codex_features::Feature;
use codex_feedback::CodexFeedback;
use codex_login::AuthManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::W3cTraceContext;
use opentelemetry::global;
use opentelemetry::trace::SpanId;
use opentelemetry::trace::SpanKind;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::trace::SpanData;
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde::Serializer;
use serde_json::json;
use serial_test::serial;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use wiremock::MockServer;

const TEST_CONNECTION_ID: ConnectionId = ConnectionId(7);
const SECOND_TEST_CONNECTION_ID: ConnectionId = ConnectionId(8);

#[test]
fn logging_contract_notification_metadata_is_bounded_and_omits_params() {
    let notification = JSONRPCNotification {
        method: format!("{}é", "m".repeat(127)),
        params: Some(json!({"secret": "notification secret"})),
    };

    let metadata = notification_log_metadata(TEST_CONNECTION_ID, &notification);

    assert_eq!(metadata.connection_id, TEST_CONNECTION_ID);
    assert_eq!(metadata.method, "m".repeat(127));
    assert!(metadata.method_truncated);
    assert!(metadata.params_present);
    assert!(metadata.method.len() <= 128);
    assert!(!format!("{metadata:?}").contains("notification secret"));
}

struct SerializationProbe<'a>(&'a AtomicUsize);

impl Serialize for SerializationProbe<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.fetch_add(1, Ordering::Relaxed);
        serializer.serialize_bool(true)
    }
}

#[test]
fn unqueued_request_skips_request_size_serialization() {
    let serializations = AtomicUsize::new(0);
    let probe = SerializationProbe(&serializations);

    assert_eq!(serialized_request_queue_bytes(false, &probe), 0);
    assert_eq!(serializations.load(Ordering::Relaxed), 0);
    assert!(serialized_request_queue_bytes(true, &probe) > 0);
    assert_eq!(serializations.load(Ordering::Relaxed), 1);
}

#[test]
fn queued_request_size_matches_compact_json_without_materializing_it() {
    let request = serde_json::json!({
        "input": "multibyte µ payload".repeat(1024),
        "enabled": true,
    });

    assert_eq!(
        serialized_request_queue_bytes(true, &request),
        serde_json::to_vec(&request)
            .expect("serialize comparison request")
            .len(),
    );
}

struct TestTracing {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
}

struct RemoteTrace {
    trace_id: TraceId,
    parent_span_id: SpanId,
    context: W3cTraceContext,
}

impl RemoteTrace {
    fn new(trace_id: &str, parent_span_id: &str) -> Self {
        let trace_id = TraceId::from_hex(trace_id).expect("trace id");
        let parent_span_id = SpanId::from_hex(parent_span_id).expect("parent span id");
        let context = W3cTraceContext {
            traceparent: Some(format!("00-{trace_id}-{parent_span_id}-01")),
            tracestate: Some("vendor=value".to_string()),
        };

        Self {
            trace_id,
            parent_span_id,
            context,
        }
    }
}

fn init_test_tracing() -> &'static TestTracing {
    static TEST_TRACING: OnceLock<TestTracing> = OnceLock::new();
    TEST_TRACING.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-app-server-message-processor-tests");
        global::set_text_map_propagator(TraceContextPropagator::new());
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        tracing::subscriber::set_global_default(subscriber)
            .expect("global tracing subscriber should only be installed once");
        TestTracing { exporter, provider }
    })
}

fn request_from_client_request(request: ClientRequest) -> JSONRPCRequest {
    serde_json::from_value(serde_json::to_value(request).expect("serialize client request"))
        .expect("client request should convert to JSON-RPC")
}

struct TracingHarness {
    _server: MockServer,
    _codex_home: TempDir,
    processor: Arc<MessageProcessor>,
    outgoing_rx: mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    session: Arc<ConnectionSessionState>,
    tracing: &'static TestTracing,
}

impl TracingHarness {
    async fn new() -> Result<Self> {
        Self::new_with_thread_config_loader(Arc::new(codex_config::NoopThreadConfigLoader)).await
    }

    async fn new_with_thread_config_loader(
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
    ) -> Result<Self> {
        let server = create_mock_responses_server_repeating_assistant("Done").await;
        Self::new_with_server_and_thread_config_loader(server, thread_config_loader, false).await
    }

    async fn new_with_server_and_thread_config_loader(
        server: MockServer,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
        enable_collab: bool,
    ) -> Result<Self> {
        Self::new_with_features(server, thread_config_loader, enable_collab, false).await
    }

    async fn new_with_features(
        server: MockServer,
        thread_config_loader: Arc<dyn ThreadConfigLoader>,
        enable_collab: bool,
        enable_goals: bool,
    ) -> Result<Self> {
        let codex_home = TempDir::new()?;
        let config = Arc::new(
            build_test_config(
                codex_home.path(),
                &server.uri(),
                enable_collab,
                enable_goals,
            )
            .await?,
        );
        let state_db = if enable_goals {
            Some(
                codex_state::StateRuntime::init(
                    config.sqlite_home.clone(),
                    config.model_provider_id.clone(),
                )
                .await?,
            )
        } else {
            None
        };
        let (processor, outgoing_rx) =
            build_test_processor(config, thread_config_loader, state_db).await;
        let tracing = init_test_tracing();
        tracing.exporter.reset();
        tracing::callsite::rebuild_interest_cache();
        let mut harness = Self {
            _server: server,
            _codex_home: codex_home,
            processor,
            outgoing_rx,
            session: Arc::new(ConnectionSessionState::new()),
            tracing,
        };

        let outbound_initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        harness
            .processor
            .outgoing
            .connection_opened(TEST_CONNECTION_ID, Arc::clone(&outbound_initialized))
            .await;
        let _: InitializeResponse = harness
            .request(
                ClientRequest::Initialize {
                    request_id: RequestId::Integer(1),
                    params: InitializeParams {
                        client_info: ClientInfo {
                            name: "codex-app-server-tests".to_string(),
                            title: None,
                            version: "0.1.0".to_string(),
                        },
                        capabilities: Some(InitializeCapabilities {
                            experimental_api: true,
                            ..Default::default()
                        }),
                    },
                },
                /*trace*/ None,
            )
            .await;
        assert!(harness.session.initialized());
        // process_request leaves outbound initialization to the transport loop.
        // Mirror that loop's registration so normal listener admission sees a live client.
        harness
            .processor
            .thread_processor
            .thread_state_manager
            .connection_initialized(
                TEST_CONNECTION_ID,
                ConnectionCapabilities {
                    request_attestation: harness.session.request_attestation(),
                    experimental_api: harness.session.experimental_api_enabled(),
                },
            )
            .await;
        outbound_initialized.store(true, Ordering::Release);

        Ok(harness)
    }

    fn reset_tracing(&self) {
        self.tracing.exporter.reset();
    }

    async fn shutdown(self) {
        self.processor.thread_processor.shutdown_threads().await;
        self.processor.drain_background_tasks().await;
    }

    async fn request<T>(&mut self, request: ClientRequest, trace: Option<W3cTraceContext>) -> T
    where
        T: serde::de::DeserializeOwned,
    {
        let request_id = match request.id() {
            RequestId::Integer(request_id) => *request_id,
            request_id => panic!("expected integer request id in test harness, got {request_id:?}"),
        };
        let mut request = request_from_client_request(request);
        request.trace = trace;

        self.processor
            .process_request(
                TEST_CONNECTION_ID,
                request,
                &AppServerTransport::Stdio,
                Arc::clone(&self.session),
            )
            .await;
        read_response(&mut self.outgoing_rx, request_id).await
    }

    async fn start_thread(
        &mut self,
        request_id: i64,
        trace: Option<W3cTraceContext>,
    ) -> ThreadStartResponse {
        let response = self
            .request(
                ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(request_id),
                    params: ThreadStartParams {
                        ephemeral: Some(true),
                        ..ThreadStartParams::default()
                    },
                },
                trace,
            )
            .await;
        read_thread_started_notification(&mut self.outgoing_rx).await;
        response
    }
}

async fn build_test_config(
    codex_home: &Path,
    server_uri: &str,
    enable_collab: bool,
    enable_goals: bool,
) -> Result<Config> {
    let mut feature_flags = if enable_collab {
        BTreeMap::from([(Feature::Collab, true)])
    } else {
        BTreeMap::new()
    };
    if enable_goals {
        feature_flags.insert(Feature::Goals, true);
    }
    write_mock_responses_config_toml(
        codex_home,
        server_uri,
        &feature_flags,
        /*auto_compact_limit*/ if enable_collab { 1_000_000 } else { 8_192 },
        Some(false),
        "mock_provider",
        "compact",
    )?;

    Ok(ConfigBuilder::default()
        .codex_home(codex_home.to_path_buf())
        .build()
        .await?)
}

async fn build_test_processor(
    config: Arc<Config>,
    thread_config_loader: Arc<dyn ThreadConfigLoader>,
    state_db: Option<Arc<codex_state::StateRuntime>>,
) -> (
    Arc<MessageProcessor>,
    mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
) {
    // Match the transport queue: terminal notifications intentionally use try_send.
    let (outgoing_tx, outgoing_rx) = mpsc::channel(crate::transport::CHANNEL_CAPACITY);
    let auth_manager =
        AuthManager::shared_from_config(config.as_ref(), /*enable_codex_api_key_env*/ false).await;
    let config_manager = ConfigManager::new(
        config.codex_home.to_path_buf(),
        Vec::new(),
        LoaderOverrides::default(),
        /*strict_config*/ false,
        CloudConfigBundleLoader::default(),
        Arg0DispatchPaths::default(),
        thread_config_loader,
    );
    let analytics_events_client =
        analytics_events_client_from_config(Arc::clone(&auth_manager), config.as_ref());
    let outgoing = Arc::new(OutgoingMessageSender::new(
        outgoing_tx,
        analytics_events_client.clone(),
    ));
    let processor = Arc::new(MessageProcessor::new(MessageProcessorArgs {
        outgoing,
        analytics_events_client,
        arg0_paths: Arg0DispatchPaths::default(),
        config,
        config_manager,
        environment_manager: Arc::new(EnvironmentManager::default_for_tests()),
        feedback: CodexFeedback::new(),
        log_db: None,
        state_db,
        config_warnings: Vec::new(),
        session_source: SessionSource::VSCode,
        auth_manager,
        installation_id: "11111111-1111-4111-8111-111111111111".to_string(),
        rpc_transport: AppServerRpcTransport::Stdio,
        remote_control_handle: None,
        plugin_startup_tasks: crate::PluginStartupTasks::Start,
    }));
    (processor, outgoing_rx)
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

struct BlockingThreadConfigLoader {
    entered: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
    dropped: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl ThreadConfigLoader for BlockingThreadConfigLoader {
    fn load(
        &self,
        _context: ThreadConfigContext,
    ) -> ThreadConfigLoaderFuture<'_, Vec<ThreadConfigSource>> {
        let entered = self
            .entered
            .lock()
            .expect("entered signal lock should not be poisoned")
            .take();
        let dropped = self
            .dropped
            .lock()
            .expect("dropped signal lock should not be poisoned")
            .take();
        Box::pin(async move {
            let _drop_signal = DropSignal(dropped);
            if let Some(sender) = entered {
                let _ = sender.send(());
            }
            std::future::pending::<Result<Vec<ThreadConfigSource>, ThreadConfigLoadError>>().await
        })
    }
}

fn run_current_thread_test_with_stack<F>(name: &str, future: F) -> Result<()>
where
    F: Future<Output = Result<()>> + Send + 'static,
{
    // Match the larger stack used by the other async integration harnesses.
    // The fully instrumented thread/start path exceeds 4 MiB on Windows.
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(move || -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(Box::pin(future))
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("{name} thread panicked")),
    }
}

#[test]
#[serial(app_server_tracing)]
fn connection_close_cancels_in_progress_thread_start_before_post_close_response() -> Result<()> {
    run_current_thread_test_with_stack(
        "connection_close_cancels_in_progress_thread_start_before_post_close_response",
        async {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
            let loader = Arc::new(BlockingThreadConfigLoader {
                entered: StdMutex::new(Some(entered_tx)),
                dropped: StdMutex::new(Some(dropped_tx)),
            });
            let mut harness = TracingHarness::new_with_thread_config_loader(loader).await?;
            let request = request_from_client_request(ClientRequest::ThreadStart {
                request_id: RequestId::Integer(40_001),
                params: ThreadStartParams {
                    ephemeral: Some(true),
                    ..ThreadStartParams::default()
                },
            });

            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request,
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
                .await
                .expect("thread config load should start")
                .expect("thread config load signal should be sent");

            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                harness
                    .processor
                    .connection_closed(TEST_CONNECTION_ID, &harness.session),
            )
            .await
            .expect("connection close should cancel the in-progress thread start");
            tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
                .await
                .expect("thread config load should be dropped on connection close")
                .expect("thread config drop signal should be sent");

            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    harness.outgoing_rx.recv(),
                )
                .await
                .is_err(),
                "canceled thread start must not send a response or thread/started notification"
            );

            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn thread_created_lag_resync_uses_only_missed_broadcast_instances() -> Result<()> {
    run_current_thread_test_with_stack(
        "thread_created_lag_resync_uses_only_missed_broadcast_instances",
        async {
            const CHILD_PROMPT: &str = "child: exercise thread-created lag recovery";
            let spawn_args = serde_json::to_string(&json!({
                "message": CHILD_PROMPT,
                "task_name": "lag_child",
            }))?;
            let spawn_response = core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("resp-spawn"),
                core_test_support::responses::ev_function_call_with_namespace(
                    "spawn-call",
                    "agents",
                    "spawn_agent",
                    &spawn_args,
                ),
                core_test_support::responses::ev_completed("resp-spawn"),
            ]);
            let final_response = core_test_support::responses::sse(vec![
                core_test_support::responses::ev_response_created("resp-final"),
                core_test_support::responses::ev_assistant_message("msg-final", "Done"),
                core_test_support::responses::ev_completed("resp-final"),
            ]);
            let server = create_mock_responses_server_sequence_unchecked(vec![
                spawn_response,
                final_response.clone(),
                final_response,
            ])
            .await;
            let mut harness = TracingHarness::new_with_server_and_thread_config_loader(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                true,
            )
            .await?;
            // The tracing harness drives `process_request` directly and therefore does
            // not execute the transport loop's post-initialize connection registration.
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: false,
                        experimental_api: true,
                    },
                )
                .await;
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    SECOND_TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: false,
                        experimental_api: false,
                    },
                )
                .await;

            let top_level_thread = harness
                .start_thread(/*request_id*/ 30_001, /*trace*/ None)
                .await;
            let top_level_thread_id = ThreadId::from_string(&top_level_thread.thread.id)?;
            assert!(
                harness
                    .processor
                    .thread_processor
                    .handle_thread_created_event(Ok(top_level_thread_id), vec![TEST_CONNECTION_ID])
                    .await
            );

            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .subscribed_connection_ids(top_level_thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID]),
                "lag recovery must not attach unrelated top-level threads to other connections"
            );

            let _: TurnStartResponse = harness
                .request(
                    ClientRequest::TurnStart {
                        request_id: RequestId::Integer(30_002),
                        params: TurnStartParams {
                            thread_id: top_level_thread.thread.id.clone(),
                            input: vec![UserInput::Text {
                                text: "spawn a child".to_string(),
                                text_elements: Vec::new(),
                            }],
                            ..TurnStartParams::default()
                        },
                    },
                    None,
                )
                .await;
            let missed_thread_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(thread_id) = harness
                        .processor
                        .thread_processor
                        .thread_created_ids_for_test()
                        .await
                        .into_iter()
                        .next()
                    {
                        break thread_id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("spawned child should enter the thread-created recovery registry");
            assert!(
                harness
                    .processor
                    .thread_processor
                    .handle_thread_created_event(Ok(missed_thread_id), vec![TEST_CONNECTION_ID])
                    .await
            );
            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .subscribed_connection_ids(missed_thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID]),
                "the initial child event should attach only its current connection"
            );
            let (thread_created_tx, mut thread_created_rx) = tokio::sync::broadcast::channel(1);
            thread_created_tx.send(top_level_thread_id)?;
            thread_created_tx.send(missed_thread_id)?;
            let lagged = thread_created_rx.recv().await;
            assert!(matches!(
                &lagged,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(1))
            ));
            assert!(
                harness
                    .processor
                    .thread_processor
                    .handle_thread_created_event(
                        lagged,
                        vec![TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID],
                    )
                    .await
            );
            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .subscribed_connection_ids(missed_thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID]),
                "lag recovery must retry an already-handled child for newly initialized connections"
            );
            let queued = thread_created_rx.recv().await;
            assert_eq!(queued, Ok(missed_thread_id));
            assert!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .is_connection_initialized(SECOND_TEST_CONNECTION_ID)
                    .await,
                "the synthetic second connection must remain initialized"
            );
            assert!(
                harness
                    .processor
                    .thread_processor
                    .handle_thread_created_event(
                        queued,
                        vec![TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID],
                    )
                    .await
            );
            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .subscribed_connection_ids(missed_thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID]),
                "lag recovery must attach every initialized connection to a missed broadcast instance"
            );

            harness.shutdown().await;
            Ok(())
        },
    )
}

fn span_attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a str> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            opentelemetry::Value::String(value) => Some(value.as_str()),
            _ => None,
        })
}

fn find_rpc_span_with_trace<'a>(
    spans: &'a [SpanData],
    kind: SpanKind,
    method: &str,
    trace_id: TraceId,
) -> &'a SpanData {
    spans
        .iter()
        .find(|span| {
            span.span_kind == kind
                && span_attr(span, "rpc.system") == Some("jsonrpc")
                && span_attr(span, "rpc.method") == Some(method)
                && span.span_context.trace_id() == trace_id
        })
        .unwrap_or_else(|| {
            panic!(
                "missing {kind:?} span for rpc.method={method} trace={trace_id}; exported spans:\n{}",
                format_spans(spans)
            )
        })
}

fn find_span_with_trace<'a, F>(
    spans: &'a [SpanData],
    trace_id: TraceId,
    description: &str,
    predicate: F,
) -> &'a SpanData
where
    F: Fn(&SpanData) -> bool,
{
    spans
        .iter()
        .find(|span| span.span_context.trace_id() == trace_id && predicate(span))
        .unwrap_or_else(|| {
            panic!(
                "missing span matching {description} for trace={trace_id}; exported spans:\n{}",
                format_spans(spans)
            )
        })
}

fn format_spans(spans: &[SpanData]) -> String {
    spans
        .iter()
        .map(|span| {
            let rpc_method = span_attr(span, "rpc.method").unwrap_or("-");
            format!(
                "name={} span_id={} kind={:?} parent={} trace={} rpc.method={}",
                span.name,
                span.span_context.span_id(),
                span.span_kind,
                span.parent_span_id,
                span.span_context.trace_id(),
                rpc_method
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn span_depth_from_ancestor(
    spans: &[SpanData],
    child: &SpanData,
    ancestor: &SpanData,
) -> Option<usize> {
    let ancestor_span_id = ancestor.span_context.span_id();
    let mut parent_span_id = child.parent_span_id;
    let mut depth = 1;
    while parent_span_id != SpanId::INVALID {
        if parent_span_id == ancestor_span_id {
            return Some(depth);
        }
        let Some(parent_span) = spans
            .iter()
            .find(|span| span.span_context.span_id() == parent_span_id)
        else {
            break;
        };
        parent_span_id = parent_span.parent_span_id;
        depth += 1;
    }

    None
}

fn assert_span_descends_from(spans: &[SpanData], child: &SpanData, ancestor: &SpanData) {
    if span_depth_from_ancestor(spans, child, ancestor).is_some() {
        return;
    }

    panic!(
        "span {} does not descend from {}; exported spans:\n{}",
        child.name,
        ancestor.name,
        format_spans(spans)
    );
}

fn assert_has_internal_descendant_at_min_depth(
    spans: &[SpanData],
    ancestor: &SpanData,
    min_depth: usize,
) {
    if spans.iter().any(|span| {
        span.span_kind == SpanKind::Internal
            && span.span_context.trace_id() == ancestor.span_context.trace_id()
            && span_depth_from_ancestor(spans, span, ancestor)
                .is_some_and(|depth| depth >= min_depth)
    }) {
        return;
    }

    panic!(
        "missing internal descendant at depth >= {min_depth} below {}; exported spans:\n{}",
        ancestor.name,
        format_spans(spans)
    );
}

async fn read_response<T: serde::de::DeserializeOwned>(
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
    request_id: i64,
) -> T {
    loop {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), outgoing_rx.recv())
            .await
            .expect("timed out waiting for response")
            .expect("outgoing channel closed");
        let crate::outgoing_message::OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            continue;
        };
        if connection_id != TEST_CONNECTION_ID {
            continue;
        }
        if let crate::outgoing_message::OutgoingMessage::Error(error) = &message
            && error.id == RequestId::Integer(request_id)
        {
            panic!(
                "expected successful response for request {request_id}, got: {:?}",
                error.error
            );
        }
        let crate::outgoing_message::OutgoingMessage::Response(response) = message else {
            continue;
        };
        if response.id != RequestId::Integer(request_id) {
            continue;
        }
        return serde_json::from_value(response.result)
            .expect("response payload should deserialize");
    }
}

async fn read_thread_started_notification(
    outgoing_rx: &mut mpsc::Receiver<crate::outgoing_message::OutgoingEnvelope>,
) {
    loop {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), outgoing_rx.recv())
            .await
            .expect("timed out waiting for thread/started notification")
            .expect("outgoing channel closed");
        match envelope {
            crate::outgoing_message::OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } => {
                if connection_id != TEST_CONNECTION_ID {
                    continue;
                }
                let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) =
                    message
                else {
                    continue;
                };
                if matches!(
                    notification,
                    codex_app_server_protocol::ServerNotification::ThreadStarted(_)
                ) {
                    return;
                }
            }
            crate::outgoing_message::OutgoingEnvelope::Broadcast { message } => {
                let crate::outgoing_message::OutgoingMessage::AppServerNotification(notification) =
                    message
                else {
                    continue;
                };
                if matches!(
                    notification,
                    codex_app_server_protocol::ServerNotification::ThreadStarted(_)
                ) {
                    return;
                }
            }
        }
    }
}

async fn wait_for_exported_spans<F>(tracing: &TestTracing, predicate: F) -> Vec<SpanData>
where
    F: Fn(&[SpanData]) -> bool,
{
    let mut last_spans = Vec::new();
    for _ in 0..200 {
        tokio::task::yield_now().await;
        tracing
            .provider
            .force_flush()
            .expect("force flush should succeed");
        let spans = tracing.exporter.get_finished_spans().expect("span export");
        last_spans = spans.clone();
        if predicate(&spans) {
            return spans;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    panic!(
        "timed out waiting for expected exported spans:\n{}",
        format_spans(&last_spans)
    );
}

async fn wait_for_new_exported_spans<F>(
    tracing: &TestTracing,
    baseline_len: usize,
    predicate: F,
) -> Vec<SpanData>
where
    F: Fn(&[SpanData]) -> bool,
{
    let spans = wait_for_exported_spans(tracing, |spans| {
        spans.len() > baseline_len && predicate(&spans[baseline_len..])
    })
    .await;
    spans.into_iter().skip(baseline_len).collect()
}

#[test]
#[serial(app_server_tracing)]
fn thread_start_jsonrpc_span_exports_server_span_and_parents_children() -> Result<()> {
    run_current_thread_test_with_stack(
        "thread_start_jsonrpc_span_exports_server_span_and_parents_children",
        async {
            let mut harness = TracingHarness::new().await?;

            let RemoteTrace {
                trace_id: remote_trace_id,
                parent_span_id: remote_parent_span_id,
                context: remote_trace,
                ..
            } = RemoteTrace::new("00000000000000000000000000000011", "0000000000000022");

            let _: ThreadStartResponse = harness
                .start_thread(/*request_id*/ 20_002, /*trace*/ None)
                .await;
            let untraced_spans = wait_for_exported_spans(harness.tracing, |spans| {
                spans.iter().any(|span| {
                    span.span_kind == SpanKind::Server
                        && span_attr(span, "rpc.method") == Some("thread/start")
                })
            })
            .await;
            let untraced_server_span = find_rpc_span_with_trace(
                &untraced_spans,
                SpanKind::Server,
                "thread/start",
                untraced_spans
                    .iter()
                    .rev()
                    .find(|span| {
                        span.span_kind == SpanKind::Server
                            && span_attr(span, "rpc.system") == Some("jsonrpc")
                            && span_attr(span, "rpc.method") == Some("thread/start")
                    })
                    .unwrap_or_else(|| {
                        panic!(
                            "missing latest thread/start server span; exported spans:\n{}",
                            format_spans(&untraced_spans)
                        )
                    })
                    .span_context
                    .trace_id(),
            );
            assert_has_internal_descendant_at_min_depth(
                &untraced_spans,
                untraced_server_span,
                /*min_depth*/ 1,
            );

            let baseline_len = untraced_spans.len();
            let _: ThreadStartResponse = harness
                .start_thread(/*request_id*/ 20_003, Some(remote_trace))
                .await;
            let spans = wait_for_new_exported_spans(harness.tracing, baseline_len, |spans| {
                spans.iter().any(|span| {
                    span.span_kind == SpanKind::Server
                        && span_attr(span, "rpc.method") == Some("thread/start")
                        && span.span_context.trace_id() == remote_trace_id
                }) && spans.iter().any(|span| {
                    span.name.as_ref() == "app_server.thread_start.notify_started"
                        && span.span_context.trace_id() == remote_trace_id
                })
            })
            .await;

            let server_request_span =
                find_rpc_span_with_trace(&spans, SpanKind::Server, "thread/start", remote_trace_id);
            assert_eq!(server_request_span.name.as_ref(), "thread/start");
            assert_eq!(server_request_span.parent_span_id, remote_parent_span_id);
            assert!(server_request_span.parent_span_is_remote);
            assert_eq!(server_request_span.span_context.trace_id(), remote_trace_id);
            assert_ne!(server_request_span.span_context.span_id(), SpanId::INVALID);
            assert_has_internal_descendant_at_min_depth(
                &spans,
                server_request_span,
                /*min_depth*/ 1,
            );
            assert_has_internal_descendant_at_min_depth(
                &spans,
                server_request_span,
                /*min_depth*/ 2,
            );
            harness.shutdown().await;

            Ok(())
        },
    )
}

#[tokio::test(flavor = "current_thread")]
#[serial(app_server_tracing)]
async fn turn_start_jsonrpc_span_parents_core_turn_spans() -> Result<()> {
    let mut harness = TracingHarness::new().await?;
    let thread_start_response = harness.start_thread(/*request_id*/ 2, /*trace*/ None).await;
    let thread_id = thread_start_response.thread.id.clone();

    harness.reset_tracing();

    let RemoteTrace {
        trace_id: remote_trace_id,
        parent_span_id: remote_parent_span_id,
        context: remote_trace,
    } = RemoteTrace::new("00000000000000000000000000000077", "0000000000000088");
    let turn_start_response: TurnStartResponse = harness
        .request(
            ClientRequest::TurnStart {
                request_id: RequestId::Integer(3),
                params: TurnStartParams {
                    environments: None,
                    thread_id,
                    client_user_message_id: None,
                    run_independently: None,
                    input: vec![UserInput::Text {
                        text: "hello".to_string(),
                        text_elements: Vec::new(),
                    }],
                    responsesapi_client_metadata: None,
                    additional_context: None,
                    cwd: None,
                    runtime_workspace_roots: None,
                    approval_policy: None,
                    sandbox_policy: None,
                    permission_profile: None,
                    permissions: None,
                    approvals_reviewer: None,
                    model: None,
                    service_tier: None,
                    effort: None,
                    summary: None,
                    personality: None,
                    output_schema: None,
                    collaboration_mode: None,
                },
            },
            Some(remote_trace),
        )
        .await;
    let spans = wait_for_exported_spans(harness.tracing, |spans| {
        spans.iter().any(|span| {
            span.span_kind == SpanKind::Server
                && span_attr(span, "rpc.method") == Some("turn/start")
                && span.span_context.trace_id() == remote_trace_id
        }) && spans.iter().any(|span| {
            span_attr(span, "codex.op") == Some("user_input")
                && span.span_context.trace_id() == remote_trace_id
        })
    })
    .await;

    let server_request_span =
        find_rpc_span_with_trace(&spans, SpanKind::Server, "turn/start", remote_trace_id);
    let core_turn_span =
        find_span_with_trace(&spans, remote_trace_id, "codex.op=user_input", |span| {
            span_attr(span, "codex.op") == Some("user_input")
        });

    assert_eq!(server_request_span.parent_span_id, remote_parent_span_id);
    assert!(server_request_span.parent_span_is_remote);
    assert_eq!(server_request_span.span_context.trace_id(), remote_trace_id);
    assert_eq!(
        span_attr(server_request_span, "turn.id"),
        Some(turn_start_response.turn.id.as_str())
    );
    assert_span_descends_from(&spans, core_turn_span, server_request_span);
    harness.shutdown().await;

    Ok(())
}

#[test]
#[serial(app_server_tracing)]
fn rollback_request_cancellation_releases_reservation_before_retry() -> Result<()> {
    run_current_thread_test_with_stack(
        "rollback_request_cancellation_releases_reservation_before_retry",
        async {
            use crate::outgoing_message::OutgoingEnvelope;
            use crate::outgoing_message::OutgoingMessage;
            use codex_app_server_protocol::ServerNotification;
            use codex_app_server_protocol::ThreadRollbackParams;
            use codex_app_server_protocol::ThreadRollbackResponse;
            const CANCELLED_REQUEST: i64 = 40_020;

            async fn response_for(
                rx: &mut mpsc::Receiver<OutgoingEnvelope>,
                connection: ConnectionId,
                id: i64,
            ) -> serde_json::Value {
                loop {
                    let envelope =
                        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                            .await
                            .expect("response deadline")
                            .expect("outgoing open");
                    let OutgoingEnvelope::ToConnection {
                        connection_id,
                        message,
                        ..
                    } = envelope
                    else {
                        continue;
                    };
                    match message {
                        OutgoingMessage::Response(response) => {
                            assert_ne!(
                                response.id,
                                RequestId::Integer(CANCELLED_REQUEST),
                                "cancelled rollback responded"
                            );
                            if connection_id == connection && response.id == RequestId::Integer(id)
                            {
                                return response.result;
                            }
                        }
                        OutgoingMessage::Error(error) => {
                            assert_ne!(
                                error.id,
                                RequestId::Integer(CANCELLED_REQUEST),
                                "cancelled rollback failed after close"
                            );
                            assert!(
                                connection_id != connection || error.id != RequestId::Integer(id),
                                "request failed: {error:?}"
                            );
                        }
                        _ => {}
                    }
                }
            }

            let mut harness = TracingHarness::new().await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(40_001),
                        params: ThreadStartParams {
                            ephemeral: Some(false),
                            ..Default::default()
                        },
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = started.thread.id;
            let parsed_thread_id = ThreadId::from_string(&thread_id)?;
            let manager = &harness.processor.thread_processor.thread_state_manager;
            assert_eq!(
                manager.subscribed_connection_ids(parsed_thread_id).await,
                vec![TEST_CONNECTION_ID]
            );
            assert!(
                manager
                    .current_listener_command_tx(parsed_thread_id)
                    .is_some()
            );
            let mut turn_ids = Vec::new();
            for (index, text) in ["first retained turn", "last removable turn"]
                .into_iter()
                .enumerate()
            {
                let request_id = RequestId::Integer(40_002 + index as i64);
                harness
                    .processor
                    .process_request(
                        TEST_CONNECTION_ID,
                        request_from_client_request(ClientRequest::TurnStart {
                            request_id: request_id.clone(),
                            params: serde_json::from_value(json!({
                                "threadId": thread_id,
                                "input": [{"type": "text", "text": text, "textElements": []}]
                            }))?,
                        }),
                        &AppServerTransport::Stdio,
                        Arc::clone(&harness.session),
                    )
                    .await;
                // Core execution can finish before the turn/start response is sent.
                // Retain both messages instead of discarding notifications while
                // waiting for the response in the generic tracing helper.
                let mut started_turn = None;
                let mut completed_turn = None;
                while started_turn.is_none() || completed_turn.is_none() {
                    let envelope = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        harness.outgoing_rx.recv(),
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "turn setup deadline: {error}; response={started_turn:?}; completion={completed_turn:?}"
                        )
                    })
                    .expect("outgoing open");
                    let message = match envelope {
                        OutgoingEnvelope::ToConnection {
                            connection_id,
                            message,
                            write_complete_tx,
                        } => {
                            assert_eq!(connection_id, TEST_CONNECTION_ID);
                            if let Some(receipt) = write_complete_tx {
                                receipt.send(()).expect("transport receipt receiver");
                            }
                            message
                        }
                        OutgoingEnvelope::Broadcast { message } => message,
                    };
                    match message {
                        OutgoingMessage::Response(response) => {
                            assert_eq!(response.id, request_id);
                            let response: TurnStartResponse =
                                serde_json::from_value(response.result)?;
                            assert!(started_turn.replace(response.turn.id).is_none());
                        }
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::TurnCompleted(completed),
                        ) => {
                            assert_eq!(completed.thread_id, thread_id);
                            assert_eq!(
                                completed.turn.status,
                                codex_app_server_protocol::TurnStatus::Completed
                            );
                            assert!(completed_turn.replace(completed.turn.id).is_none());
                        }
                        OutgoingMessage::Error(error) => {
                            panic!("turn setup RPC failed: {error:?}");
                        }
                        OutgoingMessage::AppServerNotification(ServerNotification::Error(
                            error,
                        )) => {
                            panic!("turn setup failed: {error:?}");
                        }
                        OutgoingMessage::Request(request) => {
                            panic!("unexpected model setup client request: {request:?}");
                        }
                        _ => {}
                    }
                }
                let turn_id = started_turn.expect("turn/start response");
                assert_eq!(completed_turn.as_deref(), Some(turn_id.as_str()));
                turn_ids.push(turn_id);
            }

            let live_session = Arc::new(ConnectionSessionState::new());
            let live_outbound_initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
            harness
                .processor
                .outgoing
                .connection_opened(
                    SECOND_TEST_CONNECTION_ID,
                    Arc::clone(&live_outbound_initialized),
                )
                .await;
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::Initialize {
                        request_id: RequestId::Integer(40_010),
                        params: InitializeParams {
                            client_info: ClientInfo {
                                name: "codex-app-server-tests".to_string(),
                                title: None,
                                version: "0.1.0".to_string(),
                            },
                            capabilities: Some(InitializeCapabilities {
                                experimental_api: true,
                                ..Default::default()
                            }),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&live_session),
                )
                .await;
            response_for(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 40_010).await;
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    SECOND_TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: live_session.request_attestation(),
                        experimental_api: live_session.experimental_api_enabled(),
                    },
                )
                .await;
            live_outbound_initialized.store(true, Ordering::Release);
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadResume {
                        request_id: RequestId::Integer(40_011),
                        params: serde_json::from_value(json!({"threadId": thread_id}))?,
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&live_session),
                )
                .await;
            response_for(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 40_011).await;

            let state = harness
                .processor
                .thread_processor
                .thread_state_manager
                .thread_state(ThreadId::from_string(&thread_id)?)
                .await;
            // Hold the existing state mutex until normal registration finishes,
            // then hold the external trace-context mutex across core submission.
            let state_guard = state.lock().await;
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadRollback {
                        request_id: RequestId::Integer(CANCELLED_REQUEST),
                        params: ThreadRollbackParams {
                            thread_id: thread_id.clone(),
                            num_turns: 1,
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            let outgoing = Arc::clone(&harness.processor.outgoing);
            let trace_guard = outgoing.lock_request_contexts_for_test().await;
            drop(state_guard);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !state.lock().await.has_pending_rollback() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("registered rollback reserved before trace lookup");

            let processor = Arc::clone(&harness.processor);
            let closing_session = Arc::clone(&harness.session);
            let close = tokio::spawn(async move {
                processor
                    .connection_closed(TEST_CONNECTION_ID, &closing_session)
                    .await;
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while state.lock().await.has_pending_rollback() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancellation must invalidate reservation while trace mutex is held");
            drop(trace_guard);
            tokio::time::timeout(std::time::Duration::from_secs(5), close)
                .await
                .expect("connection closes")
                .expect("close task");

            for (request_id, remaining) in [(40_021, 1), (40_022, 0)] {
                harness
                    .processor
                    .process_request(
                        SECOND_TEST_CONNECTION_ID,
                        request_from_client_request(ClientRequest::ThreadRollback {
                            request_id: RequestId::Integer(request_id),
                            params: ThreadRollbackParams {
                                thread_id: thread_id.clone(),
                                num_turns: 1,
                            },
                        }),
                        &AppServerTransport::Stdio,
                        Arc::clone(&live_session),
                    )
                    .await;
                let response: ThreadRollbackResponse = serde_json::from_value(
                    response_for(
                        &mut harness.outgoing_rx,
                        SECOND_TEST_CONNECTION_ID,
                        request_id,
                    )
                    .await,
                )?;
                assert_eq!(response.thread.id, thread_id);
                assert_eq!(
                    response.thread.turns.len(),
                    remaining,
                    "cancelled request must never submit a duplicate rollback"
                );
                if remaining == 1 {
                    assert_eq!(response.thread.turns[0].id, turn_ids[0]);
                }
                assert!(!state.lock().await.has_pending_rollback());
            }
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn archive_cleanup_survives_origin_disconnect_after_store_commit() -> Result<()> {
    run_current_thread_test_with_stack(
        "archive_cleanup_survives_origin_disconnect_after_store_commit",
        async {
            use crate::outgoing_message::OutgoingEnvelope;
            use crate::outgoing_message::OutgoingMessage;
            use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
            use codex_app_server_protocol::FileChangeRequestApprovalParams;
            use codex_app_server_protocol::ServerNotification;
            use codex_app_server_protocol::ServerRequestPayload;
            use codex_app_server_protocol::ThreadArchiveParams;
            use codex_thread_store::ReadThreadParams;
            use std::time::Duration;

            async fn response_for_connection(
                rx: &mut mpsc::Receiver<OutgoingEnvelope>,
                connection: ConnectionId,
                request_id: i64,
            ) {
                loop {
                    let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .unwrap_or_else(|error| {
                            panic!("response deadline for connection {connection:?}, request {request_id}: {error}")
                        })
                        .expect("outgoing open");
                    if let OutgoingEnvelope::ToConnection {
                        connection_id,
                        message,
                        ..
                    } = envelope
                    {
                        match message {
                            OutgoingMessage::Response(response)
                                if connection_id == connection
                                    && response.id == RequestId::Integer(request_id) =>
                            {
                                return;
                            }
                            OutgoingMessage::Error(error)
                                if connection_id == connection
                                    && error.id == RequestId::Integer(request_id) =>
                            {
                                panic!("request failed: {error:?}");
                            }
                            _ => {}
                        }
                    }
                }
            }

            let mut harness = TracingHarness::new().await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(50_001),
                        params: ThreadStartParams {
                            ephemeral: Some(false),
                            ..Default::default()
                        },
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = ThreadId::from_string(&started.thread.id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            thread.ensure_rollout_materialized().await;
            thread.flush_rollout().await?;
            let rollout_path = thread.rollout_path().expect("materialized rollout path");
            assert!(rollout_path.is_file());
            let config = thread.config().await;
            let store = codex_core::thread_store_from_config(config.as_ref(), None);
            assert!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: false,
                        include_history: false,
                    })
                    .await?
                    .archived_at
                    .is_none()
            );

            let remaining_session = Arc::new(ConnectionSessionState::new());
            let remaining_outbound_initialized =
                Arc::new(std::sync::atomic::AtomicBool::new(false));
            harness
                .processor
                .outgoing
                .connection_opened(
                    SECOND_TEST_CONNECTION_ID,
                    Arc::clone(&remaining_outbound_initialized),
                )
                .await;
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::Initialize {
                        request_id: RequestId::Integer(50_002),
                        params: InitializeParams {
                            client_info: ClientInfo {
                                name: "codex-app-server-tests".to_string(),
                                title: None,
                                version: "0.1.0".to_string(),
                            },
                            capabilities: Some(InitializeCapabilities {
                                experimental_api: true,
                                ..Default::default()
                            }),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&remaining_session),
                )
                .await;
            response_for_connection(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 50_002)
                .await;
            assert!(remaining_session.initialized());
            // JSON request dispatch leaves transport readiness to lib.rs. Complete
            // that same registration before sending the surviving client's resume.
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    SECOND_TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: remaining_session.request_attestation(),
                        experimental_api: remaining_session.experimental_api_enabled(),
                    },
                )
                .await;
            remaining_outbound_initialized.store(true, Ordering::Release);
            for connection in [TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID] {
                assert!(
                    harness
                        .processor
                        .thread_processor
                        .thread_state_manager
                        .is_connection_initialized(connection)
                        .await,
                    "archive setup requires live transport registration for {connection:?}"
                );
            }
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadResume {
                        request_id: RequestId::Integer(50_003),
                        params: serde_json::from_value(json!({"threadId": thread_id.to_string()}))?,
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&remaining_session),
                )
                .await;
            response_for_connection(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 50_003)
                .await;
            let manager = harness
                .processor
                .thread_processor
                .thread_state_manager
                .clone();
            assert_eq!(
                manager
                    .subscribed_connection_ids(thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID]),
            );
            let original_state = manager.thread_state(thread_id).await;
            assert!(original_state.lock().await.listener_matches(&thread));
            let listener_sender = manager
                .current_listener_command_tx(thread_id)
                .expect("normal listener route");
            let outgoing = Arc::clone(&harness.processor.outgoing);
            let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
                Arc::clone(&outgoing),
                vec![TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID],
                thread_id,
            );
            let (approval_id, mut approval_result) = thread_outgoing
                .send_request(ServerRequestPayload::FileChangeRequestApproval(
                    FileChangeRequestApprovalParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "archive-turn".to_string(),
                        item_id: "archive-approval".to_string(),
                        started_at_ms: 0,
                        reason: None,
                        grant_root: None,
                    },
                ))
                .await;
            assert_eq!(
                outgoing.pending_requests_for_thread(thread_id).await.len(),
                1
            );
            let mut approval_recipients = HashSet::new();
            while approval_recipients.len() < 2 {
                let envelope =
                    tokio::time::timeout(Duration::from_secs(5), harness.outgoing_rx.recv())
                        .await
                        .expect("approval delivery deadline")
                        .expect("outgoing open");
                if let OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    ..
                } = envelope
                    && request.id() == &approval_id
                {
                    approval_recipients.insert(connection_id);
                }
            }
            assert_eq!(
                approval_recipients,
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID])
            );

            let release_shutdown = codex_core::test_support::block_thread_terminal_tasks(&thread);
            assert_eq!(
                thread.acquire_out_of_band_elicitation_lease(
                    codex_core::OutOfBandElicitationLeaseId::new(
                        TEST_CONNECTION_ID.0,
                        "archive-shutdown-observer".to_string()
                    ),
                )?,
                1
            );
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadArchive {
                        request_id: RequestId::Integer(50_004),
                        params: ThreadArchiveParams {
                            thread_id: thread_id.to_string(),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while thread.active_out_of_band_elicitation_lease_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("archive owner must enter real post-commit shutdown");
            assert!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await?
                    .archived_at
                    .is_some()
            );
            assert!(
                !rollout_path.exists(),
                "store commit must move the active rollout"
            );
            assert!(
                harness
                    .processor
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .is_err()
            );
            assert!(matches!(
                approval_result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));

            tokio::time::timeout(
                Duration::from_secs(5),
                harness
                    .processor
                    .connection_closed(TEST_CONNECTION_ID, &harness.session),
            )
            .await
            .expect("origin RPC cancellation must not wait for archive completion");
            assert_eq!(
                manager.subscribed_connection_ids(thread_id).await,
                vec![SECOND_TEST_CONNECTION_ID]
            );
            assert_eq!(
                outgoing.pending_requests_for_thread(thread_id).await.len(),
                1,
                "remaining authorized connection keeps approval pending until archive cleanup"
            );
            assert!(matches!(
                approval_result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            release_shutdown
                .send(())
                .expect("real terminal task remains blocked");
            tokio::time::timeout(Duration::from_secs(5), thread.wait_until_terminated())
                .await
                .expect("archived core thread must terminate");
            assert!(
                tokio::time::timeout(Duration::from_secs(5), approval_result)
                    .await
                    .expect("archive must cancel the pending approval callback")
                    .is_err()
            );
            let archived = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let envelope = harness.outgoing_rx.recv().await.expect("outgoing open");
                    let message = match envelope {
                        OutgoingEnvelope::Broadcast { message } => message,
                        OutgoingEnvelope::ToConnection {
                            connection_id,
                            message,
                            ..
                        } if connection_id == SECOND_TEST_CONNECTION_ID => message,
                        OutgoingEnvelope::ToConnection { .. } => continue,
                    };
                    if let OutgoingMessage::AppServerNotification(
                        ServerNotification::ThreadArchived(archived),
                    ) = message
                    {
                        break archived;
                    }
                }
            })
            .await
            .expect("surviving connection must be notified of the committed archive");
            assert_eq!(archived.thread_id, thread_id.to_string());
            assert!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await?
                    .archived_at
                    .is_some()
            );
            assert!(
                harness
                    .processor
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .is_err()
            );
            assert!(
                outgoing
                    .pending_requests_for_thread(thread_id)
                    .await
                    .is_empty()
            );
            assert!(
                manager
                    .subscribed_connection_ids(thread_id)
                    .await
                    .is_empty()
            );
            assert!(manager.current_listener_command_tx(thread_id).is_none());
            assert!(!original_state.lock().await.listener_matches(&thread));
            assert!(!Arc::ptr_eq(
                &original_state,
                &manager.thread_state(thread_id).await
            ));
            tokio::time::timeout(Duration::from_secs(5), listener_sender.closed())
                .await
                .expect("stale listener must stop");
            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_watch_manager
                    .loaded_status_for_thread(&thread_id.to_string())
                    .await,
                codex_app_server_protocol::ThreadStatus::NotLoaded,
            );
            assert!(
                manager
                    .is_connection_initialized(SECOND_TEST_CONNECTION_ID)
                    .await
            );
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn delete_cleanup_survives_origin_disconnect_after_store_commit() -> Result<()> {
    run_current_thread_test_with_stack(
        "delete_cleanup_survives_origin_disconnect_after_store_commit",
        async {
            use crate::outgoing_message::OutgoingEnvelope;
            use crate::outgoing_message::OutgoingMessage;
            use crate::outgoing_message::ThreadScopedOutgoingMessageSender;
            use codex_app_server_protocol::FileChangeRequestApprovalParams;
            use codex_app_server_protocol::ServerNotification;
            use codex_app_server_protocol::ServerRequestPayload;
            use codex_app_server_protocol::ThreadDeleteParams;
            use codex_thread_store::ReadThreadParams;
            use std::time::Duration;

            async fn response_for_connection(
                rx: &mut mpsc::Receiver<OutgoingEnvelope>,
                connection: ConnectionId,
                request_id: i64,
            ) {
                loop {
                    let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .unwrap_or_else(|error| {
                            panic!("response deadline for connection {connection:?}, request {request_id}: {error}")
                        })
                        .expect("outgoing open");
                    if let OutgoingEnvelope::ToConnection {
                        connection_id,
                        message,
                        ..
                    } = envelope
                    {
                        match message {
                            OutgoingMessage::Response(response)
                                if connection_id == connection
                                    && response.id == RequestId::Integer(request_id) =>
                            {
                                return;
                            }
                            OutgoingMessage::Error(error)
                                if connection_id == connection
                                    && error.id == RequestId::Integer(request_id) =>
                            {
                                panic!("request failed: {error:?}");
                            }
                            _ => {}
                        }
                    }
                }
            }

            let mut harness = TracingHarness::new().await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(50_001),
                        params: ThreadStartParams {
                            ephemeral: Some(false),
                            ..Default::default()
                        },
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = ThreadId::from_string(&started.thread.id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            thread.ensure_rollout_materialized().await;
            thread.flush_rollout().await?;
            let rollout_path = thread.rollout_path().expect("materialized rollout path");
            assert!(rollout_path.is_file());
            let config = thread.config().await;
            let store = codex_core::thread_store_from_config(config.as_ref(), None);
            assert!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: false,
                        include_history: false,
                    })
                    .await?
                    .archived_at
                    .is_none()
            );

            let remaining_session = Arc::new(ConnectionSessionState::new());
            let remaining_outbound_initialized =
                Arc::new(std::sync::atomic::AtomicBool::new(false));
            harness
                .processor
                .outgoing
                .connection_opened(
                    SECOND_TEST_CONNECTION_ID,
                    Arc::clone(&remaining_outbound_initialized),
                )
                .await;
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::Initialize {
                        request_id: RequestId::Integer(50_002),
                        params: InitializeParams {
                            client_info: ClientInfo {
                                name: "codex-app-server-tests".to_string(),
                                title: None,
                                version: "0.1.0".to_string(),
                            },
                            capabilities: Some(InitializeCapabilities {
                                experimental_api: true,
                                ..Default::default()
                            }),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&remaining_session),
                )
                .await;
            response_for_connection(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 50_002)
                .await;
            assert!(remaining_session.initialized());
            // JSON request dispatch leaves transport readiness to lib.rs. Complete
            // that same registration before sending the surviving client's resume.
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    SECOND_TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: remaining_session.request_attestation(),
                        experimental_api: remaining_session.experimental_api_enabled(),
                    },
                )
                .await;
            remaining_outbound_initialized.store(true, Ordering::Release);
            for connection in [TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID] {
                assert!(
                    harness
                        .processor
                        .thread_processor
                        .thread_state_manager
                        .is_connection_initialized(connection)
                        .await,
                    "delete setup requires live transport registration for {connection:?}"
                );
            }
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadResume {
                        request_id: RequestId::Integer(50_003),
                        params: serde_json::from_value(json!({"threadId": thread_id.to_string()}))?,
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&remaining_session),
                )
                .await;
            response_for_connection(&mut harness.outgoing_rx, SECOND_TEST_CONNECTION_ID, 50_003)
                .await;
            let manager = harness
                .processor
                .thread_processor
                .thread_state_manager
                .clone();
            assert_eq!(
                manager
                    .subscribed_connection_ids(thread_id)
                    .await
                    .into_iter()
                    .collect::<HashSet<_>>(),
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID]),
            );
            let original_state = manager.thread_state(thread_id).await;
            assert!(original_state.lock().await.listener_matches(&thread));
            let listener_sender = manager
                .current_listener_command_tx(thread_id)
                .expect("normal listener route");
            let outgoing = Arc::clone(&harness.processor.outgoing);
            let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
                Arc::clone(&outgoing),
                vec![TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID],
                thread_id,
            );
            let (approval_id, mut approval_result) = thread_outgoing
                .send_request(ServerRequestPayload::FileChangeRequestApproval(
                    FileChangeRequestApprovalParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "delete-turn".to_string(),
                        item_id: "delete-approval".to_string(),
                        started_at_ms: 0,
                        reason: None,
                        grant_root: None,
                    },
                ))
                .await;
            assert_eq!(
                outgoing.pending_requests_for_thread(thread_id).await.len(),
                1
            );
            let mut approval_recipients = HashSet::new();
            while approval_recipients.len() < 2 {
                let envelope =
                    tokio::time::timeout(Duration::from_secs(5), harness.outgoing_rx.recv())
                        .await
                        .expect("approval delivery deadline")
                        .expect("outgoing open");
                if let OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    ..
                } = envelope
                    && request.id() == &approval_id
                {
                    approval_recipients.insert(connection_id);
                }
            }
            assert_eq!(
                approval_recipients,
                HashSet::from([TEST_CONNECTION_ID, SECOND_TEST_CONNECTION_ID])
            );

            let release_shutdown = codex_core::test_support::block_thread_terminal_tasks(&thread);
            assert_eq!(
                thread.acquire_out_of_band_elicitation_lease(
                    codex_core::OutOfBandElicitationLeaseId::new(
                        TEST_CONNECTION_ID.0,
                        "delete-shutdown-observer".to_string()
                    ),
                )?,
                1
            );
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadDelete {
                        request_id: RequestId::Integer(50_004),
                        params: ThreadDeleteParams {
                            thread_id: thread_id.to_string(),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                while thread.active_out_of_band_elicitation_lease_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("delete owner must enter real post-commit shutdown");
            assert!(matches!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await,
                Err(codex_thread_store::ThreadStoreError::ThreadNotFound {
                    thread_id: missing_thread_id,
                }) if missing_thread_id == thread_id
            ));
            assert!(
                !rollout_path.exists(),
                "store commit must remove the active rollout"
            );
            assert!(
                harness
                    .processor
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .is_err()
            );
            assert!(matches!(
                approval_result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));

            tokio::time::timeout(
                Duration::from_secs(5),
                harness
                    .processor
                    .connection_closed(TEST_CONNECTION_ID, &harness.session),
            )
            .await
            .expect("origin RPC cancellation must not wait for delete completion");
            assert_eq!(
                manager.subscribed_connection_ids(thread_id).await,
                vec![SECOND_TEST_CONNECTION_ID]
            );
            assert_eq!(
                outgoing.pending_requests_for_thread(thread_id).await.len(),
                1,
                "remaining authorized connection keeps approval pending until delete cleanup"
            );
            assert!(matches!(
                approval_result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            release_shutdown
                .send(())
                .expect("real terminal task remains blocked");
            tokio::time::timeout(Duration::from_secs(5), thread.wait_until_terminated())
                .await
                .expect("deleted core thread must terminate");
            assert!(
                tokio::time::timeout(Duration::from_secs(5), approval_result)
                    .await
                    .expect("delete must cancel the pending approval callback")
                    .is_err()
            );
            let deleted = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let envelope = harness.outgoing_rx.recv().await.expect("outgoing open");
                    let message = match envelope {
                        OutgoingEnvelope::Broadcast { message } => message,
                        OutgoingEnvelope::ToConnection {
                            connection_id,
                            message,
                            ..
                        } if connection_id == SECOND_TEST_CONNECTION_ID => message,
                        OutgoingEnvelope::ToConnection { .. } => continue,
                    };
                    if let OutgoingMessage::AppServerNotification(
                        ServerNotification::ThreadDeleted(deleted),
                    ) = message
                    {
                        break deleted;
                    }
                }
            })
            .await
            .expect("surviving connection must be notified of the committed delete");
            assert_eq!(deleted.thread_id, thread_id.to_string());
            assert!(matches!(
                store
                    .read_thread(ReadThreadParams {
                        thread_id,
                        include_archived: true,
                        include_history: false,
                    })
                    .await,
                Err(codex_thread_store::ThreadStoreError::ThreadNotFound {
                    thread_id: missing_thread_id,
                }) if missing_thread_id == thread_id
            ));
            assert!(
                harness
                    .processor
                    .thread_manager
                    .get_thread(thread_id)
                    .await
                    .is_err()
            );
            assert!(
                outgoing
                    .pending_requests_for_thread(thread_id)
                    .await
                    .is_empty()
            );
            assert!(
                manager
                    .subscribed_connection_ids(thread_id)
                    .await
                    .is_empty()
            );
            assert!(manager.current_listener_command_tx(thread_id).is_none());
            assert!(!original_state.lock().await.listener_matches(&thread));
            assert!(!Arc::ptr_eq(
                &original_state,
                &manager.thread_state(thread_id).await
            ));
            tokio::time::timeout(Duration::from_secs(5), listener_sender.closed())
                .await
                .expect("stale listener must stop");
            assert_eq!(
                harness
                    .processor
                    .thread_processor
                    .thread_watch_manager
                    .loaded_status_for_thread(&thread_id.to_string())
                    .await,
                codex_app_server_protocol::ThreadStatus::NotLoaded,
            );
            assert!(
                manager
                    .is_connection_initialized(SECOND_TEST_CONNECTION_ID)
                    .await
            );
            harness.shutdown().await;
            Ok(())
        },
    )
}

mod resume_listener_generation_rpc_tests {
    use super::*;
    use crate::outgoing_message::ConnectionRequestId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ThreadGoalClearedNotification;
    use codex_app_server_protocol::ThreadResumeResponse;
    use codex_app_server_protocol::TurnStatus;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    async fn submit(
        harness: &TracingHarness,
        connection_id: ConnectionId,
        session: &Arc<ConnectionSessionState>,
        request: ClientRequest,
    ) {
        harness
            .processor
            .process_request(
                connection_id,
                request_from_client_request(request),
                &AppServerTransport::Stdio,
                Arc::clone(session),
            )
            .await;
    }

    async fn await_response_preparation(outgoing: &OutgoingMessageSender, id: ConnectionRequestId) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !outgoing
                    .lock_request_contexts_for_test()
                    .await
                    .contains_key(&id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("resume must reach response preparation despite outgoing backpressure");
    }

    async fn superseded_resume_is_rejected(exclude_turns: bool) -> Result<()> {
        let mut harness = TracingHarness::new().await?;
        let started: ThreadStartResponse = harness
            .request(
                ClientRequest::ThreadStart {
                    request_id: RequestId::Integer(80_000),
                    params: ThreadStartParams::default(),
                },
                None,
            )
            .await;
        read_thread_started_notification(&mut harness.outgoing_rx).await;
        let thread_id = ThreadId::from_string(&started.thread.id)?;
        let thread = harness
            .processor
            .thread_manager
            .get_thread(thread_id)
            .await?;
        submit(&harness, TEST_CONNECTION_ID, &harness.session, ClientRequest::TurnStart {
            request_id: RequestId::Integer(80_001),
            params: serde_json::from_value(json!({
                "threadId": started.thread.id,
                "input": [{"type": "text", "text": "resume-generation-retained-input", "textElements": []}],
            }))?,
        }).await;
        let mut turn_started = None;
        let mut turn_completed = None;
        tokio::time::timeout(Duration::from_secs(15), async {
            while turn_started.is_none() || turn_completed.is_none() {
                let envelope = harness.outgoing_rx.recv().await.expect("outgoing open");
                let message = match envelope {
                    OutgoingEnvelope::ToConnection {
                        message,
                        write_complete_tx,
                        ..
                    } => {
                        if let Some(receipt) = write_complete_tx {
                            let _ = receipt.send(());
                        }
                        message
                    }
                    OutgoingEnvelope::Broadcast { message } => message,
                };
                match message {
                    OutgoingMessage::Response(response)
                        if response.id == RequestId::Integer(80_001) =>
                    {
                        let response: TurnStartResponse =
                            serde_json::from_value(response.result).expect("turn response");
                        turn_started = Some(response.turn.id);
                    }
                    OutgoingMessage::AppServerNotification(ServerNotification::TurnCompleted(
                        completed,
                    )) => {
                        assert_eq!(completed.thread_id, started.thread.id);
                        assert_eq!(completed.turn.status, TurnStatus::Completed);
                        turn_completed = Some(completed.turn.id);
                    }
                    OutgoingMessage::Error(error) => panic!("turn setup failed: {error:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("real turn must complete");
        assert_eq!(turn_started, turn_completed);
        let turn_id = turn_started.expect("turn id");
        let seeded: ThreadResumeResponse = harness
            .request(
                ClientRequest::ThreadResume {
                    request_id: RequestId::Integer(80_002),
                    params: serde_json::from_value(json!({"threadId": started.thread.id}))?,
                },
                None,
            )
            .await;
        assert_eq!(seeded.thread.turns.len(), 1);
        assert_eq!(seeded.thread.turns[0].id, turn_id);
        assert_eq!(seeded.thread.turns[0].status, TurnStatus::Completed);
        thread.flush_rollout().await?;
        let rollout = thread.rollout_path().expect("persisted thread rollout");
        let original_rollout = std::fs::read(&rollout)?;
        let manager = harness
            .processor
            .thread_processor
            .thread_state_manager
            .clone();
        let state = manager.thread_state(thread_id).await;
        let generation = {
            let state = state.lock().await;
            assert!(state.resume_history_is_seeded_for_current_listener());
            state.listener_generation
        };

        let second_session = Arc::new(ConnectionSessionState::new());
        let outbound_initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
        harness
            .processor
            .outgoing
            .connection_opened(SECOND_TEST_CONNECTION_ID, Arc::clone(&outbound_initialized))
            .await;
        submit(
            &harness,
            SECOND_TEST_CONNECTION_ID,
            &second_session,
            ClientRequest::Initialize {
                request_id: RequestId::Integer(80_003),
                params: InitializeParams {
                    client_info: ClientInfo {
                        name: "codex-app-server-tests".to_string(),
                        title: None,
                        version: "0.1.0".to_string(),
                    },
                    capabilities: Some(InitializeCapabilities {
                        experimental_api: true,
                        ..Default::default()
                    }),
                },
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Response(response),
                    ..
                }) = harness.outgoing_rx.recv().await
                    && connection_id == SECOND_TEST_CONNECTION_ID
                    && response.id == RequestId::Integer(80_003)
                {
                    break;
                }
            }
        })
        .await
        .expect("second connection initializes");
        manager
            .connection_initialized(
                SECOND_TEST_CONNECTION_ID,
                ConnectionCapabilities {
                    request_attestation: second_session.request_attestation(),
                    experimental_api: second_session.experimental_api_enabled(),
                },
            )
            .await;
        outbound_initialized.store(true, Ordering::Release);

        let outgoing = Arc::clone(&harness.processor.outgoing);
        while harness.outgoing_rx.try_recv().is_ok() {}
        let mut filled = 0;
        while outgoing.try_send_server_notification(ServerNotification::ThreadGoalCleared(
            ThreadGoalClearedNotification {
                thread_id: started.thread.id.clone(),
            },
        )) {
            filled += 1;
        }
        assert!(filled > 0, "fixture must exercise actual outgoing capacity");
        let stale_id = ConnectionRequestId {
            connection_id: SECOND_TEST_CONNECTION_ID,
            request_id: RequestId::Integer(80_004),
        };
        submit(
            &harness,
            SECOND_TEST_CONNECTION_ID,
            &second_session,
            ClientRequest::ThreadResume {
                request_id: stale_id.request_id.clone(),
                params: serde_json::from_value(json!({
                    "threadId": started.thread.id, "excludeTurns": exclude_turns,
                }))?,
            },
        )
        .await;
        // Removal of the registered request context locates the real listener
        // inside response preparation; the full transport queue keeps it there.
        await_response_preparation(&outgoing, stale_id.clone()).await;
        assert_eq!(
            manager.subscribed_connection_ids(thread_id).await,
            vec![TEST_CONNECTION_ID],
            "waiting response preparation must not admit a stale subscriber"
        );
        tokio::time::timeout(Duration::from_secs(1), manager.clear_all_listeners())
            .await
            .expect("outgoing backpressure must not pin listener teardown");
        let replacement_id = ConnectionRequestId {
            connection_id: TEST_CONNECTION_ID,
            request_id: RequestId::Integer(80_005),
        };
        submit(
            &harness,
            TEST_CONNECTION_ID,
            &harness.session,
            ClientRequest::ThreadResume {
                request_id: replacement_id.request_id.clone(),
                params: serde_json::from_value(json!({"threadId": started.thread.id}))?,
            },
        )
        .await;
        await_response_preparation(&outgoing, replacement_id.clone()).await;
        {
            let state = state.lock().await;
            assert_ne!(
                state.listener_generation, generation,
                "normal resume registers a replacement listener"
            );
            assert!(
                state.resume_history_is_seeded_for_current_listener(),
                "replacement history is seeded before stale commit"
            );
            assert!(state.listener_matches(&thread));
        }

        let mut stale_error = None;
        let mut replacement_response = None;
        tokio::time::timeout(Duration::from_secs(5), async {
            while stale_error.is_none() || replacement_response.is_none() {
                let Some(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message,
                    ..
                }) = harness.outgoing_rx.recv().await
                else {
                    continue;
                };
                match message {
                    OutgoingMessage::Error(error) if error.id == stale_id.request_id => {
                        assert_eq!(connection_id, SECOND_TEST_CONNECTION_ID);
                        assert!(
                            stale_error.replace(error.error).is_none(),
                            "one stale resume error"
                        );
                    }
                    OutgoingMessage::Response(response)
                        if response.id == replacement_id.request_id =>
                    {
                        assert_eq!(connection_id, TEST_CONNECTION_ID);
                        let response: ThreadResumeResponse =
                            serde_json::from_value(response.result).expect("replacement resume");
                        assert!(replacement_response.replace(response).is_none());
                    }
                    OutgoingMessage::Response(response) if response.id == stale_id.request_id => {
                        panic!("superseded listener emitted stale success: {response:?}")
                    }
                    OutgoingMessage::Error(error) => panic!("replacement resume failed: {error:?}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("old rejection and healthy replacement response");
        let error = stale_error.expect("stale resume error");
        assert_eq!(error.code, crate::error_code::INTERNAL_ERROR_CODE);
        assert_eq!(
            error.message,
            format!("thread {thread_id} listener changed while composing resume response")
        );
        let response = replacement_response.expect("replacement response");
        assert_eq!(response.thread.id, started.thread.id);
        assert_eq!(
            response.thread.session_id,
            thread.session_configured().session_id.to_string()
        );
        assert_eq!(
            response.thread.turns.len(),
            1,
            "resume preserves the single real turn"
        );
        assert_eq!(response.thread.turns[0].id, turn_id);
        assert_eq!(response.thread.turns[0].status, TurnStatus::Completed);
        let turns = serde_json::to_string(&response.thread.turns)?;
        assert!(turns.contains("resume-generation-retained-input"));
        assert!(turns.contains("Done"));
        assert_eq!(
            manager.subscribed_connection_ids(thread_id).await,
            vec![TEST_CONNECTION_ID],
            "rejected stale resume must not subscribe its connection"
        );
        thread.flush_rollout().await?;
        assert_eq!(
            std::fs::read(rollout)?,
            original_rollout,
            "stale resume must not rewrite durable history"
        );
        let contexts = outgoing.lock_request_contexts_for_test().await;
        assert!(!contexts.contains_key(&stale_id));
        assert!(!contexts.contains_key(&replacement_id));
        drop(contexts);
        harness.shutdown().await;
        Ok(())
    }

    #[test]
    #[serial(app_server_tracing)]
    fn metadata_resume_rejects_listener_replaced_during_response_preparation() -> Result<()> {
        run_current_thread_test_with_stack(
            "metadata_resume_rejects_listener_replaced_during_response_preparation",
            superseded_resume_is_rejected(true),
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn history_resume_rejects_listener_replaced_during_response_preparation() -> Result<()> {
        run_current_thread_test_with_stack(
            "history_resume_rejects_listener_replaced_during_response_preparation",
            superseded_resume_is_rejected(false),
        )
    }
}

#[cfg(windows)]
mod command_output_fallback_rpc_tests {
    use super::*;
    use crate::outgoing_message::ConnectionRequestId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use codex_app_server_protocol::CommandExecOutputStream;
    use codex_app_server_protocol::CommandExecResponse;
    use codex_app_server_protocol::ServerNotification;
    use codex_app_server_protocol::ThreadGoalClearedNotification;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    #[test]
    #[serial(app_server_tracing)]
    fn command_exec_rpc_failed_stream_delivery_preserves_capped_output_once() -> Result<()> {
        run_current_thread_test_with_stack(
            "command_exec_rpc_failed_stream_delivery_preserves_capped_output_once",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let gate = dir.path().join("write-suffix");
                let finished = dir.path().join("child-finished");
                let quote_path = |path: &Path| path.display().to_string().replace('\'', "''");
                let script = format!(
                    "$out = [Console]::OpenStandardOutput(); $err = [Console]::OpenStandardError(); \
                     $out.Write([byte[]](111,117,116,124), 0, 4); $out.Flush(); \
                     $err.Write([byte[]](101,114,114,124), 0, 4); $err.Flush(); \
                     while (-not [IO.File]::Exists('{}')) {{ Start-Sleep -Milliseconds 10 }}; \
                     $bytes = [Text.Encoding]::UTF8.GetBytes(('€abcde' * 20000)); \
                     $out.Write($bytes, 0, $bytes.Length); $out.Flush(); \
                     $bytes = [Text.Encoding]::UTF8.GetBytes(('λABCDEF' * 20000)); \
                     $err.Write($bytes, 0, $bytes.Length); $err.Flush(); \
                     [IO.File]::WriteAllText('{}', 'finished')",
                    quote_path(&gate),
                    quote_path(&finished),
                );
                let request_id = ConnectionRequestId {
                    connection_id: TEST_CONNECTION_ID,
                    request_id: RequestId::Integer(81_000),
                };
                harness.processor.process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::OneOffCommandExec {
                        request_id: request_id.request_id.clone(),
                        params: serde_json::from_value(json!({
                            "command": ["powershell.exe", "-NoLogo", "-NoProfile", "-Command", script],
                            "processId": "output-fallback",
                            "streamStdoutStderr": true,
                            "disableTimeout": true,
                            "outputBytesCap": 4 + 8 * 16384,
                            "sandboxPolicy": {"type": "dangerFullAccess"},
                            "cwd": dir.path(),
                        }))?,
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                ).await;
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                tokio::time::timeout(Duration::from_secs(15), async {
                    while stdout.is_empty() || stderr.is_empty() {
                        let Some(OutgoingEnvelope::ToConnection { message, .. }) =
                            harness.outgoing_rx.recv().await
                        else {
                            continue;
                        };
                        match message {
                            OutgoingMessage::AppServerNotification(
                                ServerNotification::CommandExecOutputDelta(delta),
                            ) => {
                                assert_eq!(delta.process_id, "output-fallback");
                                assert!(!delta.cap_reached);
                                let bytes = STANDARD
                                    .decode(delta.delta_base64)
                                    .expect("raw output delta");
                                match delta.stream {
                                    CommandExecOutputStream::Stdout => stdout.extend(bytes),
                                    CommandExecOutputStream::Stderr => stderr.extend(bytes),
                                }
                            }
                            OutgoingMessage::Error(error) => {
                                panic!("command startup failed: {error:?}")
                            }
                            OutgoingMessage::Response(response) => {
                                panic!("child exited before gate: {response:?}")
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .expect("real child emits both delivered prefixes");
                assert_eq!(stdout.as_slice(), b"out|");
                assert_eq!(stderr.as_slice(), b"err|");
                assert!(!finished.exists());
                let outgoing = Arc::clone(&harness.processor.outgoing);
                let mut filled = 0;
                while outgoing.try_send_server_notification(ServerNotification::ThreadGoalCleared(
                    ThreadGoalClearedNotification {
                        thread_id: "output-capacity-fixture".to_string(),
                    },
                )) {
                    filled += 1;
                }
                assert!(filled > 0);
                std::fs::write(&gate, "release suffix")?;
                // The final response cannot take its request context until the
                // actual relay send has timed out and both collectors finished.
                // Keep the transport full until that observable transition.
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        if !outgoing
                            .lock_request_contexts_for_test()
                            .await
                            .contains_key(&request_id)
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("transport failure must not prevent capture and child cleanup");
                assert_eq!(std::fs::read_to_string(&finished)?, "finished");
                let response = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let Some(OutgoingEnvelope::ToConnection { message, .. }) =
                            harness.outgoing_rx.recv().await
                        else {
                            continue;
                        };
                        match message {
                            OutgoingMessage::Response(response)
                                if response.id == request_id.request_id =>
                            {
                                break serde_json::from_value::<CommandExecResponse>(
                                    response.result,
                                )
                                .expect("final command response");
                            }
                            OutgoingMessage::AppServerNotification(
                                ServerNotification::CommandExecOutputDelta(delta),
                            ) => {
                                assert_eq!(delta.process_id, "output-fallback");
                                let bytes = STANDARD
                                    .decode(delta.delta_base64)
                                    .expect("raw output delta");
                                match delta.stream {
                                    CommandExecOutputStream::Stdout => stdout.extend(bytes),
                                    CommandExecOutputStream::Stderr => stderr.extend(bytes),
                                }
                            }
                            OutgoingMessage::Error(error) => {
                                panic!("command output failure: {error:?}")
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .expect("draining transport delivers the fallback response");
                assert_eq!(response.exit_code, 0);
                assert!(
                    !response.stdout.is_empty(),
                    "undelivered stdout must be retained"
                );
                assert!(
                    !response.stderr.is_empty(),
                    "undelivered stderr must be retained"
                );
                stdout.extend(response.stdout.as_bytes());
                stderr.extend(response.stderr.as_bytes());
                let expected_stdout = format!("out|{}", "€abcde".repeat(16384)).into_bytes();
                let expected_stderr = format!("err|{}", "λABCDEF".repeat(16384)).into_bytes();
                assert!(
                    stdout == expected_stdout,
                    "delivered stdout plus fallback must equal the capped bytes once (actual {}, expected {})",
                    stdout.len(),
                    expected_stdout.len()
                );
                assert!(
                    stderr == expected_stderr,
                    "delivered stderr plus fallback must equal the capped bytes once (actual {}, expected {})",
                    stderr.len(),
                    expected_stderr.len()
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }
}

#[cfg(windows)]
mod process_exec_control_rpc_tests {
    use super::*;
    use crate::outgoing_message::ConnectionRequestId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use codex_app_server_protocol::JSONRPCErrorError;
    use codex_app_server_protocol::ProcessExitedNotification;
    use codex_app_server_protocol::ProcessKillParams;
    use codex_app_server_protocol::ProcessWriteStdinParams;
    use codex_app_server_protocol::ServerNotification;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    const BUSY: &str = "process stdin write queue is full; retry after a pending write completes";

    #[derive(Default)]
    struct Observed {
        replies: BTreeMap<i64, std::result::Result<serde_json::Value, JSONRPCErrorError>>,
        exited: Option<ProcessExitedNotification>,
    }

    impl Observed {
        fn record(&mut self, envelope: OutgoingEnvelope) {
            let OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                ..
            } = envelope
            else {
                return;
            };
            assert_eq!(connection_id, TEST_CONNECTION_ID);
            let (id, result) = match message {
                OutgoingMessage::Response(response) => (response.id, Ok(response.result)),
                OutgoingMessage::Error(error) => (error.id, Err(error.error)),
                OutgoingMessage::AppServerNotification(ServerNotification::ProcessExited(
                    exited,
                )) => {
                    assert!(
                        self.exited.replace(exited).is_none(),
                        "duplicate process exit"
                    );
                    return;
                }
                _ => return,
            };
            let RequestId::Integer(id) = id else {
                panic!("integer request id required");
            };
            assert!(
                self.replies.insert(id, result).is_none(),
                "duplicate RPC reply for {id}"
            );
        }
    }

    async fn submit(harness: &TracingHarness, request: ClientRequest) {
        harness
            .processor
            .process_request(
                TEST_CONNECTION_ID,
                request_from_client_request(request),
                &AppServerTransport::Stdio,
                Arc::clone(&harness.session),
            )
            .await;
    }

    async fn observe_until(
        harness: &mut TracingHarness,
        observed: &mut Observed,
        label: &str,
        done: impl Fn(&Observed) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !done(observed) {
                observed.record(harness.outgoing_rx.recv().await.expect("outgoing open"));
            }
            while let Ok(envelope) = harness.outgoing_rx.try_recv() {
                observed.record(envelope);
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label}: reply ids {:?}, exit {:?}",
                observed.replies.keys().collect::<Vec<_>>(),
                observed.exited,
            )
        });
    }

    async fn start_gated_reader(harness: &mut TracingHarness, dir: &Path, handle: &str) {
        let quote_path = |name| dir.join(name).display().to_string().replace('\'', "''");
        let gate = quote_path("read-stdin");
        let output = quote_path("received.bin");
        let pid_file = quote_path("child-pid");
        let script = format!(
            "[IO.File]::WriteAllText('{pid_file}', [string]$PID); \
             [Console]::Out.WriteLine('stdin-ready'); [Console]::Out.Flush(); \
             while (-not [IO.File]::Exists('{gate}')) {{ Start-Sleep -Milliseconds 10 }}; \
             $source = [Console]::OpenStandardInput(); $destination = [IO.File]::Create('{output}'); \
             try {{ $source.CopyTo($destination) }} finally {{ $destination.Dispose() }}"
        );
        submit(
            harness,
            ClientRequest::ProcessSpawn {
                request_id: RequestId::Integer(70_000),
                params: serde_json::from_value(json!({
                    "command": ["powershell.exe", "-NoLogo", "-NoProfile", "-Command", script],
                    "processHandle": handle,
                    "streamStdin": true,
                    "streamStdoutStderr": true,
                    "timeoutMs": null,
                    "cwd": dir,
                }))
                .expect("valid process parameters"),
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut spawned = false;
            let mut output = Vec::new();
            loop {
                let OutgoingEnvelope::ToConnection { message, .. } =
                    harness.outgoing_rx.recv().await.expect("outgoing open")
                else {
                    continue;
                };
                match message {
                    OutgoingMessage::Response(response) => {
                        assert_eq!(response.id, RequestId::Integer(70_000));
                        assert_eq!(response.result, json!({}));
                        assert!(!spawned, "duplicate spawn response");
                        spawned = true;
                    }
                    OutgoingMessage::AppServerNotification(
                        ServerNotification::ProcessOutputDelta(delta),
                    ) => {
                        assert!(spawned, "spawn response must precede process output");
                        assert_eq!(delta.process_handle, handle);
                        output.extend(STANDARD.decode(delta.delta_base64).expect("output base64"));
                        if String::from_utf8_lossy(&output).contains("stdin-ready") {
                            break;
                        }
                    }
                    OutgoingMessage::Error(error) => panic!("process startup failed: {error:?}"),
                    OutgoingMessage::AppServerNotification(ServerNotification::ProcessExited(
                        exited,
                    )) => {
                        panic!("child exited before readiness: {exited:?}");
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("real child must report readiness before writes");
    }

    async fn write(harness: &TracingHarness, handle: &str, id: i64, bytes: &[u8], close: bool) {
        submit(
            harness,
            ClientRequest::ProcessWriteStdin {
                request_id: RequestId::Integer(id),
                params: ProcessWriteStdinParams {
                    process_handle: handle.to_string(),
                    delta_base64: Some(STANDARD.encode(bytes)),
                    close_stdin: close,
                },
            },
        )
        .await;
    }

    // A malformed write is a FIFO admission barrier through the registered
    // process lane. Batches stay below the ordinary RPC queue count/byte limits.
    // Earlier ACKs prove driver acceptance even while the child does not read.
    async fn saturate(
        harness: &mut TracingHarness,
        handle: &str,
        observed: &mut Observed,
    ) -> BTreeMap<i64, Vec<u8>> {
        let mut attempted = BTreeMap::new();
        for batch in 0..32_i64 {
            for offset in 0..8_i64 {
                let index = batch * 8 + offset;
                let id = 71_000 + index;
                let mut bytes = vec![b'a' + (index % 26) as u8; 64 * 1024];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                write(harness, handle, id, &bytes, false).await;
                attempted.insert(id, bytes);
            }
            let barrier = 72_000 + batch;
            submit(
                harness,
                ClientRequest::ProcessWriteStdin {
                    request_id: RequestId::Integer(barrier),
                    params: ProcessWriteStdinParams {
                        process_handle: handle.to_string(),
                        delta_base64: Some("%%%".to_string()),
                        close_stdin: false,
                    },
                },
            )
            .await;
            observe_until(harness, observed, "write admission barrier", |o| {
                o.replies.contains_key(&barrier)
            })
            .await;
            assert!(
                observed.replies[&barrier]
                    .as_ref()
                    .expect_err("malformed base64 rejected")
                    .message
                    .starts_with("invalid deltaBase64:")
            );
        }
        let mut acknowledged = 0;
        let mut rejected = 0;
        for id in attempted.keys() {
            if let Some(result) = observed.replies.get(id) {
                match result {
                    Ok(value) => {
                        assert_eq!(value, &json!({}));
                        acknowledged += 1;
                    }
                    Err(error) => {
                        assert_eq!(error.message, BUSY);
                        rejected += 1;
                    }
                }
            }
        }
        assert!(
            acknowledged > 0,
            "driver must accept earlier writes without child reads"
        );
        assert!(rejected > 0, "normal RPC must reject overflow");
        assert_eq!(
            attempted.len() - acknowledged - rejected,
            32,
            "exactly 32 accepted write response owners remain unresolved"
        );
        assert!(observed.exited.is_none(), "child must still be blocked");
        attempted
    }

    #[test]
    #[serial(app_server_tracing)]
    fn process_rpc_backpressured_writes_do_not_block_kill() -> Result<()> {
        run_current_thread_test_with_stack(
            "process_rpc_backpressured_writes_do_not_block_kill",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let handle = "process-rpc-backpressured-kill";
                start_gated_reader(&mut harness, dir.path(), handle).await;
                let mut observed = Observed::default();
                let attempted = saturate(&mut harness, handle, &mut observed).await;
                let pending = attempted
                    .keys()
                    .filter(|id| !observed.replies.contains_key(id))
                    .copied()
                    .collect::<Vec<_>>();
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "child has not read stdin"
                );
                write(&harness, handle, 73_000, b"REJECTED-CLOSE", true).await;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "overflow close rejection",
                    |o| o.replies.contains_key(&73_000),
                )
                .await;
                assert_eq!(
                    observed.replies[&73_000]
                        .as_ref()
                        .expect_err("full owner capacity")
                        .message,
                    BUSY
                );
                submit(
                    &harness,
                    ClientRequest::ProcessKill {
                        request_id: RequestId::Integer(73_001),
                        params: ProcessKillParams {
                            process_handle: handle.to_string(),
                        },
                    },
                )
                .await;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "kill, actual exit, and all write owners",
                    |o| {
                        o.replies.contains_key(&73_001)
                            && o.exited.is_some()
                            && attempted.keys().all(|id| o.replies.contains_key(id))
                    },
                )
                .await;
                assert_eq!(
                    observed.replies[&73_001].as_ref().expect("kill succeeds"),
                    &json!({})
                );
                let exited = observed.exited.as_ref().expect("real process exit");
                assert_eq!(exited.process_handle, handle);
                assert_ne!(exited.exit_code, 0, "kill must terminate the blocked child");
                for id in pending {
                    // ACK means driver queue acceptance. Killing the child may release
                    // that queue before the stdin writer observes exit and is aborted.
                    match observed.replies[&id].as_ref() {
                        Ok(value) => assert_eq!(value, &json!({})),
                        Err(error) => assert!(
                            error.message.contains("no longer running")
                                || error.message == "stdin is already closed",
                            "unexpected cleanup error: {error:?}"
                        ),
                    }
                }
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "kill must not release the read gate"
                );
                write(&harness, handle, 73_002, b"after-exit", false).await;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "removed process rejects writes",
                    |o| o.replies.contains_key(&73_002),
                )
                .await;
                assert!(
                    observed.replies[&73_002]
                        .as_ref()
                        .expect_err("process removed")
                        .message
                        .contains("no active process")
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn process_rpc_queued_writes_preserve_fifo_and_busy_rejection_has_no_side_effect() -> Result<()>
    {
        run_current_thread_test_with_stack(
            "process_rpc_queued_writes_preserve_fifo_and_busy_rejection_has_no_side_effect",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let handle = "process-rpc-backpressured-fifo";
                start_gated_reader(&mut harness, dir.path(), handle).await;
                let mut observed = Observed::default();
                let attempted = saturate(&mut harness, handle, &mut observed).await;
                write(
                    &harness,
                    handle,
                    73_000,
                    b"REJECTED-PAYLOAD-MUST-NOT-REACH-STDIN",
                    true,
                )
                .await;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "saturated close rejection",
                    |o| o.replies.contains_key(&73_000),
                )
                .await;
                assert_eq!(
                    observed.replies[&73_000]
                        .as_ref()
                        .expect_err("reject before mutation")
                        .message,
                    BUSY
                );
                assert!(!dir.path().join("received.bin").exists());
                std::fs::write(dir.path().join("read-stdin"), b"release real child")?;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "accepted write acknowledgements",
                    |o| attempted.keys().all(|id| o.replies.contains_key(id)),
                )
                .await;
                let tail = b"ACCEPTED-TAIL-AFTER-REJECTED-CLOSE";
                write(&harness, handle, 73_001, tail, true).await;
                observe_until(
                    &mut harness,
                    &mut observed,
                    "tail acknowledgement and real EOF",
                    |o| o.replies.contains_key(&73_001) && o.exited.is_some(),
                )
                .await;
                assert_eq!(
                    observed.replies[&73_001]
                        .as_ref()
                        .expect("rejected close leaves stdin open"),
                    &json!({})
                );
                let exited = observed.exited.as_ref().expect("real process exit");
                assert_eq!(exited.process_handle, handle);
                assert_eq!(exited.exit_code, 0);
                let mut expected = Vec::new();
                for (id, bytes) in attempted {
                    match observed.replies[&id].as_ref() {
                        Ok(value) => {
                            assert_eq!(value, &json!({}));
                            expected.extend_from_slice(&bytes);
                        }
                        Err(error) => assert_eq!(error.message, BUSY),
                    }
                }
                expected.extend_from_slice(tail);
                let actual = std::fs::read(dir.path().join("received.bin"))?;
                assert!(
                    actual == expected,
                    "child bytes must equal accepted inputs in FIFO order plus tail (actual {}, expected {})",
                    actual.len(),
                    expected.len()
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn process_rpc_disconnect_cancels_and_joins_pending_write_owners() -> Result<()> {
        run_current_thread_test_with_stack(
            "process_rpc_disconnect_cancels_and_joins_pending_write_owners",
            async {
                use tokio::io::AsyncBufReadExt;
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let handle = "process-rpc-backpressured-disconnect";
                start_gated_reader(&mut harness, dir.path(), handle).await;
                let mut observed = Observed::default();
                let attempted = saturate(&mut harness, handle, &mut observed).await;
                let pending = attempted
                    .keys()
                    .filter(|id| !observed.replies.contains_key(id))
                    .copied()
                    .collect::<Vec<_>>();
                // Disconnect suppresses process/exited delivery. Observe the actual
                // child through an OS process handle acquired before closing the RPC.
                let child_pid: u32 =
                    std::fs::read_to_string(dir.path().join("child-pid"))?.parse()?;
                let mut watcher = tokio::process::Command::new("powershell.exe")
                .args(["-NoLogo", "-NoProfile", "-Command", &format!(
                    "$ErrorActionPreference = 'Stop'; $child = Get-Process -Id {child_pid}; \
                     $null = $child.Handle; \
                     [Console]::Out.WriteLine('watching'); [Console]::Out.Flush(); $child.WaitForExit()"
                )])
                .stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn()?;
                let mut watcher_output =
                    tokio::io::BufReader::new(watcher.stdout.take().expect("watcher stdout"));
                let mut ready = String::new();
                tokio::time::timeout(
                    Duration::from_secs(15),
                    watcher_output.read_line(&mut ready),
                )
                .await??;
                assert_eq!(
                    ready.trim(),
                    "watching",
                    "watcher must hold the live process before disconnect"
                );
                assert!(
                    harness.session.rpc_gate.inflight_count() > 0,
                    "pending writes have connection owners"
                );
                tokio::time::timeout(
                    Duration::from_secs(5),
                    harness
                        .processor
                        .connection_closed(TEST_CONNECTION_ID, &harness.session),
                )
                .await
                .expect("disconnect must not wait for stdin");
                assert_eq!(
                    harness.session.rpc_gate.inflight_count(),
                    0,
                    "disconnect joins every write owner"
                );
                let status =
                    tokio::time::timeout(Duration::from_secs(15), watcher.wait()).await??;
                assert!(
                    status.success(),
                    "OS watcher must observe actual child exit"
                );
                while let Ok(envelope) = harness.outgoing_rx.try_recv() {
                    observed.record(envelope);
                }
                for id in pending {
                    assert!(
                        !observed.replies.contains_key(&id),
                        "cancelled write {id} emitted a late reply"
                    );
                }
                assert!(
                    observed.exited.is_none(),
                    "disconnected client receives no exit notification"
                );
                let contexts = harness
                    .processor
                    .outgoing
                    .lock_request_contexts_for_test()
                    .await;
                assert!(
                    contexts
                        .keys()
                        .all(|id| id.connection_id != TEST_CONNECTION_ID),
                    "disconnect must clear request contexts"
                );
                drop(contexts);
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "disconnect never releases stdin"
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }

    async fn disconnect_terminates_before_metadata_cleanup(publish_spawn: bool) -> Result<()> {
        use tokio::io::AsyncBufReadExt;
        let mut harness = TracingHarness::new().await?;
        let dir = TempDir::new()?;
        let handle = "process-rpc-disconnect-before-metadata-cleanup";
        let outgoing = Arc::clone(&harness.processor.outgoing);
        if publish_spawn {
            start_gated_reader(&mut harness, dir.path(), handle).await;
        } else {
            let pid_file = dir
                .path()
                .join("child-pid")
                .display()
                .to_string()
                .replace('\'', "''");
            submit(
                &harness,
                ClientRequest::ProcessSpawn {
                    request_id: RequestId::Integer(70_000),
                    params: serde_json::from_value(json!({
                        "command": ["powershell.exe", "-NoLogo", "-NoProfile", "-Command",
                            format!("[IO.File]::WriteAllText('{pid_file}', [string]$PID); while ($true) {{ Start-Sleep -Milliseconds 10 }}")],
                        "processHandle": handle,
                        "streamStdin": true,
                        "streamStdoutStderr": true,
                        "timeoutMs": null,
                        "cwd": dir.path(),
                    }))?,
                },
            )
            .await;
        }
        // On this current-thread runtime, registration returns before the spawn
        // handler can publish its response. Hold the real response metadata lock
        // through disconnect, so later connection cleanup cannot deliver Kill.
        let contexts = outgoing.lock_request_contexts_for_test().await;
        if !publish_spawn {
            assert!(contexts.contains_key(&ConnectionRequestId {
                connection_id: TEST_CONNECTION_ID,
                request_id: RequestId::Integer(70_000),
            }));
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            while !dir.path().join("child-pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("native child starts before response metadata is released");
        let child_pid: u32 = std::fs::read_to_string(dir.path().join("child-pid"))?.parse()?;
        let mut watcher = tokio::process::Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                &format!(
                    "$ErrorActionPreference = 'Stop'; $child = Get-Process -Id {child_pid}; \
                 $null = $child.Handle; \
                 [Console]::Out.WriteLine('watching'); [Console]::Out.Flush(); $child.WaitForExit()"
                ),
            ])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut watcher_output =
            tokio::io::BufReader::new(watcher.stdout.take().expect("watcher stdout"));
        let mut ready = String::new();
        tokio::time::timeout(
            Duration::from_secs(15),
            watcher_output.read_line(&mut ready),
        )
        .await??;
        assert_eq!(
            ready.trim(),
            "watching",
            "watcher owns a live native process handle"
        );
        if !publish_spawn {
            assert!(
                harness.outgoing_rx.try_recv().is_err(),
                "spawn is still unpublished"
            );
            assert_eq!(harness.session.rpc_gate.inflight_count(), 1);
        }
        let processor = Arc::clone(&harness.processor);
        let session = Arc::clone(&harness.session);
        let close = tokio::spawn(async move {
            processor
                .connection_closed(TEST_CONNECTION_ID, &session)
                .await;
        });
        let cancellation = harness.session.rpc_gate.cancellation_token();
        tokio::time::timeout(Duration::from_secs(5), async {
            cancellation.cancelled().await;
            while harness.session.rpc_gate.inflight_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect cancels and joins the spawn RPC without its metadata lock");
        assert!(
            !close.is_finished(),
            "connection cleanup is still waiting for metadata"
        );
        let status = tokio::time::timeout(Duration::from_secs(5), watcher.wait()).await??;
        assert!(
            status.success(),
            "native child exits before connection cleanup can send Kill"
        );
        assert!(
            !close.is_finished(),
            "metadata remains locked through native exit"
        );
        drop(contexts);
        tokio::time::timeout(Duration::from_secs(5), close).await??;
        let mut observed = Observed::default();
        while let Ok(envelope) = harness.outgoing_rx.try_recv() {
            observed.record(envelope);
        }
        assert!(
            observed.replies.is_empty(),
            "cancelled spawn cannot emit a late acknowledgement"
        );
        assert!(
            observed.exited.is_none(),
            "disconnected client receives no exit notification"
        );
        assert!(
            outgoing
                .lock_request_contexts_for_test()
                .await
                .keys()
                .all(|id| id.connection_id != TEST_CONNECTION_ID)
        );
        harness.shutdown().await;
        Ok(())
    }

    #[test]
    #[serial(app_server_tracing)]
    fn process_rpc_unpublished_spawn_disconnect_terminates_before_metadata_cleanup() -> Result<()> {
        run_current_thread_test_with_stack(
            "process_rpc_unpublished_spawn_disconnect_terminates_before_metadata_cleanup",
            disconnect_terminates_before_metadata_cleanup(false),
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn process_rpc_running_disconnect_terminates_before_metadata_cleanup() -> Result<()> {
        run_current_thread_test_with_stack(
            "process_rpc_running_disconnect_terminates_before_metadata_cleanup",
            disconnect_terminates_before_metadata_cleanup(true),
        )
    }
}

#[cfg(windows)]
mod command_exec_control_rpc_tests {
    use super::*;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use codex_app_server_protocol::CommandExecResponse;
    use codex_app_server_protocol::CommandExecTerminateParams;
    use codex_app_server_protocol::CommandExecWriteParams;
    use codex_app_server_protocol::JSONRPCErrorError;
    use codex_app_server_protocol::ServerNotification;
    use pretty_assertions::assert_eq;
    use std::time::Duration;

    type Replies = BTreeMap<i64, std::result::Result<serde_json::Value, JSONRPCErrorError>>;
    const BUSY: &str =
        "command/exec stdin write queue is full; retry after a pending write completes";

    async fn submit(harness: &TracingHarness, request: ClientRequest) {
        harness
            .processor
            .process_request(
                TEST_CONNECTION_ID,
                request_from_client_request(request),
                &AppServerTransport::Stdio,
                Arc::clone(&harness.session),
            )
            .await;
    }

    fn record_reply(envelope: OutgoingEnvelope, replies: &mut Replies) {
        let OutgoingEnvelope::ToConnection {
            connection_id,
            message,
            ..
        } = envelope
        else {
            return;
        };
        assert_eq!(connection_id, TEST_CONNECTION_ID);
        let (id, result) = match message {
            OutgoingMessage::Response(response) => (response.id, Ok(response.result)),
            OutgoingMessage::Error(error) => (error.id, Err(error.error)),
            _ => return,
        };
        let RequestId::Integer(id) = id else {
            panic!("integer request id required");
        };
        assert!(
            replies.insert(id, result).is_none(),
            "duplicate RPC reply for {id}"
        );
    }

    async fn replies_until(
        harness: &mut TracingHarness,
        replies: &mut Replies,
        label: &str,
        done: impl Fn(&Replies) -> bool,
    ) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !done(replies) {
                record_reply(
                    harness.outgoing_rx.recv().await.expect("outgoing open"),
                    replies,
                );
            }
            while let Ok(envelope) = harness.outgoing_rx.try_recv() {
                record_reply(envelope, replies);
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{label}: received reply ids {:?}",
                replies.keys().collect::<Vec<_>>()
            )
        });
    }

    async fn start_gated_reader(
        harness: &mut TracingHarness,
        dir: &Path,
        process_id: &str,
        request_id: i64,
    ) {
        let gate = dir
            .join("read-stdin")
            .display()
            .to_string()
            .replace('\'', "''");
        let output = dir
            .join("received.bin")
            .display()
            .to_string()
            .replace('\'', "''");
        let script = format!(
            "[Console]::Out.WriteLine('stdin-ready'); [Console]::Out.Flush(); \
             while (-not [IO.File]::Exists('{gate}')) {{ Start-Sleep -Milliseconds 10 }}; \
             $source = [Console]::OpenStandardInput(); $destination = [IO.File]::Create('{output}'); \
             try {{ $source.CopyTo($destination) }} finally {{ $destination.Dispose() }}"
        );
        submit(
            harness,
            ClientRequest::OneOffCommandExec {
                request_id: RequestId::Integer(request_id),
                params: serde_json::from_value(json!({
                    "command": ["powershell.exe", "-NoLogo", "-NoProfile", "-Command", script],
                    "processId": process_id,
                    "streamStdin": true,
                    "streamStdoutStderr": true,
                    "disableTimeout": true,
                    "sandboxPolicy": {"type": "dangerFullAccess"},
                    "cwd": dir,
                }))
                .expect("valid command parameters"),
            },
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut output = Vec::new();
            loop {
                let OutgoingEnvelope::ToConnection { message, .. } =
                    harness.outgoing_rx.recv().await.expect("outgoing open")
                else {
                    continue;
                };
                match message {
                    OutgoingMessage::AppServerNotification(
                        ServerNotification::CommandExecOutputDelta(delta),
                    ) if delta.process_id == process_id => {
                        output.extend(STANDARD.decode(delta.delta_base64).expect("output base64"));
                        if String::from_utf8_lossy(&output).contains("stdin-ready") {
                            break;
                        }
                    }
                    OutgoingMessage::Error(error) => panic!("command startup failed: {error:?}"),
                    OutgoingMessage::Response(response) => {
                        panic!("child exited before readiness: {response:?}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("real child must report readiness before writes");
    }

    async fn write(
        harness: &TracingHarness,
        process_id: &str,
        request_id: i64,
        bytes: &[u8],
        close_stdin: bool,
    ) {
        submit(
            harness,
            ClientRequest::CommandExecWrite {
                request_id: RequestId::Integer(request_id),
                params: CommandExecWriteParams {
                    process_id: process_id.to_string(),
                    delta_base64: Some(STANDARD.encode(bytes)),
                    close_stdin,
                },
            },
        )
        .await;
    }

    // Batches stay below the ordinary RPC queue count/byte limits. Each malformed
    // write is an observable FIFO admission barrier on the same normal RPC lane.
    // A single large write is insufficient: the real process driver buffers stdin.
    async fn saturate(
        harness: &mut TracingHarness,
        process_id: &str,
        replies: &mut Replies,
    ) -> BTreeMap<i64, Vec<u8>> {
        let mut attempted = BTreeMap::new();
        for batch in 0..32_i64 {
            for offset in 0..8_i64 {
                let index = batch * 8 + offset;
                let id = 61_000 + index;
                let mut bytes = vec![b'a' + (index % 26) as u8; 64 * 1024];
                bytes[..8].copy_from_slice(&index.to_le_bytes());
                write(harness, process_id, id, &bytes, false).await;
                attempted.insert(id, bytes);
            }
            let barrier = 62_000 + batch;
            submit(
                harness,
                ClientRequest::CommandExecWrite {
                    request_id: RequestId::Integer(barrier),
                    params: CommandExecWriteParams {
                        process_id: process_id.to_string(),
                        delta_base64: Some("%%%".to_string()),
                        close_stdin: false,
                    },
                },
            )
            .await;
            replies_until(harness, replies, "write admission barrier", |r| {
                r.contains_key(&barrier)
            })
            .await;
            assert!(
                replies[&barrier]
                    .as_ref()
                    .expect_err("malformed base64 rejected")
                    .message
                    .starts_with("invalid deltaBase64:")
            );
        }
        for id in attempted.keys() {
            if let Some(result) = replies.get(id) {
                match result {
                    Ok(value) => assert_eq!(value, &json!({})),
                    Err(error) => assert_eq!(error.message, BUSY),
                }
            }
        }
        assert!(
            attempted
                .keys()
                .any(|id| replies.get(id).is_some_and(|reply| reply.is_ok())),
            "real driver must accept earlier writes"
        );
        assert!(
            attempted
                .keys()
                .any(|id| replies.get(id).is_some_and(|reply| reply.is_err())),
            "normal RPC must reject writes once owner capacity is full"
        );
        assert_eq!(
            attempted
                .keys()
                .filter(|id| !replies.contains_key(id))
                .count(),
            32,
            "the public write-owner capacity includes every unresolved accepted acknowledgement"
        );
        attempted
    }

    #[test]
    #[serial(app_server_tracing)]
    fn command_exec_rpc_backpressured_writes_do_not_block_terminate() -> Result<()> {
        run_current_thread_test_with_stack(
            "command_exec_rpc_backpressured_writes_do_not_block_terminate",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let process_id = "rpc-backpressured-terminate";
                start_gated_reader(&mut harness, dir.path(), process_id, 60_000).await;
                let mut replies = Replies::new();
                let attempted = saturate(&mut harness, process_id, &mut replies).await;
                let pending = attempted
                    .keys()
                    .filter(|id| !replies.contains_key(id))
                    .copied()
                    .collect::<Vec<_>>();
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "child has not read any bytes"
                );
                submit(
                    &harness,
                    ClientRequest::CommandExecTerminate {
                        request_id: RequestId::Integer(63_000),
                        params: CommandExecTerminateParams {
                            process_id: process_id.to_string(),
                        },
                    },
                )
                .await;
                replies_until(
                    &mut harness,
                    &mut replies,
                    "terminate and pending write cleanup",
                    |r| {
                        r.contains_key(&63_000)
                            && r.contains_key(&60_000)
                            && attempted.keys().all(|id| r.contains_key(id))
                    },
                )
                .await;
                assert_eq!(
                    replies[&63_000].as_ref().expect("terminate RPC succeeds"),
                    &json!({})
                );
                let result: CommandExecResponse = serde_json::from_value(
                    replies[&60_000]
                        .as_ref()
                        .expect("final child response")
                        .clone(),
                )?;
                assert_ne!(
                    result.exit_code, 0,
                    "terminate must actually kill the blocked child"
                );
                for id in pending {
                    // The write acknowledgement means admission to the process
                    // driver's queue. Killing the child can release that queue
                    // before the stdin worker observes exit and is aborted.
                    // All previously blocked owners must settle either way.
                    match replies[&id].as_ref() {
                        Ok(value) => assert_eq!(value, &json!({})),
                        Err(error) => assert!(
                            error.message.contains("no longer running")
                                || error.message == "stdin is already closed",
                            "unexpected write cleanup error: {error:?}"
                        ),
                    }
                }
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "termination must not release the file gate"
                );
                write(&harness, process_id, 63_001, b"after-exit", false).await;
                replies_until(
                    &mut harness,
                    &mut replies,
                    "removed process rejects writes",
                    |r| r.contains_key(&63_001),
                )
                .await;
                assert!(
                    replies[&63_001]
                        .as_ref()
                        .expect_err("process removed")
                        .message
                        .contains("no active command/exec")
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn command_exec_rpc_queued_writes_preserve_fifo_and_busy_rejection_has_no_side_effect()
    -> Result<()> {
        run_current_thread_test_with_stack(
            "command_exec_rpc_queued_writes_preserve_fifo_and_busy_rejection_has_no_side_effect",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let process_id = "rpc-backpressured-fifo";
                start_gated_reader(&mut harness, dir.path(), process_id, 60_000).await;
                let mut replies = Replies::new();
                let attempted = saturate(&mut harness, process_id, &mut replies).await;
                write(
                    &harness,
                    process_id,
                    63_000,
                    b"REJECTED-PAYLOAD-MUST-NOT-REACH-STDIN",
                    true,
                )
                .await;
                replies_until(
                    &mut harness,
                    &mut replies,
                    "saturated close-stdin rejection",
                    |r| r.contains_key(&63_000),
                )
                .await;
                assert_eq!(
                    replies[&63_000]
                        .as_ref()
                        .expect_err("full capacity rejects before mutation")
                        .message,
                    BUSY
                );
                assert!(!dir.path().join("received.bin").exists());
                std::fs::write(dir.path().join("read-stdin"), b"release real child")?;
                replies_until(
                    &mut harness,
                    &mut replies,
                    "accepted write acknowledgements after child begins reading",
                    |r| attempted.keys().all(|id| r.contains_key(id)),
                )
                .await;
                let tail = b"ACCEPTED-TAIL-AFTER-REJECTED-CLOSE";
                write(&harness, process_id, 63_001, tail, true).await;
                replies_until(
                    &mut harness,
                    &mut replies,
                    "tail acknowledgement and real child EOF",
                    |r| r.contains_key(&63_001) && r.contains_key(&60_000),
                )
                .await;
                assert_eq!(
                    replies[&63_001]
                        .as_ref()
                        .expect("rejected close must leave stdin open"),
                    &json!({})
                );
                let result: CommandExecResponse = serde_json::from_value(
                    replies[&60_000]
                        .as_ref()
                        .expect("final child response")
                        .clone(),
                )?;
                assert_eq!(result.exit_code, 0);
                let mut expected = Vec::new();
                let mut rejected = 0;
                for (id, bytes) in &attempted {
                    match replies[id].as_ref() {
                        Ok(value) => {
                            assert_eq!(value, &json!({}));
                            expected.extend_from_slice(bytes);
                        }
                        Err(error) => {
                            assert_eq!(error.message, BUSY);
                            rejected += 1;
                        }
                    }
                }
                assert!(rejected > 0);
                expected.extend_from_slice(tail);
                let actual = std::fs::read(dir.path().join("received.bin"))?;
                assert!(
                    actual == expected,
                    "child bytes must exactly equal acknowledged inputs in submission order followed by the tail (actual {} bytes, expected {} bytes)",
                    actual.len(),
                    expected.len()
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }

    #[test]
    #[serial(app_server_tracing)]
    fn command_exec_rpc_disconnect_cancels_and_joins_pending_write_owners() -> Result<()> {
        run_current_thread_test_with_stack(
            "command_exec_rpc_disconnect_cancels_and_joins_pending_write_owners",
            async {
                let mut harness = TracingHarness::new().await?;
                let dir = TempDir::new()?;
                let process_id = "rpc-backpressured-disconnect";
                start_gated_reader(&mut harness, dir.path(), process_id, 60_000).await;
                let mut replies = Replies::new();
                let attempted = saturate(&mut harness, process_id, &mut replies).await;
                let pending = attempted
                    .keys()
                    .filter(|id| !replies.contains_key(id))
                    .copied()
                    .collect::<Vec<_>>();
                assert!(
                    harness.session.rpc_gate.inflight_count() > 0,
                    "pending normal writes must have connection-owned completion tasks"
                );
                tokio::time::timeout(
                    Duration::from_secs(5),
                    harness
                        .processor
                        .connection_closed(TEST_CONNECTION_ID, &harness.session),
                )
                .await
                .expect("disconnect must cancel acknowledgements without waiting for stdin");
                assert_eq!(
                    harness.session.rpc_gate.inflight_count(),
                    0,
                    "connection close must join every write response owner"
                );
                // The harness observes the internal terminal response even though the
                // real transport has disconnected. Its exit code proves child death.
                replies_until(
                    &mut harness,
                    &mut replies,
                    "disconnected child termination",
                    |r| r.contains_key(&60_000),
                )
                .await;
                let result: CommandExecResponse = serde_json::from_value(
                    replies[&60_000]
                        .as_ref()
                        .expect("terminal process result")
                        .clone(),
                )?;
                assert_ne!(result.exit_code, 0);
                for id in pending {
                    assert!(
                        !replies.contains_key(&id),
                        "cancelled write {id} must not emit a late reply"
                    );
                }
                let contexts = harness
                    .processor
                    .outgoing
                    .lock_request_contexts_for_test()
                    .await;
                assert!(
                    contexts
                        .keys()
                        .all(|id| id.connection_id != TEST_CONNECTION_ID),
                    "disconnect must clear request contexts"
                );
                drop(contexts);
                assert!(
                    !dir.path().join("received.bin").exists(),
                    "disconnect kills the gated child without releasing stdin"
                );
                harness.shutdown().await;
                Ok(())
            },
        )
    }
}

#[test]
#[serial(app_server_tracing)]
fn interrupt_request_cancellation_releases_reservation_before_terminal() -> Result<()> {
    run_current_thread_test_with_stack(
        "interrupt_request_cancellation_releases_reservation_before_terminal",
        async {
            use crate::outgoing_message::ConnectionRequestId;
            use crate::outgoing_message::OutgoingEnvelope;
            use crate::outgoing_message::OutgoingMessage;
            use codex_app_server_protocol::ServerNotification;
            use codex_app_server_protocol::TurnInterruptParams;
            use codex_app_server_protocol::TurnStatus;
            use std::time::Duration;
            const CANCELLED: i64 = 48_390;
            const HEALTHY: i64 = 48_391;

            async fn next_message(harness: &mut TracingHarness) -> OutgoingMessage {
                let envelope =
                    tokio::time::timeout(Duration::from_secs(5), harness.outgoing_rx.recv())
                        .await
                        .expect("message deadline")
                        .expect("outgoing open");
                let message = match envelope {
                    OutgoingEnvelope::ToConnection {
                        message,
                        write_complete_tx,
                        ..
                    } => {
                        if let Some(receipt) = write_complete_tx {
                            let _ = receipt.send(());
                        }
                        message
                    }
                    OutgoingEnvelope::Broadcast { message } => message,
                };
                match &message {
                    OutgoingMessage::Response(response) => assert_ne!(
                        response.id,
                        RequestId::Integer(CANCELLED),
                        "cancelled interrupt responded"
                    ),
                    OutgoingMessage::Error(error) => panic!("unexpected RPC error: {error:?}"),
                    _ => {}
                }
                message
            }

            let server = MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/responses"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            app_test_support::create_final_assistant_message_sse_response(
                                "too late",
                            )?,
                        )
                        .set_delay(Duration::from_secs(60)),
                )
                .mount(&server)
                .await;
            let mut harness = TracingHarness::new_with_server_and_thread_config_loader(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
            )
            .await?;
            let started = harness.start_thread(48_380, None).await;
            let thread_id = started.thread.id;
            let parsed = ThreadId::from_string(&thread_id)?;

            let live_session = Arc::new(ConnectionSessionState::new());
            let initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));
            harness
                .processor
                .outgoing
                .connection_opened(SECOND_TEST_CONNECTION_ID, Arc::clone(&initialized))
                .await;
            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::Initialize {
                        request_id: RequestId::Integer(48_381),
                        params: InitializeParams {
                            client_info: ClientInfo {
                                name: "codex-app-server-tests".to_string(),
                                title: None,
                                version: "0.1.0".to_string(),
                            },
                            capabilities: Some(InitializeCapabilities {
                                experimental_api: true,
                                ..Default::default()
                            }),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&live_session),
                )
                .await;
            loop {
                if matches!(next_message(&mut harness).await, OutgoingMessage::Response(response) if response.id == RequestId::Integer(48_381))
                {
                    break;
                }
            }
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .connection_initialized(
                    SECOND_TEST_CONNECTION_ID,
                    ConnectionCapabilities {
                        request_attestation: live_session.request_attestation(),
                        experimental_api: live_session.experimental_api_enabled(),
                    },
                )
                .await;
            initialized.store(true, Ordering::Release);
            assert!(
                harness
                    .processor
                    .thread_processor
                    .thread_state_manager
                    .try_add_connection_to_thread(parsed, SECOND_TEST_CONNECTION_ID)
                    .await
            );

            let turn: TurnStartResponse = harness.request(ClientRequest::TurnStart {
                request_id: RequestId::Integer(48_382),
                params: serde_json::from_value(json!({"threadId": thread_id, "input": [{"type": "text", "text": "wait for interrupt", "textElements": []}]}))?,
            }, None).await;
            let turn_id = turn.turn.id;
            let state = harness
                .processor
                .thread_processor
                .thread_state_manager
                .thread_state(parsed)
                .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let requests = harness
                        ._server
                        .received_requests()
                        .await
                        .expect("request log");
                    if requests
                        .iter()
                        .any(|request| request.url.path() == "/v1/responses")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("model request must be in flight");
            let state_guard = state.lock().await;
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::TurnInterrupt {
                        request_id: RequestId::Integer(CANCELLED),
                        params: TurnInterruptParams {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            let outgoing = Arc::clone(&harness.processor.outgoing);
            let trace_guard = outgoing.lock_request_contexts_for_test().await;
            drop(state_guard);
            tokio::time::timeout(Duration::from_secs(5), async {
                while !state.lock().await.has_pending_interrupts() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("interrupt reserved before metadata lookup");
            let processor = Arc::clone(&harness.processor);
            let session = Arc::clone(&harness.session);
            let close = tokio::spawn(async move {
                processor
                    .connection_closed(TEST_CONNECTION_ID, &session)
                    .await;
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.lock().await.has_pending_interrupts() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancel must invalidate interrupt before blocked lookup resumes");
            drop(trace_guard);
            tokio::time::timeout(Duration::from_secs(5), close)
                .await
                .expect("close deadline")
                .expect("close task");
            let cancelled_id = ConnectionRequestId {
                connection_id: TEST_CONNECTION_ID,
                request_id: RequestId::Integer(CANCELLED),
            };
            assert!(
                !outgoing
                    .lock_request_contexts_for_test()
                    .await
                    .contains_key(&cancelled_id)
            );
            let thread = harness.processor.thread_manager.get_thread(parsed).await?;
            assert_eq!(
                thread.agent_status().await,
                codex_protocol::protocol::AgentStatus::Running,
                "cancelled pre-submit interrupt must not stop the turn"
            );

            harness
                .processor
                .process_request(
                    SECOND_TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::TurnInterrupt {
                        request_id: RequestId::Integer(HEALTHY),
                        params: TurnInterruptParams {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&live_session),
                )
                .await;
            let (mut response, mut terminal) = (false, false);
            while !response || !terminal {
                match next_message(&mut harness).await {
                    OutgoingMessage::Response(message)
                        if message.id == RequestId::Integer(HEALTHY) =>
                    {
                        assert!(!response, "one healthy interrupt response");
                        assert_eq!(message.result, json!({}));
                        response = true;
                    }
                    OutgoingMessage::AppServerNotification(ServerNotification::TurnCompleted(
                        message,
                    )) => {
                        assert_eq!(message.thread_id, thread_id);
                        assert_eq!(message.turn.id, turn_id);
                        assert_eq!(message.turn.status, TurnStatus::Interrupted);
                        terminal = true;
                    }
                    _ => {}
                }
            }
            assert!(!state.lock().await.has_pending_interrupts());
            assert!(
                !outgoing
                    .lock_request_contexts_for_test()
                    .await
                    .contains_key(&ConnectionRequestId {
                        connection_id: SECOND_TEST_CONNECTION_ID,
                        request_id: RequestId::Integer(HEALTHY)
                    })
            );
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn registered_device_code_cancellation_respects_auth_commit_boundary() -> Result<()> {
    use crate::outgoing_message::{OutgoingEnvelope, OutgoingMessage};
    use codex_app_server_protocol::{
        CancelLoginAccountParams, CancelLoginAccountResponse, CancelLoginAccountStatus,
        LoginAccountParams, LoginAccountResponse, ServerNotification,
    };
    use std::time::Duration;
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{method, path},
    };

    struct ReleaseWorker(Option<std::sync::mpsc::Sender<()>>);
    impl Drop for ReleaseWorker {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }
    for admit_commit in [false, true] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()?;
        runtime.block_on(async {
            let mut harness = TracingHarness::new().await?;
            let issuer = harness._server.uri();
            let id_token = app_test_support::encode_id_token(
                &app_test_support::ChatGptIdTokenClaims::new()
                    .email("commit@example.com").plan_type("pro")
                    .chatgpt_account_id("workspace-commit"),
            )?;
            for (endpoint, body) in [
                ("/api/accounts/deviceauth/usercode", json!({"device_auth_id":"device-commit", "user_code":"CODE-COMMIT", "interval":"0"})),
                ("/api/accounts/deviceauth/token", json!({"authorization_code":"code-commit", "code_challenge":"challenge-commit", "code_verifier":"verifier-commit"})),
                ("/oauth/token", json!({"id_token":id_token, "access_token":"access-commit", "refresh_token":"refresh-commit"})),
            ] {
                Mock::given(method("POST")).and(path(endpoint))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body))
                    .expect(1).mount(&harness._server).await;
            }
            let (prepared_tx, prepared_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let (committing_tx, committing_rx) = tokio::sync::oneshot::channel();
            harness.processor.account_processor.observe_device_code_commit_for_test(
                issuer, prepared_tx, release_rx, committing_tx,
            );
            let login: LoginAccountResponse = harness.request(ClientRequest::LoginAccount {
                request_id: RequestId::Integer(500), params: LoginAccountParams::ChatgptDeviceCode,
            }, None).await;
            let LoginAccountResponse::ChatgptDeviceCode { login_id, user_code, .. } = login else {
                panic!("expected normal device-code login response");
            };
            assert_eq!(user_code, "CODE-COMMIT");
            tokio::time::timeout(Duration::from_secs(5), prepared_rx).await??;
            let auth_path = harness._codex_home.path().join("auth.json");
            assert!(!auth_path.exists(), "exchanged credentials have not committed yet");

            // Hold the actual worker used by save_auth, after real OAuth exchange.
            let (worker_started, entered) = tokio::sync::oneshot::channel();
            let (worker_release, release) = std::sync::mpsc::channel();
            let worker_release = ReleaseWorker(Some(worker_release));
            let worker = tokio::task::spawn_blocking(move || {
                let _ = worker_started.send(());
                release.recv_timeout(Duration::from_secs(15)).expect("release auth persistence worker");
            });
            entered.await?;
            let mut release_tx = Some(release_tx);
            let mut committing_rx = Some(committing_rx);
            if admit_commit {
                release_tx.take().unwrap().send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), committing_rx.take().unwrap()).await??;
            }
            let canceled: CancelLoginAccountResponse = harness.request(ClientRequest::CancelLoginAccount {
                request_id: RequestId::Integer(501),
                params: CancelLoginAccountParams { login_id: login_id.clone() },
            }, None).await;
            assert_eq!(canceled.status, if admit_commit { CancelLoginAccountStatus::NotFound } else { CancelLoginAccountStatus::Canceled });
            assert!(!auth_path.exists(), "native auth persistence is still blocked");
            if let Some(release) = release_tx.take() { release.send(()).unwrap(); }
            drop(worker_release);
            worker.await?;

            let mut saw_completion = false;
            let mut saw_updated = false;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !saw_completion || (admit_commit && !saw_updated) {
                    let envelope = harness.outgoing_rx.recv().await.expect("login notification");
                    let message = match envelope {
                        OutgoingEnvelope::ToConnection { message, .. } | OutgoingEnvelope::Broadcast { message } => message,
                    };
                    match message {
                        OutgoingMessage::AppServerNotification(ServerNotification::AccountLoginCompleted(completed)) => {
                            assert!(!saw_completion, "exactly one terminal login result");
                            assert_eq!(completed.login_id.as_deref(), Some(login_id.as_str()));
                            assert_eq!(completed.success, admit_commit);
                            assert_eq!(completed.error.is_none(), admit_commit);
                            saw_completion = true;
                        }
                        OutgoingMessage::AppServerNotification(ServerNotification::AccountUpdated(updated)) => {
                            assert!(admit_commit, "accepted cancellation forbids account updates");
                            assert_eq!(updated.auth_mode, Some(codex_app_server_protocol::AuthMode::Chatgpt));
                            saw_updated = true;
                        }
                        _ => {}
                    }
                }
            }).await?;
            tokio::task::spawn_blocking(|| ()).await?;
            if admit_commit {
                let auth: serde_json::Value = serde_json::from_slice(&std::fs::read(&auth_path)?)?;
                assert_eq!(auth["tokens"]["access_token"], "access-commit");
                assert_eq!(auth["tokens"]["refresh_token"], "refresh-commit");
                assert_eq!(auth["tokens"]["account_id"], "workspace-commit");
            } else {
                assert!(committing_rx.take().unwrap().await.is_err(), "accepted cancellation never admitted persistence");
                assert!(!auth_path.exists(), "accepted cancellation forbids late auth.json after workers drain");
                assert!(!saw_updated);
                assert!(tokio::time::timeout(Duration::from_millis(100), harness.outgoing_rx.recv()).await.is_err(),
                    "cancelled login must not publish a late account update");
            }
            harness.shutdown().await;
            Ok::<(), anyhow::Error>(())
        })?;
    }
    Ok(())
}

#[test]
#[serial(app_server_tracing)]
fn registered_api_key_login_persists_before_reply_and_survives_shutdown() -> Result<()> {
    use crate::outgoing_message::{OutgoingEnvelope, OutgoingMessage};
    use codex_app_server_protocol::{
        AccountUpdatedNotification, LoginAccountParams, LoginAccountResponse, ServerNotification,
    };
    use std::time::Duration;

    for phase in ["success", "admitted_closed", "closed", "write_error"] {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let home = runtime.block_on(async {
            let mut harness = TracingHarness::new().await?;
            let auth_path = harness._codex_home.path().join("auth.json");
            assert!(!auth_path.exists());
            if phase == "write_error" {
                // A real filesystem failure, without replacing the persistence implementation.
                std::fs::create_dir(&auth_path)?;
            }
            if phase == "closed" {
                harness
                    .processor
                    .connection_closed(TEST_CONNECTION_ID, &harness.session)
                    .await;
            }
            if phase == "admitted_closed" {
                // A stalled transport fills the actual response queue. This holds the
                // normal handler after persistence but before any success can be sent.
                while harness.processor.outgoing.try_send_server_notification(
                    ServerNotification::AccountUpdated(AccountUpdatedNotification {
                        auth_mode: None,
                        plan_type: None,
                    }),
                ) {}
            }
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::LoginAccount {
                        request_id: RequestId::Integer(419_920),
                        params: LoginAccountParams::ApiKey {
                            api_key: "sk-shutdown-contract".to_string(),
                        },
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;

            if phase == "admitted_closed" {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !auth_path.is_file() {
                        tokio::task::yield_now().await;
                    }
                })
                .await?;
                assert_eq!(
                    harness.session.rpc_gate.inflight_count(),
                    1,
                    "reply remains blocked in the registered request owner"
                );
                let auth: serde_json::Value = serde_json::from_slice(&std::fs::read(&auth_path)?)?;
                assert_eq!(auth["OPENAI_API_KEY"], "sk-shutdown-contract");
            } else if phase != "closed" {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let message =
                            match harness.outgoing_rx.recv().await.expect("login response") {
                                OutgoingEnvelope::ToConnection { message, .. }
                                | OutgoingEnvelope::Broadcast { message } => message,
                            };
                        match message {
                            OutgoingMessage::Response(response)
                                if response.id == RequestId::Integer(419_920) =>
                            {
                                assert_eq!(
                                    phase, "success",
                                    "failed persistence cannot acknowledge login"
                                );
                                let result: LoginAccountResponse =
                                    serde_json::from_value(response.result)?;
                                assert_eq!(result, LoginAccountResponse::ApiKey {});
                                let auth: serde_json::Value =
                                    serde_json::from_slice(&std::fs::read(&auth_path)?)?;
                                assert_eq!(auth["OPENAI_API_KEY"], "sk-shutdown-contract");
                                assert_eq!(auth["auth_mode"], "apikey");
                                break;
                            }
                            OutgoingMessage::Error(error)
                                if error.id == RequestId::Integer(419_920) =>
                            {
                                assert_eq!(phase, "write_error");
                                assert!(error.error.message.contains("failed to save api key"));
                                break;
                            }
                            _ => {}
                        }
                    }
                    Ok::<(), anyhow::Error>(())
                })
                .await??;
            }
            harness
                .processor
                .connection_closed(TEST_CONNECTION_ID, &harness.session)
                .await;
            assert_eq!(harness.session.rpc_gate.inflight_count(), 0);
            harness.processor.thread_processor.shutdown_threads().await;
            harness.processor.drain_background_tasks().await;
            while let Ok(envelope) = harness.outgoing_rx.try_recv() {
                let message = match envelope {
                    OutgoingEnvelope::ToConnection { message, .. }
                    | OutgoingEnvelope::Broadcast { message } => message,
                };
                assert!(
                    !matches!(message, OutgoingMessage::Response(response)
                    if response.id == RequestId::Integer(419_920)),
                    "no late or duplicate login success"
                );
            }
            Ok::<TempDir, anyhow::Error>(harness._codex_home)
        })?;
        drop(runtime);
        let auth_path = home.path().join("auth.json");
        match phase {
            "success" | "admitted_closed" => {
                let auth: serde_json::Value = serde_json::from_slice(&std::fs::read(auth_path)?)?;
                assert_eq!(auth["OPENAI_API_KEY"], "sk-shutdown-contract");
                assert_eq!(auth["auth_mode"], "apikey");
                assert!(auth["tokens"].is_null());
            }
            "closed" => assert!(
                !auth_path.exists(),
                "closed request owner must never persist credentials"
            ),
            "write_error" => assert!(
                auth_path.is_dir(),
                "failed write must leave the original filesystem entry intact"
            ),
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[test]
#[serial(app_server_tracing)]
fn acknowledged_goal_set_finishes_runtime_update_after_rpc_gate_cancellation() -> Result<()> {
    run_current_thread_test_with_stack(
        "acknowledged_goal_set_finishes_runtime_update_after_rpc_gate_cancellation",
        async {
            use crate::outgoing_message::{OutgoingEnvelope, OutgoingMessage};
            use codex_app_server_protocol::{ServerNotification, ThreadGoalSetResponse};
            use std::time::Duration;

            let server = MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/responses"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            app_test_support::create_final_assistant_message_sse_response("Done")?,
                        )
                        .set_delay(Duration::from_secs(60)),
                )
                .mount(&server)
                .await;
            let mut harness = TracingHarness::new_with_features(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
                true,
            )
            .await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(428_900),
                        params: ThreadStartParams::default(),
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = ThreadId::from_string(&started.thread.id)?;
            let thread_state = harness
                .processor
                .thread_processor
                .thread_state_manager
                .thread_state(thread_id)
                .await;
            let (command_tx, listener_cancellation) = thread_state
                .lock()
                .await
                .listener_command_route()
                .expect("normal thread start registers listener");
            // Reserve the actual queue capacity as competing producers. The registered
            // listener stays live, while the goal notification cannot yet be enqueued.
            let mut reservations = Vec::new();
            for _ in 0..command_tx.max_capacity() {
                reservations.push(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        command_tx.clone().reserve_owned(),
                    )
                    .await
                    .expect("queue reservation deadline")
                    .expect("live listener"),
                );
            }
            let response: ThreadGoalSetResponse = harness
                .request(
                    ClientRequest::ThreadGoalSet {
                        request_id: RequestId::Integer(428_901),
                        params: serde_json::from_value(json!({
                            "threadId": started.thread.id,
                            "objective": "continue after the request owner closes",
                            "status": "active",
                        }))?,
                    },
                    None,
                )
                .await;
            assert_eq!(
                response.goal.objective,
                "continue after the request owner closes"
            );
            assert_eq!(
                response.goal.status,
                codex_app_server_protocol::ThreadGoalStatus::Active
            );
            assert_eq!(command_tx.capacity(), 0);
            assert!(
                harness
                    ._server
                    .received_requests()
                    .await
                    .expect("model requests")
                    .iter()
                    .all(|request| request.url.path() != "/v1/responses"),
                "runtime continuation must follow ordered notification admission"
            );
            // Use the same gate shutdown boundary as transport disconnect, while keeping
            // listener ownership live to distinguish this from listener-stop cancellation.
            harness.session.rpc_gate.shutdown().await;
            assert!(!listener_cancellation.is_cancelled());
            drop(reservations);
            let mut goal_notification = false;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let message = match harness.outgoing_rx.recv().await.expect("outgoing open") {
                        OutgoingEnvelope::Broadcast { message }
                        | OutgoingEnvelope::ToConnection { message, .. } => message,
                    };
                    match message {
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::ThreadGoalUpdated(notification),
                        ) => {
                            assert_eq!(notification.goal.objective, response.goal.objective);
                            goal_notification = true;
                        }
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::TurnStarted(notification),
                        ) => {
                            assert_eq!(notification.thread_id, started.thread.id);
                            assert!(goal_notification, "goal update precedes its automatic turn");
                            break;
                        }
                        OutgoingMessage::Response(response) => {
                            panic!("unexpected duplicate response: {response:?}")
                        }
                        OutgoingMessage::Error(error) => panic!("unexpected late error: {error:?}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("committed goal must still start its runtime continuation");
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if harness
                        ._server
                        .received_requests()
                        .await
                        .expect("model requests")
                        .iter()
                        .any(|request| request.url.path() == "/v1/responses")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("normal goal runtime must reach the model provider");
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn committed_goal_reaches_runtime_when_transport_stays_full_past_delivery_budget() -> Result<()> {
    run_current_thread_test_with_stack(
        "committed_goal_reaches_runtime_when_transport_stays_full_past_delivery_budget",
        async {
            use codex_app_server_protocol::{ServerNotification, ThreadGoalClearedNotification};
            use std::time::Duration;
            let server = MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .and(wiremock::matchers::path("/v1/responses"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(
                            app_test_support::create_final_assistant_message_sse_response("Done")?,
                        )
                        .set_delay(Duration::from_secs(60)),
                )
                .mount(&server)
                .await;
            let mut harness = TracingHarness::new_with_features(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
                true,
            )
            .await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(428_910),
                        params: ThreadStartParams::default(),
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = ThreadId::from_string(&started.thread.id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            let state_db = thread.state_db().expect("materialized thread database");
            // Exercise the normal no-listener notification route, including a blocked
            // response. Neither transport nor listener capacity is released by the test.
            harness
                .processor
                .thread_processor
                .thread_state_manager
                .clear_all_listeners()
                .await;
            while harness.processor.outgoing.try_send_server_notification(
                ServerNotification::ThreadGoalCleared(ThreadGoalClearedNotification {
                    thread_id: started.thread.id.clone(),
                }),
            ) {}
            assert_eq!(harness.outgoing_rx.capacity(), 0);
            harness
                .processor
                .process_request(
                    TEST_CONNECTION_ID,
                    request_from_client_request(ClientRequest::ThreadGoalSet {
                        request_id: RequestId::Integer(428_911),
                        params: serde_json::from_value(json!({
                            "threadId": started.thread.id,
                            "objective": "continue despite saturated transport",
                            "status": "active",
                        }))?,
                    }),
                    &AppServerTransport::Stdio,
                    Arc::clone(&harness.session),
                )
                .await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(goal) = state_db
                        .thread_goals()
                        .get_thread_goal(thread_id)
                        .await
                        .expect("read durable goal")
                    {
                        assert_eq!(goal.objective, "continue despite saturated transport");
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("normal goal RPC durably commits before cancellation");
            harness.session.rpc_gate.shutdown().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if harness
                        ._server
                        .received_requests()
                        .await
                        .expect("model requests")
                        .iter()
                        .any(|request| request.url.path() == "/v1/responses")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("delivery budget must release committed runtime effects");
            assert_eq!(
                harness.outgoing_rx.capacity(),
                0,
                "runtime continuation must happen while transport remains full"
            );
            assert_eq!(
                state_db
                    .thread_goals()
                    .get_thread_goal(thread_id)
                    .await?
                    .expect("durable goal")
                    .objective,
                "continue despite saturated transport"
            );
            harness.shutdown().await;
            Ok(())
        },
    )
}

#[test]
#[serial(app_server_tracing)]
fn acknowledged_goal_clear_survives_rpc_cancellation_without_resurrecting_goal() -> Result<()> {
    run_current_thread_test_with_stack(
        "acknowledged_goal_clear_survives_rpc_cancellation_without_resurrecting_goal",
        async {
            use crate::outgoing_message::{OutgoingEnvelope, OutgoingMessage};
            use codex_app_server_protocol::{
                ServerNotification, ThreadGoalClearResponse, ThreadGoalSetResponse,
            };
            use std::time::Duration;
            let server = create_mock_responses_server_repeating_assistant("Done").await;
            let mut harness = TracingHarness::new_with_features(
                server,
                Arc::new(codex_config::NoopThreadConfigLoader),
                false,
                true,
            )
            .await?;
            let started: ThreadStartResponse = harness
                .request(
                    ClientRequest::ThreadStart {
                        request_id: RequestId::Integer(429_200),
                        params: ThreadStartParams::default(),
                    },
                    None,
                )
                .await;
            read_thread_started_notification(&mut harness.outgoing_rx).await;
            let thread_id = ThreadId::from_string(&started.thread.id)?;
            let thread = harness
                .processor
                .thread_manager
                .get_thread(thread_id)
                .await?;
            let state_db = thread.state_db().expect("materialized thread database");
            let _: ThreadGoalSetResponse = harness.request(ClientRequest::ThreadGoalSet {
                request_id: RequestId::Integer(429_201),
                params: serde_json::from_value(json!({
                    "threadId": started.thread.id, "objective": "remove this goal", "status": "paused",
                }))?,
            }, None).await;
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let message = match harness.outgoing_rx.recv().await.expect("outgoing open") {
                        OutgoingEnvelope::Broadcast { message }
                        | OutgoingEnvelope::ToConnection { message, .. } => message,
                    };
                    if matches!(
                        message,
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::ThreadGoalUpdated(_)
                        )
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("initial goal notification");
            assert!(
                state_db
                    .thread_goals()
                    .get_thread_goal(thread_id)
                    .await?
                    .is_some()
            );
            let thread_state = harness
                .processor
                .thread_processor
                .thread_state_manager
                .thread_state(thread_id)
                .await;
            let (command_tx, cancellation) = thread_state
                .lock()
                .await
                .listener_command_route()
                .expect("normal registered listener");
            let mut reservations = Vec::new();
            for _ in 0..command_tx.max_capacity() {
                reservations.push(
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        command_tx.clone().reserve_owned(),
                    )
                    .await
                    .expect("queue reservation deadline")
                    .expect("live listener"),
                );
            }
            let cleared: ThreadGoalClearResponse = harness
                .request(
                    ClientRequest::ThreadGoalClear {
                        request_id: RequestId::Integer(429_202),
                        params: serde_json::from_value(json!({"threadId": started.thread.id}))?,
                    },
                    None,
                )
                .await;
            assert!(cleared.cleared);
            assert!(
                state_db
                    .thread_goals()
                    .get_thread_goal(thread_id)
                    .await?
                    .is_none(),
                "clear success requires durable deletion"
            );
            harness.session.rpc_gate.shutdown().await;
            assert!(!cancellation.is_cancelled());
            drop(reservations);
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let message = match harness.outgoing_rx.recv().await.expect("outgoing open") {
                        OutgoingEnvelope::Broadcast { message }
                        | OutgoingEnvelope::ToConnection { message, .. } => message,
                    };
                    match message {
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::ThreadGoalCleared(notification),
                        ) => {
                            assert_eq!(notification.thread_id, started.thread.id);
                            break;
                        }
                        OutgoingMessage::AppServerNotification(
                            ServerNotification::ThreadGoalUpdated(_),
                        ) => panic!("cleared goal must not be resurrected"),
                        OutgoingMessage::Response(response) => {
                            panic!("duplicate clear response: {response:?}")
                        }
                        OutgoingMessage::Error(error) => panic!("late clear error: {error:?}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("owned clear notification survives request cancellation");
            harness.shutdown().await;
            assert!(
                state_db
                    .thread_goals()
                    .get_thread_goal(thread_id)
                    .await?
                    .is_none(),
                "shutdown must not resurrect the cleared goal"
            );
            Ok(())
        },
    )
}
