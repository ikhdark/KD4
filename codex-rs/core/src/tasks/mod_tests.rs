use super::SessionTask;
use super::SessionTaskResult;
use super::TASK_COMPACT_METRIC;
use super::TerminalSchedule;
use super::TurnTerminalOutcome;
use super::emit_compact_metric;
use super::emit_turn_network_proxy_metric;
use crate::session::TurnInput;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::SamplingAdmission;
use crate::state::TaskKind;
use crate::state::TerminalWakeResult;
use crate::state::TurnTerminalCoordinator;
use crate::tools::tool_dispatch_trace::ToolDispatchTimingSnapshot;
use crate::turn_timing::ToolCallTimingLineage;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnTimingToolCallSource;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn terminal_persistence_fixture(turn_id: &str) -> EventMsg {
    EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.into(),
        last_agent_message: Some("canonical answer".into()),
        surfaced_result: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: Some(Default::default()),
    })
}

#[tokio::test(start_paused = true)]
async fn failed_parent_interrupt_fanout_bounds_blocked_queues_and_reaches_ready_sibling() {
    let mut children = [ThreadId::new(), ThreadId::new(), ThreadId::new()];
    children.sort_unstable_by_key(ToString::to_string);
    let (blocked, mut blocked_rx) = tokio::sync::mpsc::channel(1);
    blocked.send(()).await.unwrap();
    let (ready, mut ready_rx) = tokio::sync::mpsc::channel(1);
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(1);
    let ready_child = children[2];
    let attempts = AtomicUsize::new(0);
    let cleanup = super::collect_failed_turn_interrupts(
        vec![children[2], children[0], children[1], children[0]],
        deadline,
        |id| {
            let sender = if id == ready_child {
                ready.clone()
            } else {
                blocked.clone()
            };
            let attempts = &attempts;
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                sender.send(()).await.unwrap();
                "enqueued"
            }
        },
    );
    tokio::pin!(cleanup);
    // Poll the actual fanout owner before advancing time. A serial loop would
    // leave the ready child's channel empty behind the first full queue.
    assert!(futures::poll!(cleanup.as_mut()).is_pending());
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "one attempt per unique child"
    );
    assert_eq!(ready_rx.try_recv(), Ok(()));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        cleanup.await,
        vec![
            (children[0], "timed_out"),
            (children[1], "timed_out"),
            (children[2], "enqueued")
        ]
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(1)
    );
    blocked_rx.recv().await.unwrap();
    assert!(
        blocked_rx.try_recv().is_err(),
        "timed-out sends must be cancelled, not detached"
    );
}

#[tokio::test]
async fn cancelling_failed_parent_fanout_drops_pending_sends() {
    let child = ThreadId::new();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.send(()).await.unwrap();
    {
        let cleanup = super::collect_failed_turn_interrupts(
            vec![child],
            tokio::time::Instant::now() + Duration::from_secs(1),
            |_| async {
                sender.send(()).await.unwrap();
                "enqueued"
            },
        );
        tokio::pin!(cleanup);
        assert!(futures::poll!(cleanup.as_mut()).is_pending());
    }
    receiver.recv().await.unwrap();
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn terminal_durable_only_failure_is_in_required_handoff_and_retry_is_idempotent() {
    let (mut session, context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).unwrap()).await;
    session.flush_rollout().await.unwrap(); // earlier ordinary flush succeeds
    let mut published = None;
    let terminal = session
        .publish_terminal_event_with_barrier(
            &context,
            terminal_persistence_fixture(&context.sub_id),
            &mut published,
            false,
            async || {
                assert!(
                    session
                        .live_thread()
                        .unwrap()
                        .terminal_event(&context.sub_id, false)
                        .await
                        .unwrap()
                        .is_some(),
                    "the terminal itself must precede the barrier"
                );
                Err(std::io::Error::other(
                    "injected final stable-storage failure",
                ))
            },
        )
        .await;
    let EventMsg::TurnComplete(done) = &terminal else {
        unreachable!()
    };
    assert!(
        done.error.is_none(),
        "persistence failure is not execution failure"
    );
    assert_eq!(done.last_agent_message.as_deref(), Some("canonical answer"));
    assert!(
        done.timing
            .as_ref()
            .unwrap()
            .completion_assessment
            .as_ref()
            .unwrap()
            .verification_gaps
            .iter()
            .any(|gap| gap.contains("injected final stable-storage failure"))
    );
    let mut terminals = 0;
    let mut warning_seen = false;
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::Warning(_) => {
                assert_eq!(terminals, 0);
                warning_seen = true;
            }
            EventMsg::TurnComplete(_) => {
                assert!(warning_seen);
                terminals += 1;
            }
            _ => {}
        }
    }
    assert_eq!(terminals, 1);
    let retried = session
        .publish_terminal_event_with_barrier(
            &context,
            terminal_persistence_fixture(&context.sub_id),
            &mut published,
            true,
            async || panic!("a published terminal must not append or flush again"),
        )
        .await;
    assert_eq!(
        serde_json::to_value(terminal).unwrap(),
        serde_json::to_value(retried).unwrap()
    );
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn blocked_terminal_barrier_reports_uncertainty_without_stranding_completion() {
    let (mut session, context, _events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(Arc::get_mut(&mut session).unwrap()).await;
    let mut published = None;
    let terminal = session
        .publish_terminal_event_with_barrier(
            &context,
            terminal_persistence_fixture(&context.sub_id),
            &mut published,
            false,
            async || {
                // Pause only after real ordered I/O has acknowledged the append.
                tokio::time::pause();
                std::future::pending().await
            },
        )
        .await;
    let EventMsg::TurnComplete(done) = terminal else {
        unreachable!()
    };
    assert!(
        done.timing
            .unwrap()
            .completion_assessment
            .unwrap()
            .verification_gaps
            .iter()
            .any(|gap| gap.contains("not acknowledged within 5 seconds"))
    );
    assert!(published.is_some());
}

#[tokio::test]
async fn ephemeral_terminal_does_not_claim_or_wait_for_durability() {
    let (session, context, _events) = make_session_and_context_with_rx().await;
    let mut published = None;
    let terminal = session
        .publish_terminal_event_with_barrier(
            &context,
            terminal_persistence_fixture(&context.sub_id),
            &mut published,
            false,
            async || panic!("ephemeral terminal has no durability barrier"),
        )
        .await;
    let EventMsg::TurnComplete(done) = terminal else {
        unreachable!()
    };
    assert!(done.timing.unwrap().completion_assessment.is_none());
}

#[derive(Clone, Copy)]
struct FenceBlockingTask;

impl SessionTask for FenceBlockingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.terminal_fence"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            cancellation_token.cancelled().await;
            Ok(super::TurnTaskResult::default())
        })
    }
}

#[derive(Clone, Copy)]
struct ImmediateCompletingTask;

struct CapacityStoppedTask;

impl SessionTask for CapacityStoppedTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.capacity_stop"
    }
    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async {
            Ok(super::TurnTaskResult {
                stop_cause: Some(
                    codex_protocol::protocol::TurnExecutionStopCause::SubagentContextBudget,
                ),
                last_agent_message: Some("Partial coverage; remaining scope retained.".into()),
                ..Default::default()
            })
        })
    }
}

