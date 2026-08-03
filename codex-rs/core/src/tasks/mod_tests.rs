use super::TASK_COMPACT_METRIC;
use super::WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON;
use super::WorkspaceFinalizationGuard;
use super::emit_compact_metric;
use super::emit_turn_memory_metric;
use super::emit_turn_network_proxy_metric;
use super::merge_completion_review_partial;
use super::seal_passed_completion_for_terminal_dispatch;
use crate::session::tests::make_session_and_context_with_rx;
use crate::state::ActiveTurn;
use codex_agent_task_store::AgentTaskStore;
use codex_agent_task_store::LocalAgentTaskStore;
use codex_otel::MetricsClient;
use codex_otel::MetricsConfig;
use codex_otel::SessionTelemetry;
use codex_otel::TURN_MEMORY_METRIC;
use codex_otel::TURN_NETWORK_PROXY_METRIC;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TaskCompletionGate;
use codex_protocol::protocol::TaskCompletionStatus;
use codex_protocol::protocol::TurnAbortReason;
use codex_state::StateRuntime;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::AggregatedMetrics;
use opentelemetry_sdk::metrics::data::Metric;
use opentelemetry_sdk::metrics::data::MetricData;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::TempDir;

async fn workspace_finalization_guard() -> (
    TempDir,
    TempDir,
    Arc<LocalAgentTaskStore>,
    WorkspaceFinalizationGuard,
) {
    let codex_home = TempDir::new().expect("codex home tempdir");
    let repo = TempDir::new().expect("repository tempdir");
    let state = StateRuntime::init(codex_home.path().to_path_buf(), "test-provider".to_string())
        .await
        .expect("state runtime initializes");
    let store = Arc::new(
        LocalAgentTaskStore::initialize(&state)
            .await
            .expect("task store initializes"),
    );
    let fence = store
        .begin_workspace_finalization(repo.path(), "root-session".to_string())
        .await
        .expect("workspace finalization starts");
    let trait_store: Arc<dyn AgentTaskStore> = store.clone();
    let guard = WorkspaceFinalizationGuard::new(trait_store, repo.path().to_path_buf(), fence);
    (codex_home, repo, store, guard)
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

#[test]
fn completion_review_partial_status_never_overrides_concrete_blockers() {
    for (initial, expected) in [
        (TaskCompletionStatus::Passed, TaskCompletionStatus::Partial),
        (TaskCompletionStatus::Partial, TaskCompletionStatus::Partial),
        (TaskCompletionStatus::Blocked, TaskCompletionStatus::Blocked),
    ] {
        let mut completion = Some(TaskCompletionGate {
            status: initial,
            reasons: vec!["task state".to_string()],
            evidence_path: None,
        });
        merge_completion_review_partial(&mut completion, vec!["review infrastructure".to_string()]);
        let completion = completion.expect("completion");
        assert_eq!(completion.status, expected);
        assert!(
            completion
                .reasons
                .contains(&"review infrastructure".to_string())
        );
    }

    let mut completion = None;
    merge_completion_review_partial(&mut completion, vec!["review infrastructure".to_string()]);
    assert_eq!(
        completion.expect("partial completion").status,
        TaskCompletionStatus::Partial
    );
}

#[tokio::test]
async fn workspace_finalization_guard_seals_and_releases_dispatching_fence() {
    let (_codex_home, repo, store, mut guard) = workspace_finalization_guard().await;
    let original_expiry = guard.fence.as_ref().expect("active fence").expires_at;

    guard
        .seal_for_terminal_dispatch()
        .await
        .expect("fence seals for terminal dispatch");

    let sealed = guard.fence.as_ref().expect("sealed fence").clone();
    assert!(sealed.expires_at >= original_expiry);
    assert!(
        store
            .heartbeat_workspace_finalization(
                repo.path(),
                sealed.fence_id.clone(),
                sealed.root_session_id.clone(),
            )
            .await
            .expect("dispatching fence heartbeat succeeds")
    );
    guard.release().await.expect("dispatching fence releases");
    assert!(
        !store
            .heartbeat_workspace_finalization(repo.path(), sealed.fence_id, sealed.root_session_id,)
            .await
            .expect("released fence heartbeat is rejected")
    );
}

#[tokio::test]
async fn terminal_dispatch_seal_failure_downgrades_passed_but_no_store_does_not() {
    let (_codex_home, repo, store, mut guard) = workspace_finalization_guard().await;
    let stale_fence = guard.fence.as_ref().expect("active fence").clone();
    store
        .release_workspace_finalization(repo.path(), stale_fence.clone())
        .await
        .expect("test invalidates active fence");
    let mut completion = Some(TaskCompletionGate {
        status: TaskCompletionStatus::Passed,
        reasons: Vec::new(),
        evidence_path: None,
    });

    let reason =
        seal_passed_completion_for_terminal_dispatch(&mut completion, Some(&mut guard)).await;

    assert_eq!(
        reason,
        Some(WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON)
    );
    assert_eq!(
        completion,
        Some(TaskCompletionGate {
            status: TaskCompletionStatus::Partial,
            reasons: vec![WORKSPACE_FINALIZATION_DISPATCH_SEAL_FAILED_REASON.to_string()],
            evidence_path: None,
        })
    );
    assert!(!guard.is_healthy());
    assert_eq!(guard.fence.as_ref(), Some(&stale_fence));

    guard.heartbeat_cancel.cancel();
    if let Some(task) = guard.heartbeat_task.take() {
        task.await.expect("heartbeat task joins");
    }
    guard.fence = None;

    let mut no_store_completion = Some(TaskCompletionGate {
        status: TaskCompletionStatus::Passed,
        reasons: Vec::new(),
        evidence_path: None,
    });
    assert_eq!(
        seal_passed_completion_for_terminal_dispatch(&mut no_store_completion, None).await,
        None
    );
    assert_eq!(
        no_store_completion.as_ref().map(|gate| gate.status),
        Some(TaskCompletionStatus::Passed)
    );
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

    session.clear_taskless_placeholder(&first_state).await;
    assert!(session.active_turn.lock().await.is_none());

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
