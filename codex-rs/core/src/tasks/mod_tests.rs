use super::SessionTask;
use super::SessionTaskResult;
use super::TASK_COMPACT_METRIC;
use super::TerminalSchedule;
use super::TurnTerminalOutcome;
use super::emit_compact_metric;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use crate::session::TurnInput;
use crate::session::tests::attach_thread_persistence;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::turn_context::TurnContext;
use crate::state::ActiveTurn;
use crate::state::SamplingAdmission;
use crate::state::TaskKind;
use crate::state::TerminalWakeResult;
use crate::state::TurnTerminalCoordinator;
use crate::tools::tool_dispatch_trace::ToolDispatchTimingSnapshot;
use crate::turn_timing::ToolCallTimingLineage;
use codex_features::Feature;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_otel::TURN_TOKEN_USAGE_METRIC;
use codex_otel::TURN_TOOL_CALL_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::ToolExecutionId;
use codex_protocol::protocol::TurnAbortReason;
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
                .await;
            assert!(ctx.tool_call_acceptance.try_accept(|| {
                ctx.turn_timing_state.try_record_accepted_tool_call(
                    "unresolved-call",
                    &ToolExecutionId("unresolved-execution".to_string()),
                    TurnTimingToolCallSource::Direct,
                    None,
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

#[test]
fn session_task_is_the_object_safe_runtime_boundary() {
    let task: Arc<dyn SessionTask> = Arc::new(FenceBlockingTask);
    assert_eq!(task.kind(), TaskKind::Regular);
    assert_eq!(task.span_name(), "session_task.terminal_fence");
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

fn histogram_attribute_maps(
    resource_metrics: &ResourceMetrics,
    name: &str,
) -> Vec<BTreeMap<String, String>> {
    let metric = find_metric(resource_metrics, name);
    match metric.data() {
        AggregatedMetrics::F64(data) => match data {
            MetricData::Histogram(histogram) => histogram
                .data_points()
                .map(|point| attributes_to_map(point.attributes()))
                .collect(),
            _ => panic!("unexpected histogram aggregation"),
        },
        _ => panic!("unexpected histogram data type"),
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
    let (mut session, turn_context, _events) = make_session_and_context_with_rx().await;
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

    assert!(!session.shutting_down.load(Ordering::Acquire));
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

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let (session, turn_context, _events) = make_session_and_context_with_rx().await;
    session
        .start_task(Arc::clone(&turn_context), Vec::new(), FenceBlockingTask)
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

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(
        auxiliary_dropped.load(Ordering::Acquire),
        "terminal cleanup must join even an auxiliary task that ignores cancellation"
    );
    assert!(session.active_turn.lock().await.is_none());
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
                if let Some(_permit) = coordinators[index].try_claim() {
                    claim_wins[index].store(true, Ordering::Release);
                }
                finish.wait();
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
fn terminal_analytics_claim_is_process_local_one_shot() {
    let coordinator = TurnTerminalCoordinator::new("turn-analytics".to_string());
    let permit = coordinator.try_claim().expect("terminal claimant");
    assert!(permit.try_claim_analytics_emission());
    assert!(!permit.try_claim_analytics_emission());
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
    drop(normal);
    let recovery = after_claim
        .try_claim()
        .expect("fail-safe terminal claimant");
    assert!(!recovery.try_claim_analytics_emission());
}

#[test]
fn emit_turn_network_proxy_metric_records_active_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ true,
        ("tmp_mem_enabled", "true"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "true".to_string()),
            ("tmp_mem_enabled".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_network_proxy_metric_records_inactive_turn() {
    let session_telemetry = test_session_telemetry();

    emit_turn_network_proxy_metric(
        &session_telemetry,
        /*network_proxy_active*/ false,
        ("tmp_mem_enabled", "false"),
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("active".to_string(), "false".to_string()),
            ("tmp_mem_enabled".to_string(), "false".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_read_allowed_with_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ true,
        /*has_citations*/ true,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "true".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "true".to_string()),
            ("read_allowed".to_string(), "true".to_string()),
        ])
    );
}

#[test]
fn emit_turn_memory_metric_records_config_disabled_without_citations() {
    let session_telemetry = test_session_telemetry();

    emit_turn_memory_metric(
        &session_telemetry,
        /*feature_enabled*/ true,
        /*config_enabled*/ false,
        /*has_citations*/ false,
    );

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TURN_MEMORY_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("config_use_memories".to_string(), "false".to_string()),
            ("feature_enabled".to_string(), "true".to_string()),
            ("has_citations".to_string(), "false".to_string()),
            ("read_allowed".to_string(), "false".to_string()),
        ])
    );
}

#[tokio::test]
async fn live_runtime_memory_feature_labels_use_refreshed_turn_state() {
    let (mut session, _initial_turn) = make_session_and_context().await;
    assert!(
        !session.enabled(Feature::MemoryTool),
        "the fixture must begin with the session-invariant feature disabled"
    );
    session.services.session_telemetry = test_session_telemetry();
    let mut refreshed_config = session.get_config().await.as_ref().clone();
    refreshed_config
        .features
        .enable(Feature::MemoryTool)
        .expect("memory feature should be enabled in refreshed config");
    session
        .refresh_runtime_config_features(refreshed_config, &[Feature::MemoryTool])
        .await;
    let turn_context = session.new_default_turn().await;
    assert!(turn_context.config.features.enabled(Feature::MemoryTool));
    assert!(!session.enabled(Feature::MemoryTool));

    session
        .emit_post_terminal_metrics(
            &turn_context,
            /*turn_had_memory_citation*/ false,
            /*turn_tool_calls*/ 1,
            &TokenUsage::default(),
        )
        .await;

    let snapshot = session
        .services
        .session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (network_attrs, _) = metric_point(&snapshot, TURN_NETWORK_PROXY_METRIC);
    assert_eq!(
        network_attrs.get("tmp_mem_enabled").map(String::as_str),
        Some("true")
    );
    for metric_name in [TURN_TOOL_CALL_METRIC, TURN_TOKEN_USAGE_METRIC] {
        let attribute_maps = histogram_attribute_maps(&snapshot, metric_name);
        assert!(
            !attribute_maps.is_empty(),
            "{metric_name} should have samples"
        );
        assert!(
            attribute_maps
                .iter()
                .all(|attrs| { attrs.get("tmp_mem_enabled").map(String::as_str) == Some("true") })
        );
    }
    let (memory_attrs, _) = metric_point(&snapshot, TURN_MEMORY_METRIC);
    assert_eq!(
        memory_attrs.get("feature_enabled").map(String::as_str),
        Some("true")
    );
}

#[test]
fn emit_compact_metric_records_manual_remote_v2() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "remote_v2", /*manual*/ true);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "true".to_string()),
            ("type".to_string(), "remote_v2".to_string()),
        ])
    );
}

#[test]
fn emit_compact_metric_records_auto_local() {
    let session_telemetry = test_session_telemetry();

    emit_compact_metric(&session_telemetry, "local", /*manual*/ false);

    let snapshot = session_telemetry
        .snapshot_metrics()
        .expect("runtime metrics snapshot");
    let (attrs, value) = metric_point(&snapshot, TASK_COMPACT_METRIC);

    assert_eq!(value, 1);
    assert_eq!(
        attrs,
        BTreeMap::from([
            ("manual".to_string(), "false".to_string()),
            ("type".to_string(), "local".to_string()),
        ])
    );
}