#[tokio::test]
async fn audit115_finalizer_preserves_capacity_cause_and_authored_report() {
    let (session, context, events) = make_session_and_context_with_rx().await;
    session
        .spawn_task(context, Vec::new(), CapacityStoppedTask)
        .await;
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EventMsg::TurnComplete(event) = events.recv().await.unwrap().msg {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        terminal.last_agent_message.as_deref(),
        Some("Partial coverage; remaining scope retained.")
    );
    assert!(terminal.error.is_none());
    assert_eq!(
        terminal.timing.unwrap().stop_cause,
        Some(codex_protocol::protocol::TurnExecutionStopCause::SubagentContextBudget)
    );
}

#[derive(Debug)]
struct ReviewStartupFailingModelsManager {
    inner: codex_models_manager::manager::SharedModelsManager,
}

impl codex_models_manager::manager::ModelsManager for ReviewStartupFailingModelsManager {
    fn model_catalog_activity(&self) -> Arc<codex_models_manager::manager::ModelCatalogActivity> {
        self.inner.model_catalog_activity()
    }

    fn refresh_models_for_background(
        &self,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'_, codex_protocol::error::Result<()>>
    {
        self.inner
            .refresh_models_for_background(http_client_factory)
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        codex_protocol::error::Result<codex_protocol::openai_models::ModelsResponse>,
    > {
        self.inner
            .raw_model_catalog(refresh_strategy, http_client_factory)
    }

    fn get_remote_models(
        &self,
    ) -> codex_models_manager::manager::ModelsManagerFuture<
        '_,
        Vec<codex_protocol::openai_models::ModelInfo>,
    > {
        self.inner.get_remote_models()
    }

    fn try_get_remote_models(
        &self,
    ) -> std::result::Result<Vec<codex_protocol::openai_models::ModelInfo>, tokio::sync::TryLockError>
    {
        self.inner.try_get_remote_models()
    }

    fn auth_manager(&self) -> Option<&codex_login::AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<codex_protocol::config_types::CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn get_default_model<'a>(
        &'a self,
        _model: &'a Option<String>,
        _allow_provider_model_fallback: bool,
        _refresh_strategy: codex_models_manager::manager::RefreshStrategy,
        _http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'a, codex_protocol::error::Result<String>>
    {
        Box::pin(async {
            Err(codex_protocol::error::CodexErr::Fatal(
                "injected review model startup failure".to_string(),
            ))
        })
    }

    fn notify_etag(
        self: Arc<Self>,
        etag: String,
        http_client_factory: codex_http_client::HttpClientFactory,
    ) -> codex_models_manager::manager::ModelsManagerFuture<'static, ()> {
        Arc::clone(&self.inner).notify_etag(etag, http_client_factory)
    }
}

impl SessionTask for ImmediateCompletingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.immediate_completion"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async { Ok(super::TurnTaskResult::default()) })
    }
}

struct IncompleteClosureTask {
    accepted: Arc<tokio::sync::Notify>,
}

impl SessionTask for IncompleteClosureTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.incomplete_tool_closure"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            session
                .record_conversation_items(
                    &ctx,
                    &[ResponseItem::FunctionCall {
                        id: None,
                        name: "unresolved_tool".to_string(),
                        namespace: None,
                        arguments: "{}".to_string(),
                        call_id: "unresolved-call".to_string(),
                        internal_chat_message_metadata_passthrough: None,
                    }],
                )
                .await
                .unwrap();
            assert!(ctx.tool_call_acceptance.try_accept(|| {
                ctx.turn_timing_state.try_record_accepted_tool_call(
                    "unresolved-call",
                    &ToolExecutionId("unresolved-execution".to_string()),
                    TurnTimingToolCallSource::Direct,
                    None,
                    "exec_command",
                )
            }));
            self.accepted.notify_one();
            Ok(super::TurnTaskResult::default())
        })
    }
}

struct QueuedToolResultTask {
    queued: Arc<tokio::sync::Notify>,
    release: CancellationToken,
}

impl SessionTask for QueuedToolResultTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.queued_tool_result"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            let call_id = "queued-flush-failure-call";
            let execution_id = ToolExecutionId("queued-flush-failure-execution".to_string());
            assert!(ctx.tool_call_acceptance.try_accept(|| {
                ctx.turn_timing_state.try_record_accepted_tool_call(
                    call_id,
                    &execution_id,
                    TurnTimingToolCallSource::Direct,
                    None,
                    "exec_command",
                )
            }));
            ctx.turn_timing_state.record_tool_dispatch_timing(
                call_id,
                "exec_command",
                TurnTimingToolCallSource::Direct,
                ToolCallTimingLineage::default(),
                ToolDispatchTimingSnapshot {
                    execution_id,
                    outcome: Some("success"),
                    ..ToolDispatchTimingSnapshot::default()
                },
            );
            session
                .record_conversation_items_ordered(
                    &ctx,
                    &[ResponseItem::FunctionCallOutput {
                        id: None,
                        call_id: call_id.to_string(),
                        output: FunctionCallOutputPayload::from_text("queued output".to_string()),
                        internal_chat_message_metadata_passthrough: None,
                    }],
                )
                .await
                .expect("ordered output should be accepted before the injected flush failure");
            self.queued.notify_one();
            tokio::select! {
                _ = self.release.cancelled() => {}
                _ = cancellation_token.cancelled() => {}
            }
            Ok(super::TurnTaskResult::default())
        })
    }
}

#[derive(Clone, Copy)]
struct PanickingTask;

#[derive(Clone, Copy)]
enum FailureMode {
    Panicked,
    Fatal,
    ReturnedError,
    EmittedError,
}

struct FailureOnReleaseTask(CancellationToken, FailureMode);

impl SessionTask for FailureOnReleaseTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.failure_before_input"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            self.0.cancelled().await;
            let message = "failure requires explicit new admission".to_string();
            match self.1 {
                FailureMode::Panicked => panic!("injected worker failure after accepted steering"),
                FailureMode::Fatal => Err(codex_protocol::error::CodexErr::Fatal(message)),
                FailureMode::ReturnedError => {
                    Err(codex_protocol::error::CodexErr::InvalidRequest(message))
                }
                FailureMode::EmittedError => {
                    session
                        .send_event(
                            &ctx,
                            EventMsg::Error(
                                codex_protocol::error::CodexErr::Fatal(message)
                                    .to_error_event(None),
                            ),
                        )
                        .await;
                    // The mid-turn compaction failure path returns Ok and defers input.
                    Ok(super::TurnTaskResult {
                        defer_pending_input: true,
                        ..Default::default()
                    })
                }
            }
        })
    }
}

impl SessionTask for PanickingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.panicking"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move { panic!("injected worker panic") })
    }
}

struct CountingBlockingTask {
    started: Arc<AtomicUsize>,
    release: CancellationToken,
}

impl SessionTask for CountingBlockingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.counting_blocking"
    }

    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::AcqRel);
            tokio::select! {
                _ = self.release.cancelled() => {}
                _ = cancellation_token.cancelled() => {}
            }
            Ok(super::TurnTaskResult::default())
        })
    }
}

fn test_session_telemetry() -> SessionTelemetry {
    let exporter = InMemoryMetricExporter::default();
    let metrics = MetricsClient::new(
        MetricsConfig::in_memory("test", "codex-core", env!("CARGO_PKG_VERSION"), exporter)
            .with_runtime_reader(),
    )
    .expect("in-memory metrics client");
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test_originator".to_string(),
        /*log_user_prompts*/ false,
        "tty".to_string(),
        SessionSource::Cli,
    )
    .with_metrics_without_metadata_tags(metrics)
}

fn find_metric<'a>(resource_metrics: &'a ResourceMetrics, name: &str) -> &'a Metric {
    for scope_metrics in resource_metrics.scope_metrics() {
        for metric in scope_metrics.metrics() {
            if metric.name() == name {
                return metric;
            }
        }
    }
    panic!("metric {name} missing");
}

fn attributes_to_map<'a>(
    attributes: impl Iterator<Item = &'a KeyValue>,
) -> BTreeMap<String, String> {
    attributes
        .map(|kv| (kv.key.as_str().to_string(), kv.value.as_str().to_string()))
        .collect()
}

fn metric_point(resource_metrics: &ResourceMetrics, name: &str) -> (BTreeMap<String, String>, u64) {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::U64(data) => match data {
            MetricData::Sum(sum) => {
                let points: Vec<_> = sum.data_points().collect();
                assert_eq!(points.len(), 1);
                let point = points[0];
                (attributes_to_map(point.attributes()), point.value())
            }
            _ => panic!("unexpected counter aggregation"),
        },
        _ => panic!("unexpected counter data type"),
    }
}

#[tokio::test]
async fn abort_all_tasks_clears_empty_active_turn() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test]
async fn targeted_interrupt_leaves_successor_running_and_settles_missing_target() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    let active_id = turn_context.sub_id.clone();
    session
        .spawn_task(Arc::clone(&turn_context), Vec::new(), FenceBlockingTask)
        .await;
    assert!(
        !session
            .abort_turn_if_active("retired-turn", TurnAbortReason::Interrupted)
            .await
    );
    assert!(
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .is_some()
    );
    assert!(
        session
            .abort_turn_if_active(&active_id, TurnAbortReason::Interrupted)
            .await
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(
        !session
            .abort_turn_if_active(&active_id, TurnAbortReason::Interrupted)
            .await
    );
}

#[tokio::test]
async fn admitted_task_start_retains_recovered_and_steering_input_in_order() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    let input = |text: &str| {
        TurnInput::ResponseItem(ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        })
    };
    let recovered = input("accepted before startup");
    let steering = input("accepted during taskless reservation");
    session
        .input_queue
        .restore_transferred_startup_input(vec![recovered.clone()])
        .await;
    let turn = ActiveTurn::default();
    let turn_state = Arc::clone(&turn.turn_state);
    session
        .input_queue
        .extend_pending_input_for_turn_state(turn_state.as_ref(), std::slice::from_ref(&steering))
        .await
        .expect("accept steering");
    *session.active_turn.lock().await = Some(turn);
    let permit = session
        .task_start_gate
        .acquire()
        .await
        .expect("task admission");
    session
        .start_task_with_admission(&permit, turn_context, Vec::new(), FenceBlockingTask)
        .await
        .expect("install admitted task");
    drop(permit);

    assert!(
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|turn| {
                turn.task.is_some() && Arc::ptr_eq(&turn.turn_state, &turn_state)
            })
    );
    assert_eq!(
        turn_state
            .lock()
            .await
            .input_in_flight
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec![recovered, steering],
        "startup now owns recovered input in the initial batch, not a later sampling queue"
    );
    assert!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .is_empty()
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        session.abort_all_tasks(TurnAbortReason::Interrupted),
    )
    .await
    .expect("installed task terminates");
}

#[tokio::test]
async fn taskless_placeholder_cleanup_is_pointer_identity_guarded() {
    let (session, _turn_context, _rx) = make_session_and_context_with_rx().await;
    let first = ActiveTurn::default();
    let first_state = Arc::clone(&first.turn_state);
    *session.active_turn.lock().await = Some(first);
    let pending_item = TurnInput::ResponseItem(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "preserve placeholder input".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            first_state.as_ref(),
            std::slice::from_ref(&pending_item),
        )
        .await
        .expect("pending input should fit");

    session.clear_taskless_placeholder(&first_state).await;
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        vec![pending_item]
    );
    assert!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await
            .is_empty()
    );

    let replacement = ActiveTurn::default();
    let replacement_state = Arc::clone(&replacement.turn_state);
    *session.active_turn.lock().await = Some(replacement);
    session.clear_taskless_placeholder(&first_state).await;

    let active = session.active_turn.lock().await;
    assert!(
        active.as_ref().is_some_and(|active_turn| {
            Arc::ptr_eq(&active_turn.turn_state, &replacement_state)
        })
    );
}

struct CaptureStartupInput {
    input: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Vec<TurnInput>>>>,
}

impl SessionTask for CaptureStartupInput {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }
    fn span_name(&self) -> &'static str {
        "session_task.capture_startup_input"
    }
    fn run(
        self: Arc<Self>,
        _session: Arc<crate::session::session::Session>,
        _turn: Arc<TurnContext>,
        input: Vec<TurnInput>,
        cancellation: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            if let Some(sent) = self.input.lock().unwrap().take() {
                let _ = sent.send(input);
            }
            cancellation.cancelled().await;
            Ok(super::TurnTaskResult::default())
        })
    }
}

#[tokio::test]
async fn recovered_input_precedes_nonempty_explicit_prompt_before_later_steering() {
    let (session, turn, _events) = make_session_and_context_with_rx().await;
    let user = |text: &str, id: &str| TurnInput::UserInput {
        content: vec![codex_protocol::user_input::UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        client_id: Some(id.to_string()),
    };
    let older = user("Use red.", "older-A");
    let newer = user("Correction: use blue; red is obsolete.", "newer-B");
    session
        .input_queue
        .restore_transferred_startup_input(vec![older.clone()])
        .await;
    let (sent, received) = tokio::sync::oneshot::channel();
    session
        .spawn_task(
            turn.clone(),
            vec![newer.clone()],
            CaptureStartupInput {
                input: std::sync::Mutex::new(Some(sent)),
            },
        )
        .await;
    let initial = received.await.unwrap();
    assert_eq!(
        initial,
        vec![older, newer],
        "the first task input must be A then B, not B followed by a later A"
    );
    let later = user("Use green instead.", "later-C");
    let state = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .unwrap()
        .turn_state
        .clone();
    session
        .input_queue
        .extend_pending_input_for_turn_state(&state, std::slice::from_ref(&later))
        .await
        .unwrap();
    assert_eq!(
        session
            .input_queue
            .get_pending_input(&session.active_turn)
            .await,
        vec![later]
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test(start_paused = true)]
async fn persisted_missing_output_repairs_tool_timing_and_publishes_terminal() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    let accepted = Arc::new(tokio::sync::Notify::new());
    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            IncompleteClosureTask {
                accepted: Arc::clone(&accepted),
            },
        )
        .await;
    let coordinator = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.as_ref().cloned())
        .expect("terminal coordinator is installed before the worker starts");

    accepted.notified().await;
    tokio::time::timeout(
        Duration::from_secs(10),
        coordinator.wait_cleanup_completed(),
    )
    .await
    .expect("repaired terminal publication must finish cleanup");

    assert!(session.active_turn.lock().await.is_none());
    assert!(!session.terminal_interaction_pending.load(Ordering::Acquire));
    assert!(coordinator.interaction_released());
    let closure = turn_context.turn_timing_state.tool_closure_snapshot();
    assert_eq!(closure.accepted_count, 1);
    assert_eq!(closure.timing_paired_count, 1);
    assert_eq!(closure.terminal_count, 1);
    assert_eq!(closure.persisted_count, 1);
    assert!(closure.complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_terminal_rollout_flush_releases_fence_without_persistence_attestation() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    attach_thread_persistence(
        Arc::get_mut(&mut session).expect("test session should be uniquely owned"),
    )
    .await;
    let queued = Arc::new(tokio::sync::Notify::new());
    let release = CancellationToken::new();
    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            QueuedToolResultTask {
                queued: Arc::clone(&queued),
                release: release.clone(),
            },
        )
        .await;
    let coordinator = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.as_ref().cloned())
        .expect("terminal coordinator is installed before the worker starts");

    queued.notified().await;
    assert_eq!(
        turn_context
            .turn_timing_state
            .tool_closure_snapshot()
            .persisted_count,
        0
    );
    session
        .live_thread()
        .expect("test session should have live persistence")
        .shutdown()
        .await
        .expect("test thread store should shut down");
    release.cancel();

    tokio::time::timeout(
        Duration::from_secs(10),
        coordinator.wait_cleanup_completed(),
    )
    .await
    .expect("flush failure must not leave terminal cleanup active");

    let closure = turn_context.turn_timing_state.tool_closure_snapshot();
    assert_eq!(closure.accepted_count, 1);
    assert_eq!(closure.timing_paired_count, 1);
    assert_eq!(closure.terminal_count, 1);
    assert_eq!(closure.persisted_count, 0);
    assert_eq!(closure.unresolved_calls.len(), 1);
    assert!(!closure.complete);
    assert!(session.active_turn.lock().await.is_none());
    assert!(!session.terminal_interaction_pending.load(Ordering::Acquire));
    assert!(coordinator.interaction_released());

    // The turn completed but its record did not: the client must learn that
    // before the terminal event instead of assuming crash-safe resumption.
    let mut unsaved_warnings = 0;
    let mut terminal_seen = false;
    while let Ok(event) = events.try_recv() {
        match event.msg {
            EventMsg::Warning(warning) if warning.message.contains("not durably saved") => {
                assert!(
                    !terminal_seen,
                    "unsaved-record warning must precede the terminal event"
                );
                assert!(
                    warning.message.contains("conversation history"),
                    "{}",
                    warning.message
                );
                unsaved_warnings += 1;
            }
            EventMsg::TurnComplete(completed) if completed.turn_id == turn_context.sub_id => {
                assert!(completed.error.is_none(), "execution itself succeeded");
                terminal_seen = true;
            }
            _ => {}
        }
    }
    assert!(terminal_seen);
    assert_eq!(unsaved_warnings, 1, "one unsaved record is reported once");
}

#[tokio::test]
async fn panicked_worker_emits_error_before_internal_error_abort() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    session
        .start_task(Arc::clone(&turn_context), Vec::new(), PanickingTask)
        .await;

    let mut worker_error_seen = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("worker failure event channel remains open");
            match event.msg {
                EventMsg::Error(error) if error.message.contains("turn worker panicked") => {
                    worker_error_seen = true;
                }
                EventMsg::TurnAborted(aborted)
                    if aborted.turn_id.as_deref() == Some(turn_context.sub_id.as_str()) =>
                {
                    assert!(
                        worker_error_seen,
                        "worker error must precede terminal abort"
                    );
                    assert_eq!(aborted.reason, TurnAbortReason::InternalError);
                    break;
                }
                EventMsg::TurnComplete(completed) if completed.turn_id == turn_context.sub_id => {
                    panic!("panicked worker must not publish TurnComplete");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("panicked worker must publish a bounded terminal outcome");
}

#[tokio::test]
async fn review_startup_failure_preserves_terminal_error_and_closes_review_mode() {
    let (mut session, turn_context, events) = make_session_and_context_with_rx().await;
    let models_manager = Arc::clone(&session.services.models_manager);
    Arc::get_mut(&mut session)
        .expect("session has not started any tasks")
        .services
        .models_manager = Arc::new(ReviewStartupFailingModelsManager {
        inner: models_manager,
    });
    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            super::review::ReviewTask::new(codex_protocol::items::EnteredReviewModeItem {
                id: "review-startup-failure".to_string(),
                target: codex_protocol::protocol::ReviewTarget::UncommittedChanges,
                user_facing_hint: "review current changes".to_string(),
            }),
        )
        .await;

    let mut entered_review = false;
    let mut exited_review = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("review event channel remains open");
            match event.msg {
                EventMsg::ItemCompleted(completed) => match completed.item {
                    codex_protocol::items::TurnItem::EnteredReviewMode(_) => {
                        entered_review = true;
                    }
                    codex_protocol::items::TurnItem::ExitedReviewMode(exited) => {
                        assert!(entered_review, "review must enter before exiting");
                        assert!(!exited_review, "review must exit only once");
                        assert!(exited.review_output.is_none());
                        exited_review = true;
                    }
                    _ => {}
                },
                EventMsg::TurnComplete(completed) if completed.turn_id == turn_context.sub_id => {
                    assert!(entered_review);
                    assert!(
                        exited_review,
                        "failed review must close before its terminal event"
                    );
                    assert_eq!(
                        completed
                            .error
                            .expect("startup failure must fail the turn")
                            .message,
                        "Fatal error: injected review model startup failure"
                    );
                    break;
                }
                EventMsg::TurnAborted(aborted)
                    if aborted.turn_id.as_deref() == Some(turn_context.sub_id.as_str()) =>
                {
                    panic!("startup failure must preserve its error in TurnComplete");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("failed review must publish a bounded terminal outcome");
}

#[tokio::test]
async fn fatal_turn_preserves_pending_input_without_automatic_restart() {
    assert_failed_turn_preserves_pending_input(FailureMode::Fatal).await;
}

#[tokio::test]
async fn generic_error_preserves_pending_input_without_automatic_restart() {
    assert_failed_turn_preserves_pending_input(FailureMode::ReturnedError).await;
}

#[tokio::test]
async fn emitted_error_preserves_pending_input_without_automatic_restart() {
    assert_failed_turn_preserves_pending_input(FailureMode::EmittedError).await;
}

struct CommentaryThenErrorTask;

impl SessionTask for CommentaryThenErrorTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.commentary_then_error"
    }

    fn run(
        self: Arc<Self>,
        session: Arc<crate::session::session::Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
        Box::pin(async move {
            session
                .record_conversation_items(
                    &ctx,
                    &[ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: "Findings so far: the cache breaks at the first notice."
                                .to_string(),
                        }],
                        phase: Some(codex_protocol::models::MessagePhase::Commentary),
                        internal_chat_message_metadata_passthrough: None,
                    }],
                )
                .await
                .expect("record commentary");
            Err(codex_protocol::error::CodexErr::InvalidRequest(
                "the final request failed".to_string(),
            ))
        })
    }
}

#[tokio::test]
async fn failed_turn_reports_its_last_commentary_and_the_error() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            CommentaryThenErrorTask,
        )
        .await;
    let done = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventMsg::TurnComplete(done) = events.recv().await.expect("event").msg {
                break done;
            }
        }
    })
    .await
    .expect("the failed turn completes");
    let error =
        codex_protocol::error::CodexErr::InvalidRequest("the final request failed".to_string())
            .to_error_event(None)
            .message;
    assert_eq!(done.error.map(|error| error.message), Some(error.clone()));
    assert_eq!(
        done.last_agent_message,
        Some(format!(
            "Findings so far: the cache breaks at the first notice.\n\n(This turn ended on an error before its final answer: {}. Send a message to resume.)",
            error.trim_end_matches('.')
        ))
    );
}

#[test]
fn turn_error_fallback_uses_only_this_turns_messages() {
    let message = |text: &str, turn_id: &str| {
        let mut item = ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            phase: Some(codex_protocol::models::MessagePhase::FinalAnswer),
            internal_chat_message_metadata_passthrough: None,
        };
        item.set_turn_id_if_missing(turn_id);
        item
    };
    let history = [message("earlier answer", "earlier-turn")];
    assert_eq!(
        super::last_message_before_turn_error(&history, "failed-turn", false, "boom"),
        None,
        "another turn's answer is not this turn's result"
    );
    let history = [
        message("earlier answer", "earlier-turn"),
        message("partial result", "failed-turn"),
    ];
    assert_eq!(
        super::last_message_before_turn_error(&history, "failed-turn", false, "boom."),
        Some("partial result\n\n(This turn ended on an error before its final answer: boom. Send a message to resume.)".to_string())
    );
}

#[tokio::test]
async fn panicked_worker_preserves_pending_input_without_automatic_restart() {
    assert_failed_turn_preserves_pending_input(FailureMode::Panicked).await;
}

async fn assert_failed_turn_preserves_pending_input(mode: FailureMode) {
    for fail_safe in [false, true] {
        let (session, turn_context, events) = make_session_and_context_with_rx().await;
        let release = CancellationToken::new();
        session
            .start_task(
                Arc::clone(&turn_context),
                Vec::new(),
                FailureOnReleaseTask(release.clone(), mode),
            )
            .await;
        let (turn_state, terminal) = {
            let active = session.active_turn.lock().await;
            let active = active.as_ref().expect("fatal task is waiting");
            (
                Arc::clone(&active.turn_state),
                active.terminal.clone().unwrap(),
            )
        };
        let pending = vec![
            TurnInput::InterAgentCommunication(
                codex_protocol::protocol::InterAgentCommunication::new(
                    codex_protocol::AgentPath::root(),
                    codex_protocol::AgentPath::try_from("/root/worker").unwrap(),
                    Vec::new(),
                    "retain this follow-up".to_string(),
                    true,
                ),
            ),
            TurnInput::UserInput {
                content: vec![codex_protocol::user_input::UserInput::Text {
                    text: "retain steering too".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
            },
        ];
        session
            .input_queue
            .extend_pending_input_for_turn_state(turn_state.as_ref(), &pending)
            .await
            .unwrap();
        if fail_safe {
            terminal.request_panic_after_terminal_publication();
        }
        release.cancel();
        tokio::time::timeout(Duration::from_secs(5), terminal.wait_cleanup_completed())
            .await
            .expect("fatal cleanup must settle without retrying input");

        assert!(
            session.active_turn.lock().await.is_none(),
            "fatal input was retried"
        );
        assert!(!session.terminal_interaction_pending.load(Ordering::Acquire));
        assert!(session.input_queue.has_pending_turn_start_work().await);
        let mut completed = 0;
        while let Ok(event) = events.try_recv() {
            match event.msg {
                EventMsg::TurnStarted(_) => panic!("unexpected automatic successor"),
                EventMsg::TurnComplete(done) => {
                    assert_eq!(done.turn_id, turn_context.sub_id);
                    assert!(
                        done.error
                            .unwrap()
                            .message
                            .contains("explicit new admission")
                    );
                    completed += 1;
                }
                EventMsg::TurnAborted(done) if matches!(mode, FailureMode::Panicked) => {
                    assert_eq!(done.turn_id.as_deref(), Some(turn_context.sub_id.as_str()));
                    assert_eq!(done.reason, TurnAbortReason::InternalError);
                    completed += 1;
                }
                _ => {}
            }
        }
        assert_eq!(completed, 1, "publish the original failure exactly once");

        // This is not a session-wide latch: explicit admission still receives
        // both recovered items in order, once, without discarding steering.
        let next = session
            .new_default_turn_with_sub_id("explicit-follow-up".into())
            .await;
        let (sent, received) = tokio::sync::oneshot::channel();
        session
            .start_task(
                next,
                Vec::new(),
                CaptureStartupInput {
                    input: std::sync::Mutex::new(Some(sent)),
                },
            )
            .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), received)
                .await
                .expect("explicit admission starts its task")
                .expect("startup input is captured"),
            pending,
            "startup now owns recovered input in the initial batch, not a later sampling queue"
        );
        assert!(
            session
                .input_queue
                .get_pending_input(&session.active_turn)
                .await
                .is_empty()
        );
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
    }
}

#[test]
fn terminal_failures_suppress_pending_input_restart() {
    use codex_protocol::error::CodexErr;
    let completed = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn".into(),
        last_agent_message: None,
        surfaced_result: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
        timing: None,
    });
    let mut failed = completed.clone();
    if let EventMsg::TurnComplete(event) = &mut failed {
        event.error = Some(CodexErr::Fatal("failed compaction".into()).to_error_event(None));
    }
    for outcome in [
        TurnTerminalOutcome::ReturnedError(CodexErr::Fatal("stop".into())),
        TurnTerminalOutcome::ReturnedError(CodexErr::InternalServerError { retry_after: None }),
        TurnTerminalOutcome::WorkerJoinFailed(super::WorkerJoinFailure::Panicked),
        TurnTerminalOutcome::WorkerJoinFailed(super::WorkerJoinFailure::Cancelled),
        TurnTerminalOutcome::Aborted(TurnAbortReason::InternalError),
    ] {
        assert!(!outcome.allows_pending_input_restart(Some(&completed)));
        assert!(!outcome.allows_pending_input_restart(Some(&failed)));
    }
    for outcome in [
        TurnTerminalOutcome::Completed {
            result: Default::default(),
        },
        TurnTerminalOutcome::Aborted(TurnAbortReason::Interrupted),
        TurnTerminalOutcome::ReturnedError(CodexErr::TurnAborted),
    ] {
        assert!(outcome.allows_pending_input_restart(Some(&completed)));
        assert!(!outcome.allows_pending_input_restart(Some(&failed)));
        assert!(!outcome.allows_pending_input_restart(None));
    }
}

#[tokio::test]
async fn concurrent_start_task_calls_install_exactly_one_worker() {
    let (session, first_context, _events) = make_session_and_context_with_rx().await;
    let second_context = session
        .new_default_turn_with_sub_id(format!("{}-second", first_context.sub_id))
        .await;
    let started = Arc::new(AtomicUsize::new(0));
    let release = CancellationToken::new();

    tokio::join!(
        session.start_task(
            Arc::clone(&first_context),
            Vec::new(),
            CountingBlockingTask {
                started: Arc::clone(&started),
                release: release.clone(),
            },
        ),
        session.start_task(
            Arc::clone(&second_context),
            Vec::new(),
            CountingBlockingTask {
                started: Arc::clone(&started),
                release: release.clone(),
            },
        ),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while started.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the installed worker starts");
    tokio::task::yield_now().await;

    assert_eq!(started.load(Ordering::Acquire), 1);
    let installed_turn_id = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.task.as_ref())
        .map(|task| task.turn_context.sub_id.clone())
        .expect("one running task remains installed");
    assert!(
        installed_turn_id == first_context.sub_id || installed_turn_id == second_context.sub_id
    );

    tokio::time::timeout(
        Duration::from_secs(5),
        session.abort_all_tasks(TurnAbortReason::Interrupted),
    )
    .await
    .expect("installed worker terminates");
    release.cancel();
}

#[tokio::test]
async fn shutdown_abort_retains_checkpoint_and_plan_without_claiming_user_interrupt() {
    for shutdown in [false, true] {
        let (session, mut turn, _events) = make_session_and_context_with_rx().await;
        Arc::make_mut(
            &mut Arc::get_mut(&mut turn)
                .expect("unshared turn fixture")
                .config,
        )
        .agent_interrupt_message_enabled = true;
        let checkpoint = ResponseItem::Message {
            id: None,
            role: "developer".into(),
            content: vec![ContentItem::InputText {
                text: "<completed_phase_checkpoint>keep this evidence</completed_phase_checkpoint>"
                    .into(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        session
            .record_conversation_items(&turn, &[checkpoint])
            .await
            .unwrap();
        session
            .services
            .plan_store
            .update(codex_protocol::plan_tool::UpdatePlanArgs {
                explanation: None,
                plan: vec![codex_protocol::plan_tool::PlanItemArg {
                    step: "remaining validation".into(),
                    status: codex_protocol::plan_tool::StepStatus::InProgress,
                }],
            })
            .await;
        session
            .start_task(Arc::clone(&turn), Vec::new(), FenceBlockingTask)
            .await;
        if shutdown {
            session.begin_shutdown().await;
        }
        session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        let history = session.clone_history().await;
        let marker = history
            .raw_items()
            .iter()
            .rev()
            .find_map(|item| match item {
                ResponseItem::Message { content, .. } => {
                    content.iter().find_map(|part| match part {
                        ContentItem::InputText { text } if text.contains("<turn_aborted>") => {
                            Some(text)
                        }
                        _ => None,
                    })
                }
                _ => None,
            })
            .expect("abort marker");
        if shutdown {
            assert!(marker.contains("session was shutting down"));
            assert!(!marker.contains("interrupted the previous turn on purpose"));
            assert!(marker.contains("keep this evidence"));
            assert!(marker.contains("remaining validation"));
        } else {
            assert!(marker.contains("interrupted"));
            assert!(!marker.contains("session was shutting down"));
        }
    }
}

#[tokio::test]
async fn shutdown_latch_is_linearized_with_task_start_admission() {
    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    let task_start_guard = session
        .task_start_gate
        .acquire()
        .await
        .unwrap_or_else(|_| unreachable!("session-owned task-start semaphore is never closed"));
    let shutdown_session = Arc::clone(&session);
    let shutdown = tokio::spawn(async move {
        shutdown_session.begin_shutdown().await;
    });
    tokio::task::yield_now().await;

    // `begin_shutdown` publishes the fence before observing an admitted
    // transaction's permit, then still waits for that permit to settle.
    assert!(session.shutting_down.load(Ordering::Acquire));
    assert!(
        !shutdown.is_finished(),
        "shutdown waits for the admitted start to release its permit"
    );
    drop(task_start_guard);
    shutdown.await.expect("shutdown latch task joins");
    assert!(session.shutting_down.load(Ordering::Acquire));

    let started = Arc::new(AtomicUsize::new(0));
    session
        .start_task(
            turn_context,
            Vec::new(),
            CountingBlockingTask {
                started: Arc::clone(&started),
                release: CancellationToken::new(),
            },
        )
        .await;
    tokio::task::yield_now().await;

    assert_eq!(started.load(Ordering::Acquire), 0);
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn terminalization_aborts_and_joins_turn_auxiliary_tasks() {
    struct Dropped(Arc<AtomicBool>);

    struct UnresponsiveTask;

    impl SessionTask for UnresponsiveTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "session_task.unresponsive"
        }

        fn run(
            self: Arc<Self>,
            _session: Arc<crate::session::session::Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<TurnInput>,
            _cancellation_token: CancellationToken,
        ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
            Box::pin(std::future::pending())
        }
    }

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    session
        .start_task(Arc::clone(&turn_context), Vec::new(), UnresponsiveTask)
        .await;
    let auxiliary_started = Arc::new(tokio::sync::Notify::new());
    let auxiliary_dropped = Arc::new(AtomicBool::new(false));
    let started = Arc::clone(&auxiliary_started);
    let dropped = Arc::clone(&auxiliary_dropped);
    assert!(
        session
            .spawn_active_turn_auxiliary(move |_turn_context, _cancellation_token| async move {
                let _dropped = Dropped(dropped);
                started.notify_one();
                std::future::pending::<()>().await;
            })
            .await
    );
    auxiliary_started.notified().await;

    let started = tokio::time::Instant::now();
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(
        started.elapsed() < super::GRACEFUL_INTERRUPTION_TIMEOUT + Duration::from_secs(3),
        "worker and auxiliary cancellation must share one grace period: {:?}",
        started.elapsed(),
    );
    assert!(
        auxiliary_dropped.load(Ordering::Acquire),
        "terminal cleanup must join even an auxiliary task that ignores cancellation"
    );
    assert!(session.active_turn.lock().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn terminalization_publishes_abort_while_worker_is_synchronously_blocked() {
    struct BlockingTask {
        started: Arc<tokio::sync::Notify>,
        release: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl SessionTask for BlockingTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "session_task.blocked_worker"
        }

        fn run(
            self: Arc<Self>,
            _session: Arc<crate::session::session::Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<TurnInput>,
            _cancellation_token: CancellationToken,
        ) -> futures::future::BoxFuture<'static, SessionTaskResult> {
            Box::pin(async move {
                self.started.notify_one();
                let (released, wake) = self.release.as_ref();
                let mut released = released.lock().expect("release lock");
                while !*released {
                    released = wake.wait(released).expect("release lock");
                }
                Ok(super::TurnTaskResult::default())
            })
        }
    }

    // Always release the worker, including when an assertion or deadline fails,
    // so a regression does not hang the test runtime's own shutdown.
    struct ReleaseWorker(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
    impl Drop for ReleaseWorker {
        fn drop(&mut self) {
            let (released, wake) = self.0.as_ref();
            *released.lock().expect("release lock") = true;
            wake.notify_all();
        }
    }

    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = ReleaseWorker(Arc::new((
        std::sync::Mutex::new(false),
        std::sync::Condvar::new(),
    )));
    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            BlockingTask {
                started: Arc::clone(&started),
                release: Arc::clone(&release.0),
            },
        )
        .await;
    tokio::time::timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("worker enters its synchronous section");
    let worker_done = Arc::clone(
        &session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            .worker_done,
    );

    tokio::time::timeout(
        super::GRACEFUL_INTERRUPTION_TIMEOUT
            + super::FORCED_INTERRUPTION_TIMEOUT
            + Duration::from_secs(5),
        session.abort_all_tasks(TurnAbortReason::Interrupted),
    )
    .await
    .expect("abort must finish without waiting for the blocked worker");

    assert!(!worker_done.load(Ordering::Acquire));
    assert!(session.active_turn.lock().await.is_none());
    let mut late_tool_accepted = false;
    assert!(!turn_context.tool_call_acceptance.try_accept(|| {
        late_tool_accepted = true;
        true
    }));
    assert!(!late_tool_accepted);
    // Interaction release follows terminal acceptance, even while the worker
    // remains blocked by ReleaseWorker.
    let aborted = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventMsg::TurnAborted(aborted) = events.recv().await.unwrap().msg {
                break aborted;
            }
        }
    })
    .await
    .expect("terminal event must not wait for the blocked worker");
    assert!(!worker_done.load(Ordering::Acquire));
    let mut terminal = vec![aborted];
    while let Ok(event) = events.try_recv() {
        if let EventMsg::TurnAborted(aborted) = event.msg {
            terminal.push(aborted);
        }
    }
    assert_eq!(terminal.len(), 1);
    assert_eq!(
        terminal[0].turn_id.as_deref(),
        Some(turn_context.sub_id.as_str())
    );
    assert_eq!(terminal[0].reason, TurnAbortReason::Interrupted);
}

#[tokio::test]
async fn terminal_event_is_delivered_before_optional_command_cache_persistence() {
    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    let (mut cache_persist_started, release_cache_persist) = session
        .services
        .command_execution
        .block_next_cache_persist_for_test();

    session
        .start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            ImmediateCompletingTask,
        )
        .await;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    let event = event.expect("terminal event channel remains open");
                    if let EventMsg::TurnComplete(completed) = event.msg
                        && completed.turn_id == turn_context.sub_id
                    {
                        break;
                    }
                }
                entered = &mut cache_persist_started => {
                    entered.expect("cache persistence gate remains installed");
                    panic!("optional cache persistence started before TurnComplete");
                }
            }
        }
    })
    .await
    .expect("TurnComplete is not blocked by optional cache persistence");

    cache_persist_started
        .await
        .expect("cache persistence starts after TurnComplete");
    release_cache_persist
        .send(())
        .expect("terminal finalizer still awaits cache persistence");
}

#[tokio::test(start_paused = true)]
async fn terminal_schedule_waits_for_active_turn_admission() {
    const FENCE_WAIT: Duration = Duration::from_millis(25);

    let (session, turn_context, events) = make_session_and_context_with_rx().await;
    session
        .start_task(Arc::clone(&turn_context), Vec::new(), FenceBlockingTask)
        .await;

    let (active_turn_locked_tx, active_turn_locked_rx) = tokio::sync::oneshot::channel();
    let (release_active_turn_tx, release_active_turn_rx) = std::sync::mpsc::channel();
    let active_turn_blocker = tokio::task::spawn_blocking({
        let session = Arc::clone(&session);
        move || {
            let active_turn = session.active_turn.blocking_lock();
            assert!(
                active_turn
                    .as_ref()
                    .is_some_and(|active_turn| active_turn.task.is_some())
            );
            let _ = active_turn_locked_tx.send(());
            let _ = release_active_turn_rx.recv();
            drop(active_turn);
        }
    });
    active_turn_locked_rx
        .await
        .expect("active-turn blocker acquires lock");
    let schedule = tokio::spawn({
        let session = Arc::clone(&session);
        let turn_id = turn_context.sub_id.clone();
        async move {
            session
                .schedule_turn_terminal(
                    Some(turn_id.as_str()),
                    TurnTerminalOutcome::Aborted(TurnAbortReason::Interrupted),
                )
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(!schedule.is_finished());
    tokio::time::advance(FENCE_WAIT).await;
    release_active_turn_tx
        .send(())
        .expect("release active-turn blocker");
    active_turn_blocker
        .await
        .expect("active-turn blocker joins");

    assert!(matches!(
        schedule.await.expect("terminal schedule task joins"),
        TerminalSchedule::Started(_)
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events
                .recv()
                .await
                .expect("terminal event channel remains open");
            if let EventMsg::TurnAborted(event) = event.msg
                && event.turn_id.as_deref() == Some(turn_context.sub_id.as_str())
            {
                break;
            }
        }
    })
    .await
    .expect("terminal event is emitted");
}

#[tokio::test]
async fn interrupt_pending_fences_sampling_without_terminalizing() {
    let coordinator = TurnTerminalCoordinator::new("turn-fenced".to_string());
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
    assert!(coordinator.mark_interrupt_pending().await);
    let generation = coordinator.wake_generation_id();
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Fenced);
    // The fence is pre-terminal: the normal coordinator can still claim the
    // eventual TurnAborted transition after output persistence.
    assert!(coordinator.try_claim().is_none());
    assert!(coordinator.interrupt_pending());
    assert_eq!(
        coordinator.mark_interrupt_output_durable(&generation),
        TerminalWakeResult::Applied
    );
    let permit = coordinator
        .try_claim()
        .expect("terminal claim opens after durability");
    permit.complete_cleanup();
    assert_eq!(
        coordinator.wait_for_interrupt_resolution(&generation).await,
        TerminalWakeResult::Applied
    );
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
}

#[tokio::test]
async fn orchestration_audit_interrupt_durability_resolution_is_a_first_writer_wins_state_transition()
 {
    let coordinator = TurnTerminalCoordinator::new("turn-interrupt-resolution".to_string());
    assert!(coordinator.mark_interrupt_pending().await);
    let generation = coordinator.wake_generation_id();

    assert_eq!(
        coordinator.mark_interrupt_output_durable(&generation),
        TerminalWakeResult::Applied
    );
    assert_eq!(
        coordinator.mark_interrupt_persistence_failed(&generation),
        TerminalWakeResult::Applied
    );
    assert!(
        !coordinator.interrupt_persistence_failed(),
        "a late failure report must not replace an established durability result"
    );
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Fenced);

    coordinator
        .try_claim()
        .expect("durability resolution opens terminal admission")
        .complete_cleanup();
    assert_eq!(coordinator.sampling_admission(), SamplingAdmission::Allowed);
}

#[test]
fn terminal_claim_and_interrupt_fence_have_one_atomic_winner() {
    const ROUNDS: usize = 10_000;

    let coordinators = Arc::new(
        (0..ROUNDS)
            .map(|index| TurnTerminalCoordinator::new(format!("race-{index}")))
            .collect::<Vec<_>>(),
    );
    let claim_wins = Arc::new(
        (0..ROUNDS)
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let interrupt_wins = Arc::new(
        (0..ROUNDS)
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>(),
    );
    let start = Arc::new(Barrier::new(3));
    let finish = Arc::new(Barrier::new(3));

    let claim_thread = std::thread::spawn({
        let coordinators = Arc::clone(&coordinators);
        let claim_wins = Arc::clone(&claim_wins);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        move || {
            for index in 0..ROUNDS {
                start.wait();
                let permit = coordinators[index].try_claim();
                claim_wins[index].store(permit.is_some(), Ordering::Release);
                finish.wait();
                // An abandoned permit intentionally releases its claim. Keep the
                // winning claim committed while the main thread observes it.
                if let Some(permit) = permit {
                    permit.complete_cleanup();
                }
            }
        }
    });
    let interrupt_thread = std::thread::spawn({
        let coordinators = Arc::clone(&coordinators);
        let interrupt_wins = Arc::clone(&interrupt_wins);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build interrupt race runtime");
            for index in 0..ROUNDS {
                start.wait();
                let won = runtime.block_on(coordinators[index].mark_interrupt_pending());
                interrupt_wins[index].store(won, Ordering::Release);
                finish.wait();
            }
        }
    });

    for index in 0..ROUNDS {
        start.wait();
        finish.wait();
        let claimed = claim_wins[index].load(Ordering::Acquire);
        let interrupted = interrupt_wins[index].load(Ordering::Acquire);
        assert_ne!(
            claimed, interrupted,
            "round {index} must have exactly one terminal-decision winner"
        );
        assert_eq!(coordinators[index].interrupt_pending(), interrupted);
    }

    claim_thread.join().expect("terminal claim thread joins");
    interrupt_thread
        .join()
        .expect("interrupt fence thread joins");
}

#[tokio::test]
async fn terminal_wakeups_are_generation_aware_and_waiter_counts_are_cancellation_safe() {
    let coordinator = TurnTerminalCoordinator::new("turn-generation".to_string());
    let stale_generation = coordinator.wake_generation_id();
    assert!(coordinator.mark_interrupt_pending().await);
    assert_eq!(
        coordinator
            .wait_for_interrupt_resolution(&stale_generation)
            .await,
        TerminalWakeResult::Stale
    );
    assert_eq!(
        coordinator.mark_interrupt_output_durable(&stale_generation),
        TerminalWakeResult::Stale
    );
    assert!(coordinator.try_claim().is_none());

    let generation = coordinator.wake_generation_id();
    let waiter = {
        let coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move { coordinator.wait_for_interrupt_resolution(&generation).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(coordinator.waiter_snapshot().interrupt_resolution, 1);
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(coordinator.waiter_snapshot().interrupt_resolution, 0);
}

#[test]
fn terminal_analytics_claim_converges_across_in_process_recovery() {
    let before_claim = TurnTerminalCoordinator::new("turn-before-claim".to_string());
    drop(before_claim.try_claim().expect("normal terminal claimant"));
    let recovery = before_claim
        .try_claim()
        .expect("fail-safe terminal claimant");
    assert!(recovery.try_claim_analytics_emission());

    let after_claim = TurnTerminalCoordinator::new("turn-after-claim".to_string());
    let normal = after_claim.try_claim().expect("normal terminal claimant");
    assert!(normal.try_claim_analytics_emission());
    assert!(!normal.try_claim_analytics_emission());
    drop(normal);
    let recovery = after_claim
        .try_claim()
        .expect("fail-safe terminal claimant");
    assert!(!recovery.try_claim_analytics_emission());
}

#[test]
fn emit_turn_network_proxy_metric_records_active_and_inactive_turns() {
    for (active, expected) in [(true, "true"), (false, "false")] {
        let session_telemetry = test_session_telemetry();
        emit_turn_network_proxy_metric(&session_telemetry, active);
        let snapshot = session_telemetry
            .snapshot_metrics()
            .expect("runtime metrics snapshot");
        let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

        assert_eq!(value, 1);
        assert_eq!(
            attrs,
            BTreeMap::from([("active".to_string(), expected.to_string())])
        );
    }
}

#[test]
fn emit_compact_metric_records_manual_and_automatic_compaction() {
    for (compact_type, manual, expected_manual) in
        [("remote_v2", true, "true"), ("local", false, "false")]
    {
        let session_telemetry = test_session_telemetry();
        emit_compact_metric(&session_telemetry, compact_type, manual);
        let snapshot = session_telemetry
            .snapshot_metrics()
            .expect("runtime metrics snapshot");
        let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

        assert_eq!(value, 1);
        assert_eq!(
            attrs,
            BTreeMap::from([
                ("manual".to_string(), expected_manual.to_string()),
                ("type".to_string(), compact_type.to_string()),
            ])
        );
    }
}
#[tokio::test]
async fn replacement_waits_for_predecessor_terminal_acceptance() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), FenceBlockingTask)
        .await;
    let terminal = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .unwrap()
        .terminal
        .clone()
        .unwrap();
    let (entered, release) = terminal.pause_publication_for_test();
    let (_, mut successor) = crate::session::tests::make_session_and_context().await;
    successor.sub_id = "replacement".into();
    let started = Arc::new(AtomicUsize::new(0));
    let owner = Arc::clone(&session);
    let count = Arc::clone(&started);
    let replacement = tokio::spawn(async move {
        owner
            .spawn_task(
                Arc::new(successor),
                Vec::new(),
                CountingBlockingTask {
                    started: count,
                    release: CancellationToken::new(),
                },
            )
            .await;
    });
    tokio::time::timeout(Duration::from_secs(5), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(!terminal.interaction_released());
    assert!(session.active_turn.lock().await.is_some());
    assert_eq!(started.load(Ordering::Acquire), 0);
    assert!(!replacement.is_finished());
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), replacement)
        .await
        .unwrap()
        .unwrap();
    let mut terminals = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventMsg::TurnAborted(aborted) = event.msg {
            terminals.push(aborted);
        }
    }
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].turn_id.as_deref(), Some(turn.sub_id.as_str()));
    assert_eq!(
        session
            .active_turn
            .lock()
            .await
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            .turn_context
            .sub_id,
        "replacement"
    );
    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}
