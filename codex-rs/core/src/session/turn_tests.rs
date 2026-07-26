use super::*;
use crate::state::ActiveTurn;
use crate::state::RunningTask;
use crate::state::TaskKind;
use crate::state::TurnState;
use crate::state::TurnTerminalCoordinator;
use crate::tasks::AnySessionTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;
use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use codex_thread_store::*;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::Span;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Default)]
struct PaginatedTestThreadStore {
    histories: tokio::sync::Mutex<HashMap<ThreadId, Vec<RolloutItem>>>,
}

fn unsupported_thread_store<T>(operation: &'static str) -> ThreadStoreFuture<'static, T> {
    Box::pin(async move { Err(ThreadStoreError::Unsupported { operation }) })
}

fn paginated_stored_thread(thread_id: ThreadId) -> StoredThread {
    let now = chrono::Utc::now();
    StoredThread {
        thread_id,
        extra_config: None,
        rollout_path: None,
        forked_from_id: None,
        parent_thread_id: None,
        preview: String::new(),
        name: None,
        model_provider: "test".to_string(),
        model: None,
        reasoning_effort: None,
        created_at: now,
        updated_at: now,
        recency_at: now,
        archived_at: None,
        cwd: std::path::PathBuf::new(),
        cli_version: String::new(),
        source: SessionSource::Exec,
        history_mode: ThreadHistoryMode::Paginated,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        agent_path: None,
        git_info: None,
        approval_mode: AskForApproval::Never,
        permission_profile: PermissionProfile::default(),
        token_usage: None,
        first_user_message: None,
        history: None,
    }
}

impl ThreadStore for PaginatedTestThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn default_history_mode(&self) -> ThreadHistoryMode {
        ThreadHistoryMode::Paginated
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.histories
                .lock()
                .await
                .entry(params.thread_id)
                .or_default();
            Ok(())
        })
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let mut histories = self.histories.lock().await;
            if let Some(history) = params.history {
                histories.insert(params.thread_id, Arc::unwrap_or_clone(history));
            } else {
                histories.entry(params.thread_id).or_default();
            }
            Ok(())
        })
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.histories
                .lock()
                .await
                .entry(params.thread_id)
                .or_default()
                .extend(codex_rollout::persisted_rollout_items(
                    &params.items,
                    ThreadHistoryMode::Paginated,
                ));
            Ok(())
        })
    }

    fn persist_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn shutdown_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn discard_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(async move {
            let histories = self.histories.lock().await;
            let items = histories.get(&params.thread_id).cloned().ok_or(
                ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                },
            )?;
            Ok(StoredThreadHistory {
                thread_id: params.thread_id,
                items,
            })
        })
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { Ok(paginated_stored_thread(params.thread_id)) })
    }

    fn read_thread_by_rollout_path(
        &self,
        _params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        unsupported_thread_store("read_thread_by_rollout_path")
    }

    fn list_threads(&self, _params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async {
            Ok(ThreadPage {
                items: Vec::new(),
                next_cursor: None,
            })
        })
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { Ok(paginated_stored_thread(params.thread_id)) })
    }

    fn archive_thread(&self, _params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        unsupported_thread_store("archive_thread")
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move { Ok(paginated_stored_thread(params.thread_id)) })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.histories.lock().await.remove(&params.thread_id);
            Ok(())
        })
    }
}

struct RewriteAgentMessageContributor;

#[derive(Clone)]
struct SignalCompletingTask {
    finish: CancellationToken,
}

impl SessionTask for SignalCompletingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.phase_68_signal_completing"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        tokio::select! {
            _ = self.finish.cancelled() => {}
            _ = cancellation_token.cancelled() => {}
        }
        Ok(None)
    }
}

#[derive(Clone)]
struct TurnStartedSignalTask {
    finish: CancellationToken,
}

impl SessionTask for TurnStartedSignalTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.managed_final_recovery_order_probe"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let sess = session.clone_session();
        sess.send_event_checked(
            ctx.as_ref(),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: ctx.sub_id.clone(),
                trace_id: ctx.trace_id.clone(),
                started_at: ctx.turn_timing_state.started_at_unix_secs().await,
                model_context_window: ctx.model_context_window(),
                collaboration_mode_kind: ctx.collaboration_mode.mode,
            }),
        )
        .await
        .expect("persist probe turn start");
        tokio::select! {
            _ = self.finish.cancelled() => {}
            _ = cancellation_token.cancelled() => {}
        }
        Ok(None)
    }
}

impl TurnItemContributor for RewriteAgentMessageContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.content = vec![AgentMessageContent::Text {
                    text: "plan contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some("msg-1".to_string()),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn provisional_final_text_is_hidden_from_all_precommit_hooks() {
    assert_eq!(
        precommit_hook_message(Some("provisional final".to_string()), true),
        None
    );
    assert_eq!(
        precommit_hook_message(Some("ordinary final".to_string()), false),
        Some("ordinary final".to_string())
    );
}

#[test]
fn classified_non_mutating_fixing_state_requests_closure_before_final() {
    let status = crate::task_evidence::TaskLifecycleStatus {
        phase: crate::task_evidence::TaskPhase::Fixing,
        outcome: None,
        mutation_revision: 0,
        accepted_evidence_revision: 0,
        review_required: false,
        closure_fingerprint: None,
        incomplete_occurrences: 0,
        known_roots: Vec::new(),
        unsupported_mutation_targets: Vec::new(),
        validation_receipt_ids: Vec::new(),
        command_receipt_ids: Vec::new(),
        message: String::new(),
    };

    assert!(task_lifecycle_requires_closure(&status));
}

fn non_openai_model_provider(server: &wiremock::MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.name = "OpenAI (phase 68 test)".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider
}

fn write_one_shot_stop_hook(home: &Path) -> Result<()> {
    let script_path = home.join("phase_68_stop_hook.py");
    let counter_path = home.join("phase_68_stop_hook.count");
    let counter_path = serde_json::to_string(&counter_path.to_string_lossy())?;
    let script = format!(
        r#"import json
from pathlib import Path
import sys

json.load(sys.stdin)
counter_path = Path({counter_path})
if not counter_path.exists():
    counter_path.write_text("1", encoding="utf-8")
    print(json.dumps({{"decision": "block", "reason": "continue after evidence warning"}}))
else:
    print(json.dumps({{"systemMessage": "stop hook continuation complete"}}))
"#,
    );
    let command = format!("python3 \"{}\"", script_path.display());
    let command_windows = format!("python \"{}\"", script_path.display());
    let hooks = serde_json::json!({
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "commandWindows": command_windows,
                }]
            }]
        }
    });
    fs::write(script_path, script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

async fn prepare_managed_final_for_test(
    session: &mut Session,
    turn_context: &TurnContext,
    text: &str,
) -> (PendingManagedFinal, tempfile::TempDir, tempfile::TempDir) {
    let codex_home = tempfile::tempdir().expect("create task evidence home");
    let repository = tempfile::tempdir().expect("create task repository");
    session.services.task_evidence = crate::task_evidence::TaskEvidenceLedger::load_or_new(
        codex_home.path().to_path_buf(),
        session.thread_id,
        repository.path(),
    )
    .await;
    let pending = prepare_managed_final_on_current_ledger(session, turn_context, text).await;
    (pending, codex_home, repository)
}

async fn prepare_managed_final_on_current_ledger(
    session: &Session,
    turn_context: &TurnContext,
    text: &str,
) -> PendingManagedFinal {
    session
        .services
        .task_evidence
        .begin_turn(&turn_context.sub_id, "managed final test")
        .await
        .expect("begin managed task turn");

    let response_item = assistant_output_text(text);
    let item_id = response_item
        .id()
        .expect("assistant response id")
        .to_string();
    assert!(
        session
            .services
            .task_evidence
            .authorize_final_item(&turn_context.sub_id, &item_id)
            .await
            .expect("reserve managed final")
    );
    session
        .services
        .task_evidence
        .mark_final_item_persisted(&turn_context.sub_id, &item_id, &response_item)
        .await
        .expect("persist managed final marker");
    let agent_item = handle_non_tool_response_item(
        session,
        TurnItemContributorPolicy::Skip,
        &response_item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant response should produce a turn item");

    PendingManagedFinal {
        item_id,
        agent_item,
        plan_item: None,
        facts: FinalizedTurnItemFacts {
            memory_citation: None,
            last_agent_message: Some(text.to_string()),
            defers_mailbox_delivery_to_next_turn: true,
        },
    }
}

async fn install_managed_active_turn_for_test(
    session: &Session,
    turn_context: Arc<TurnContext>,
) -> Arc<Mutex<TurnState>> {
    let task: Arc<dyn AnySessionTask> = Arc::new(SignalCompletingTask {
        finish: CancellationToken::new(),
    });
    let worker = tokio::spawn(std::future::pending::<()>());
    let worker_abort_handle = worker.abort_handle();
    let supervisor = tokio::spawn(async move {
        let _ = worker.await;
    });
    let turn_state = Arc::new(Mutex::new(TurnState::default()));
    let running_task = RunningTask {
        done: Arc::new(Notify::new()),
        kind: TaskKind::Regular,
        task,
        cancellation_token: CancellationToken::new(),
        worker_abort_handle,
        _supervisor_handle: supervisor,
        task_span: Span::none(),
        turn_context: Arc::clone(&turn_context),
        turn_extension_data: Arc::clone(&turn_context.extension_data),
        task_evidence_managed: true,
        _agent_execution_guard: None,
    };
    let mut active = session.active_turn.lock().await;
    assert!(active.is_none(), "test session already has an active turn");
    *active = Some(ActiveTurn {
        task: Some(running_task),
        turn_state: Arc::clone(&turn_state),
        terminal: Some(TurnTerminalCoordinator::new(turn_context.sub_id.clone())),
    });
    turn_state
}

async fn remove_test_active_turn(session: &Session) {
    let mut active = session.active_turn.lock().await;
    if let Some(running) = active.as_mut().and_then(|turn| turn.task.take()) {
        running.cancellation_token.cancel();
        running.worker_abort_handle.abort();
        running._supervisor_handle.abort();
    }
    *active = None;
}

async fn attach_paginated_thread_persistence(
    session: &mut Session,
) -> Arc<PaginatedTestThreadStore> {
    {
        let mut state = session.state.lock().await;
        state.session_configuration.history_mode = ThreadHistoryMode::Paginated;
    }
    let store = Arc::new(PaginatedTestThreadStore::default());
    let thread_store: Arc<dyn ThreadStore> = store.clone();
    let config = session.get_config().await;
    let live_thread = LiveThread::create(
        Arc::clone(&thread_store),
        CreateThreadParams {
            session_id: session.session_id(),
            thread_id: session.thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "managed_final_outbox_test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Paginated,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await
    .expect("create paginated thread persistence");
    session.services.thread_store = thread_store;
    session.services.live_thread = Some(live_thread);
    store
}

fn managed_final_items(pending: &PendingManagedFinal) -> Vec<TurnItem> {
    pending
        .plan_item
        .iter()
        .cloned()
        .chain(std::iter::once(pending.agent_item.clone()))
        .collect()
}

async fn stage_and_commit_managed_final(
    session: &Session,
    turn_context: &TurnContext,
    pending: &PendingManagedFinal,
) -> (String, Vec<TurnItem>) {
    let items = managed_final_items(pending);
    let emission_key = session
        .services
        .task_evidence
        .stage_final_emission_items(&turn_context.sub_id, &pending.item_id, &items)
        .await
        .expect("stage durable final outbox");
    assert!(
        session
            .services
            .task_evidence
            .commit_final_item(&turn_context.sub_id, &pending.item_id)
            .await
            .expect("commit durable final outbox")
    );
    (emission_key, items)
}

async fn reload_task_evidence(
    session: &mut Session,
    codex_home: &tempfile::TempDir,
    repository: &tempfile::TempDir,
) {
    session.services.task_evidence = crate::task_evidence::TaskEvidenceLedger::load_or_new(
        codex_home.path().to_path_buf(),
        session.thread_id,
        repository.path(),
    )
    .await;
}

async fn durable_final_item_completed_count(
    session: &Session,
    turn_id: &str,
    item_id: &str,
) -> usize {
    session
        .live_thread()
        .expect("live thread")
        .load_history(/*include_archived*/ true)
        .await
        .expect("load durable final history")
        .items
        .iter()
        .filter(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::ItemCompleted(event))
                    if event.turn_id == turn_id && event.item.id() == item_id
            )
        })
        .count()
}

async fn durable_turn_complete_count(session: &Session, turn_id: &str) -> usize {
    session
        .live_thread()
        .expect("live thread")
        .load_history(/*include_archived*/ true)
        .await
        .expect("load durable final history")
        .items
        .iter()
        .filter(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnComplete(event))
                    if event.turn_id == turn_id
            )
        })
        .count()
}

#[test]
fn legacy_final_reconciliation_is_scoped_after_the_exact_provisional_item() {
    let thread_id = ThreadId::new();
    let final_item = TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
        id: "msg-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: "repeated final".to_string(),
        }],
        phase: None,
        memory_citation: None,
    });
    let prior_same_text = RolloutItem::EventMsg(EventMsg::AgentMessage(
        codex_protocol::protocol::AgentMessageEvent {
            message: "repeated final".to_string(),
            phase: None,
            memory_citation: None,
        },
    ));
    let mut history = vec![
        prior_same_text.clone(),
        RolloutItem::ResponseItem(assistant_output_text("repeated final")),
    ];
    assert!(
        !durable_managed_final_batch_present(
            &history,
            thread_id,
            "current-turn",
            "msg-1",
            std::slice::from_ref(&final_item),
            ThreadHistoryMode::Legacy,
            false,
        ),
        "an identical legacy message from an older turn must not acknowledge this final"
    );

    history.push(prior_same_text);
    assert!(
        durable_managed_final_batch_present(
            &history,
            thread_id,
            "current-turn",
            "msg-1",
            &[final_item],
            ThreadHistoryMode::Legacy,
            false,
        ),
        "the matching legacy lifecycle event after the exact provisional item must reconcile"
    );
}

#[test]
fn legacy_plan_only_reconciliation_is_anchored_and_idempotent() {
    let thread_id = ThreadId::new();
    let turn_id = "current-turn";
    let plan = TurnItem::Plan(PlanItem {
        id: "current-turn-plan".to_string(),
        text: "- verify the exact plan".to_string(),
    });
    let lifecycle = vec![
        RolloutItem::EventMsg(EventMsg::ItemStarted(ItemStartedEvent {
            thread_id,
            turn_id: turn_id.to_string(),
            item: plan.clone(),
            started_at_ms: 1,
        })),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id,
            turn_id: turn_id.to_string(),
            item: plan.clone(),
            completed_at_ms: 2,
        })),
    ];
    let persisted_lifecycle =
        codex_rollout::persisted_rollout_items(&lifecycle, ThreadHistoryMode::Legacy);
    assert!(
        !persisted_lifecycle.is_empty(),
        "legacy plan lifecycle must have a durable representation"
    );
    let mut history = persisted_lifecycle.clone();
    history.push(RolloutItem::ResponseItem(assistant_output_text(
        "<proposed_plan>\n- verify the exact plan\n</proposed_plan>",
    )));
    assert!(
        !durable_managed_final_batch_present(
            &history,
            thread_id,
            turn_id,
            "msg-1",
            std::slice::from_ref(&plan),
            ThreadHistoryMode::Legacy,
            false,
        ),
        "a matching plan lifecycle before the provisional item must not acknowledge it"
    );

    history.extend(persisted_lifecycle);
    for _ in 0..2 {
        assert!(
            durable_managed_final_batch_present(
                &history,
                thread_id,
                turn_id,
                "msg-1",
                std::slice::from_ref(&plan),
                ThreadHistoryMode::Legacy,
                false,
            ),
            "reconciliation must repeatedly recognize the exact post-anchor plan batch"
        );
    }
}

#[tokio::test]
async fn plan_only_managed_final_emits_only_the_bound_plan_lifecycle() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let raw_plan = "<proposed_plan>\n- verify the managed final\n</proposed_plan>";
    let (mut pending, _codex_home, _repository) =
        prepare_managed_final_for_test(&mut session, turn_context.as_ref(), raw_plan).await;
    let plan_text = codex_utils_stream_parser::extract_proposed_plan_text(raw_plan)
        .map(|text| codex_utils_stream_parser::strip_citations(&text).0)
        .expect("extract proposed plan");
    let TurnItem::AgentMessage(agent_message) = &mut pending.agent_item else {
        panic!("managed final should start as an agent message");
    };
    agent_message.content = vec![AgentMessageContent::Text {
        text: String::new(),
    }];
    pending.plan_item = Some(TurnItem::Plan(PlanItem {
        id: format!("{}-plan", turn_context.sub_id),
        text: plan_text.clone(),
    }));
    pending.facts.last_agent_message = None;
    let plan_item_id = format!("{}-plan", turn_context.sub_id);
    install_managed_active_turn_for_test(&session, Arc::clone(&turn_context)).await;

    let outcome = commit_and_emit_pending_managed_final(&session, turn_context.as_ref(), pending)
        .await
        .expect("commit plan-only managed final");
    assert!(matches!(outcome, PendingManagedFinalOutcome::Emitted(None)));

    let delivered = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    let lifecycle = delivered
        .iter()
        .filter_map(|event| match &event.msg {
            EventMsg::ItemStarted(item) => Some(("started", &item.item)),
            EventMsg::ItemCompleted(item) => Some(("completed", &item.item)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 2);
    assert_eq!(lifecycle[0].0, "started");
    assert_eq!(lifecycle[1].0, "completed");
    assert!(lifecycle.iter().all(|(_, item)| {
        matches!(item, TurnItem::Plan(plan) if plan.id == plan_item_id && plan.text == plan_text)
    }));
    assert!(
        delivered.iter().all(|event| {
            !match &event.msg {
                EventMsg::ItemStarted(item) => {
                    matches!(&item.item, TurnItem::AgentMessage(_))
                }
                EventMsg::ItemCompleted(item) => {
                    matches!(&item.item, TurnItem::AgentMessage(_))
                }
                _ => false,
            }
        }),
        "an empty agent message must not escape beside a plan-only final"
    );
    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(crate::task_evidence::ManagedFinalState::TerminalPending)
    );
    remove_test_active_turn(&session).await;
}

#[tokio::test]
async fn pending_mailbox_input_wins_before_managed_final_commit() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    let (pending, _codex_home, _repository) =
        prepare_managed_final_for_test(&mut session, turn_context.as_ref(), "stale final").await;
    let item_id = pending.item_id.clone();
    install_managed_active_turn_for_test(&session, Arc::clone(&turn_context)).await;
    session
        .input_queue
        .enqueue_mailbox_communication(codex_protocol::protocol::InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "late task-relevant input".to_string(),
            false,
        ))
        .await;

    let outcome = commit_and_emit_pending_managed_final(&session, turn_context.as_ref(), pending)
        .await
        .expect("pending input should supersede the provisional final");
    assert!(matches!(outcome, PendingManagedFinalOutcome::PendingInput));
    assert!(
        events.try_recv().is_err(),
        "the superseded final must not emit lifecycle events"
    );
    assert!(session.input_queue.has_pending_mailbox_items().await);
    assert!(
        !session
            .services
            .task_evidence
            .commit_final_item(&turn_context.sub_id, &item_id)
            .await
            .expect("inspect superseded final"),
        "pending input must leave the provisional final uncommittable"
    );
    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(crate::task_evidence::ManagedFinalState::NoFinalCandidate)
    );
    remove_test_active_turn(&session).await;
}

#[tokio::test]
async fn final_commit_boundary_holds_the_active_turn_until_durable_commit_finishes() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut session = session;
    let turn_context = Arc::new(turn_context);
    let (pending, _codex_home, _repository) =
        prepare_managed_final_for_test(&mut session, turn_context.as_ref(), "serialized final")
            .await;
    let items = managed_final_items(&pending);
    session
        .services
        .task_evidence
        .stage_final_emission_items(&turn_context.sub_id, &pending.item_id, &items)
        .await
        .expect("stage serialized final");
    install_managed_active_turn_for_test(&session, Arc::clone(&turn_context)).await;
    let session = Arc::new(session);
    let entered = CancellationToken::new();
    let release = CancellationToken::new();
    let commit_session = Arc::clone(&session);
    let commit_turn_id = turn_context.sub_id.clone();
    let commit_item_id = pending.item_id.clone();
    let entered_commit = entered.clone();
    let release_commit = release.clone();
    let commit_task = tokio::spawn(async move {
        commit_session
            .input_queue
            .commit_final_if_no_pending_input(
                &commit_session.active_turn,
                &commit_turn_id,
                || async {
                    entered_commit.cancel();
                    release_commit.cancelled().await;
                    commit_session
                        .services
                        .task_evidence
                        .commit_final_item(&commit_turn_id, &commit_item_id)
                        .await
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), entered.cancelled())
        .await
        .expect("commit closure should enter");

    let lock_session = Arc::clone(&session);
    let active_lock_task = tokio::spawn(async move {
        let _active = lock_session.active_turn.lock().await;
    });
    tokio::task::yield_now().await;
    assert!(
        !active_lock_task.is_finished(),
        "terminal scheduling must not acquire the active turn while persistence is in flight"
    );

    release.cancel();
    assert_eq!(
        commit_task.await.expect("join final commit boundary"),
        Ok(FinalCommitBoundary::Committed)
    );
    tokio::time::timeout(Duration::from_secs(2), active_lock_task)
        .await
        .expect("active-turn waiter should resume after commit")
        .expect("join active-turn waiter");
    assert!(
        session
            .services
            .task_evidence
            .recoverable_final_emission()
            .await
            .expect("inspect committed final")
            .is_some(),
        "commit-first must leave a durable outbox for terminal recovery"
    );
    remove_test_active_turn(session.as_ref()).await;
}

#[tokio::test]
async fn terminal_claim_before_final_commit_rejects_and_supersedes_the_reservation() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let (pending, _codex_home, _repository) = prepare_managed_final_for_test(
        &mut session,
        turn_context.as_ref(),
        "cancelled before commit",
    )
    .await;
    let items = managed_final_items(&pending);
    session
        .services
        .task_evidence
        .stage_final_emission_items(&turn_context.sub_id, &pending.item_id, &items)
        .await
        .expect("stage cancellable final");
    install_managed_active_turn_for_test(&session, Arc::clone(&turn_context)).await;
    let running = {
        let mut active = session.active_turn.lock().await;
        active
            .as_mut()
            .and_then(|turn| turn.task.take())
            .expect("terminal scheduler should claim the running task")
    };
    session
        .services
        .task_evidence
        .abort_uncommitted_final_reservations_for_turn(&turn_context.sub_id)
        .await
        .expect("durably supersede terminal-losing final");
    let commit_called = Arc::new(AtomicBool::new(false));
    let commit_called_for_closure = Arc::clone(&commit_called);

    let outcome = session
        .input_queue
        .commit_final_if_no_pending_input(&session.active_turn, &turn_context.sub_id, || async {
            commit_called_for_closure.store(true, Ordering::Release);
            Ok(true)
        })
        .await
        .expect("terminal-first boundary result");
    assert_eq!(outcome, FinalCommitBoundary::Rejected);
    assert!(
        !commit_called.load(Ordering::Acquire),
        "a terminal-claimed turn must not invoke the durable final commit"
    );
    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(crate::task_evidence::ManagedFinalState::NoFinalCandidate)
    );
    running.cancellation_token.cancel();
    running.worker_abort_handle.abort();
    running._supervisor_handle.abort();
    remove_test_active_turn(&session).await;
}

#[tokio::test]
async fn failed_managed_final_append_keeps_the_committed_outbox_fence() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.ephemeral = false,
        )
        .await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    let (pending, _codex_home, _repository) =
        prepare_managed_final_for_test(&mut session, turn_context.as_ref(), "final answer").await;
    let item_id = pending.item_id.clone();
    install_managed_active_turn_for_test(&session, Arc::clone(&turn_context)).await;

    let err = match commit_and_emit_pending_managed_final(&session, turn_context.as_ref(), pending)
        .await
    {
        Err(err) => err,
        Ok(_) => panic!("managed final lifecycle requires durable rollout persistence"),
    };

    assert!(
        matches!(err, CodexErr::InvalidRequest(message) if message.contains("rollout persistence is disabled"))
    );
    assert!(
        events.try_recv().is_err(),
        "no managed final lifecycle event may be delivered before the checked append succeeds"
    );
    assert!(
        !session
            .services
            .task_evidence
            .authorize_final_item(&turn_context.sub_id, &item_id)
            .await
            .expect("retry final reservation"),
        "a failed lifecycle append must keep the committed outbox fence"
    );
    assert!(
        session
            .services
            .task_evidence
            .recoverable_final_emission()
            .await
            .expect("inspect durable final outbox")
            .is_some(),
        "the exact committed final must remain recoverable"
    );
    remove_test_active_turn(&session).await;
}

#[tokio::test]
async fn ephemeral_managed_final_completes_with_in_memory_outboxes() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.ephemeral = true,
        )
        .await;
    assert!(turn_context.config.ephemeral);
    let pending = prepare_managed_final_on_current_ledger(
        session.as_ref(),
        turn_context.as_ref(),
        "ephemeral final answer",
    )
    .await;
    let item_id = pending.item_id.clone();
    install_managed_active_turn_for_test(session.as_ref(), Arc::clone(&turn_context)).await;

    let outcome =
        commit_and_emit_pending_managed_final(session.as_ref(), turn_context.as_ref(), pending)
            .await
            .expect("ephemeral managed final should emit without rollout persistence");
    assert!(matches!(outcome, PendingManagedFinalOutcome::Emitted(_)));
    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(ManagedFinalState::TerminalPending)
    );
    let terminal = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_context.sub_id.clone(),
        last_agent_message: Some("ephemeral final answer".to_string()),
        completion: None,
        completed_at: Some(123),
        duration_ms: Some(45),
        time_to_first_token_ms: Some(6),
        timing: None,
    });
    emit_managed_final_terminal_checked(session.as_ref(), turn_context.as_ref(), &terminal)
        .await
        .expect("ephemeral managed terminal should emit without rollout persistence");

    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(ManagedFinalState::Completed)
    );
    assert_eq!(
        session
            .current_rollout_path()
            .await
            .expect("ephemeral rollout lookup"),
        None
    );
    let delivered = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(delivered.iter().any(|event| {
        matches!(
            &event.msg,
            EventMsg::ItemCompleted(completed)
                if completed.turn_id == turn_context.sub_id && completed.item.id() == item_id
        )
    }));
    assert!(delivered.iter().any(|event| {
        matches!(
            &event.msg,
            EventMsg::TurnComplete(completed) if completed.turn_id == turn_context.sub_id
        )
    }));
    remove_test_active_turn(session.as_ref()).await;
}

#[tokio::test]
async fn ephemeral_committed_final_recovers_from_in_memory_outboxes() {
    let (session, turn_context, events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("Test API Key"),
            Vec::new(),
            |config| config.ephemeral = true,
        )
        .await;
    let pending = prepare_managed_final_on_current_ledger(
        session.as_ref(),
        turn_context.as_ref(),
        "recover ephemeral final",
    )
    .await;
    let item_id = pending.item_id.clone();
    stage_and_commit_managed_final(session.as_ref(), turn_context.as_ref(), &pending).await;

    recover_pending_managed_final_outbox(session.as_ref())
        .await
        .expect("ephemeral committed final should recover without rollout persistence");

    assert_eq!(
        session
            .services
            .task_evidence
            .managed_final_state_for_turn(&turn_context.sub_id)
            .await,
        Some(ManagedFinalState::Completed)
    );
    assert_eq!(
        session
            .current_rollout_path()
            .await
            .expect("ephemeral rollout lookup"),
        None
    );
    let delivered = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(delivered.iter().any(|event| {
        matches!(
            &event.msg,
            EventMsg::ItemCompleted(completed)
                if completed.turn_id == turn_context.sub_id && completed.item.id() == item_id
        )
    }));
    assert!(delivered.iter().any(|event| {
        matches!(
            &event.msg,
            EventMsg::TurnComplete(completed) if completed.turn_id == turn_context.sub_id
        )
    }));
}

#[tokio::test]
async fn committed_final_recovers_before_append_under_the_original_turn_once() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let turn_context = session
        .new_default_turn_with_sub_id("original-final-turn".to_string())
        .await;
    let (pending, codex_home, repository) =
        prepare_managed_final_for_test(&mut session, &turn_context, "restart-safe final").await;
    let item_id = pending.item_id.clone();
    let (emission_key, _items) =
        stage_and_commit_managed_final(&session, &turn_context, &pending).await;

    reload_task_evidence(&mut session, &codex_home, &repository).await;
    let recovered = session
        .services
        .task_evidence
        .recoverable_final_emission()
        .await
        .expect("load committed outbox")
        .expect("incomplete committed final");
    assert_eq!(recovered.emission_key, emission_key);
    assert!(
        session
            .services
            .task_evidence
            .begin_turn("new-turn", "new task")
            .await
            .expect_err("new turn must wait for committed final recovery")
            .contains("final emission is incomplete")
    );

    recover_pending_managed_final_outbox(&session)
        .await
        .expect("recover final before a new task turn");

    let delivered = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(!delivered.is_empty());
    assert!(
        delivered
            .iter()
            .all(|event| event.id == turn_context.sub_id),
        "recovery must deliver under the stored original turn id"
    );
    assert!(delivered.iter().any(|event| {
        matches!(
            &event.msg,
            EventMsg::ItemCompleted(completed)
                if completed.turn_id == turn_context.sub_id
                    && completed.item.id() == item_id
        )
    }));
    assert_eq!(
        durable_final_item_completed_count(&session, &turn_context.sub_id, &item_id).await,
        1,
        "the recovered final item must have exactly one durable completion"
    );
    assert_eq!(
        durable_turn_complete_count(&session, &turn_context.sub_id).await,
        1,
        "the recovered turn must have exactly one durable terminal event"
    );
    assert!(
        session
            .services
            .task_evidence
            .recoverable_final_emission()
            .await
            .expect("inspect completed outbox")
            .is_none()
    );
    session
        .services
        .task_evidence
        .begin_turn("new-turn", "new task")
        .await
        .expect("completed terminal outbox releases the next task turn");
}

#[tokio::test]
async fn committed_final_recovers_once_after_repository_cwd_change() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let original_context = session
        .new_default_turn_with_sub_id("repository-moved-final-turn".to_string())
        .await;
    let (pending, codex_home, original_repository) =
        prepare_managed_final_for_test(&mut session, &original_context, "repository-moved final")
            .await;
    let item_id = pending.item_id.clone();
    stage_and_commit_managed_final(&session, &original_context, &pending).await;
    while events.try_recv().is_ok() {}

    let relocated_repository = tempfile::tempdir().expect("create relocated task repository");
    assert_ne!(original_repository.path(), relocated_repository.path());
    reload_task_evidence(&mut session, &codex_home, &relocated_repository).await;
    let recovered = session
        .services
        .task_evidence
        .recoverable_final_emission()
        .await
        .expect("load committed outbox after repository change")
        .expect("repository change must preserve the incomplete committed final");
    assert_eq!(recovered.turn_id, original_context.sub_id);

    recover_pending_managed_final_outbox(&session)
        .await
        .expect("recover committed final after repository change");

    let first_recovery = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        first_recovery
            .iter()
            .all(|event| event.id == original_context.sub_id),
        "repository-change recovery must remain bound to the stored original turn"
    );
    assert_eq!(
        first_recovery
            .iter()
            .filter(|event| {
                matches!(
                    &event.msg,
                    EventMsg::ItemCompleted(completed)
                        if completed.turn_id == original_context.sub_id
                            && completed.item.id() == item_id
                )
            })
            .count(),
        1,
        "the committed final item must be recovered exactly once"
    );
    assert_eq!(
        first_recovery
            .iter()
            .filter(|event| {
                matches!(
                    &event.msg,
                    EventMsg::TurnComplete(completed)
                        if completed.turn_id == original_context.sub_id
                )
            })
            .count(),
        1,
        "the original turn must be completed exactly once"
    );
    assert_eq!(
        durable_final_item_completed_count(&session, &original_context.sub_id, &item_id).await,
        1
    );
    assert_eq!(
        durable_turn_complete_count(&session, &original_context.sub_id).await,
        1
    );

    reload_task_evidence(&mut session, &codex_home, &relocated_repository).await;
    recover_pending_managed_final_outbox(&session)
        .await
        .expect("repeat recovery after repository change");

    assert!(
        events.try_recv().is_err(),
        "completed repository-change recovery must not emit duplicate events"
    );
    assert_eq!(
        durable_final_item_completed_count(&session, &original_context.sub_id, &item_id).await,
        1
    );
    assert_eq!(
        durable_turn_complete_count(&session, &original_context.sub_id).await,
        1
    );
}

#[tokio::test]
async fn task_start_recovers_committed_final_before_any_new_task_event() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let original_context = session
        .new_default_turn_with_sub_id("recovery-before-task-start".to_string())
        .await;
    let (pending, codex_home, repository) =
        prepare_managed_final_for_test(&mut session, &original_context, "ordered final").await;
    stage_and_commit_managed_final(&session, &original_context, &pending).await;
    reload_task_evidence(&mut session, &codex_home, &repository).await;
    while events.try_recv().is_ok() {}

    let session = Arc::new(session);
    let next_context = session
        .new_default_turn_with_sub_id("task-after-recovery".to_string())
        .await;
    let finish = CancellationToken::new();
    session
        .spawn_task(
            Arc::clone(&next_context),
            Vec::new(),
            TurnStartedSignalTask {
                finish: finish.clone(),
            },
        )
        .await;
    let terminal = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.clone())
        .expect("probe task should expose its terminal coordinator");

    let mut delivered = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for recovery ordering events")
            .expect("event channel closed");
        let reached_new_start = matches!(
            &event.msg,
            EventMsg::TurnStarted(started) if started.turn_id == next_context.sub_id
        );
        delivered.push(event);
        if reached_new_start {
            break;
        }
    }
    let recovered_terminal_index = delivered
        .iter()
        .position(|event| {
            matches!(
                &event.msg,
                EventMsg::TurnComplete(completed)
                    if completed.turn_id == original_context.sub_id
            )
        })
        .expect("prior committed final should recover its terminal event");
    let next_start_index = delivered
        .iter()
        .position(|event| {
            matches!(
                &event.msg,
                EventMsg::TurnStarted(started) if started.turn_id == next_context.sub_id
            )
        })
        .expect("probe task should emit its start event");
    assert!(
        recovered_terminal_index < next_start_index,
        "the shared task-start boundary must finish prior final recovery before any new task event"
    );

    finish.cancel();
    tokio::time::timeout(Duration::from_secs(5), terminal.wait_completed())
        .await
        .expect("probe task terminal finalization timed out");
}

#[tokio::test]
async fn committed_final_reconciles_after_append_without_duplicate_delivery() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let turn_context = session
        .new_default_turn_with_sub_id("original-appended-final-turn".to_string())
        .await;
    let (pending, codex_home, repository) =
        prepare_managed_final_for_test(&mut session, &turn_context, "already appended final").await;
    let item_id = pending.item_id.clone();
    let (emission_key, items) =
        stage_and_commit_managed_final(&session, &turn_context, &pending).await;
    session
        .emit_managed_final_items_checked(&turn_context, items)
        .await
        .expect("append committed final before simulated crash");
    session
        .services
        .task_evidence
        .mark_final_emission_items_emitted(&turn_context.sub_id, &pending.item_id, &emission_key)
        .await
        .expect("acknowledge durable final items before simulated crash");
    while events.try_recv().is_ok() {}
    assert_eq!(
        durable_final_item_completed_count(&session, &turn_context.sub_id, &item_id).await,
        1
    );

    reload_task_evidence(&mut session, &codex_home, &repository).await;
    let recovered = session
        .services
        .task_evidence
        .recoverable_final_emission()
        .await
        .expect("load committed outbox")
        .expect("incomplete committed final");
    assert_eq!(recovered.emission_key, emission_key);

    recover_pending_managed_final_outbox(&session)
        .await
        .expect("reconcile already-appended final");

    let delivered = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        delivered.iter().any(|event| {
            matches!(
                &event.msg,
                EventMsg::TurnComplete(completed)
                    if completed.turn_id == turn_context.sub_id
            )
        }),
        "recovery must finish the missing terminal event"
    );
    assert!(
        delivered.iter().all(|event| {
            !matches!(
                &event.msg,
                EventMsg::ItemStarted(_) | EventMsg::ItemCompleted(_)
            )
        }),
        "an already durable final item batch must not be delivered a second time"
    );
    assert_eq!(
        durable_final_item_completed_count(&session, &turn_context.sub_id, &item_id).await,
        1,
        "history reconciliation must not duplicate the final completion"
    );
    assert_eq!(
        durable_turn_complete_count(&session, &turn_context.sub_id).await,
        1,
        "recovery must append exactly one terminal event"
    );
}

#[tokio::test]
async fn committed_final_reconciles_terminal_append_before_ack_without_duplicate_delivery() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let turn_context = session
        .new_default_turn_with_sub_id("terminal-appended-final-turn".to_string())
        .await;
    let (pending, codex_home, repository) =
        prepare_managed_final_for_test(&mut session, &turn_context, "terminal-safe final").await;
    let item_id = pending.item_id.clone();
    let (emission_key, items) =
        stage_and_commit_managed_final(&session, &turn_context, &pending).await;
    session
        .emit_managed_final_items_checked(&turn_context, items)
        .await
        .expect("append committed final items");
    session
        .services
        .task_evidence
        .mark_final_emission_items_emitted(&turn_context.sub_id, &pending.item_id, &emission_key)
        .await
        .expect("acknowledge committed final items");
    let terminal = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_context.sub_id.clone(),
        last_agent_message: Some("terminal-safe final".to_string()),
        completion: None,
        completed_at: Some(123),
        duration_ms: Some(45),
        time_to_first_token_ms: Some(6),
        timing: None,
    });
    let staged_terminal = session
        .services
        .task_evidence
        .stage_final_terminal_event(&turn_context.sub_id, &terminal)
        .await
        .expect("stage exact terminal event");
    session
        .send_event_checked(&turn_context, staged_terminal)
        .await
        .expect("append terminal before simulated crash");
    while events.try_recv().is_ok() {}

    reload_task_evidence(&mut session, &codex_home, &repository).await;
    recover_pending_managed_final_outbox(&session)
        .await
        .expect("reconcile terminal append before acknowledgement");

    assert!(
        events.try_recv().is_err(),
        "an already durable terminal event must not be delivered a second time"
    );
    assert_eq!(
        durable_final_item_completed_count(&session, &turn_context.sub_id, &item_id).await,
        1
    );
    assert_eq!(
        durable_turn_complete_count(&session, &turn_context.sub_id).await,
        1
    );
    assert!(
        session
            .services
            .task_evidence
            .recoverable_final_emission()
            .await
            .expect("inspect completed outbox")
            .is_none()
    );
}

#[tokio::test]
async fn schema_v10_completed_final_reconciles_existing_terminal_without_duplication() {
    let (session, _initial_context, events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let mut session = match Arc::try_unwrap(session) {
        Ok(session) => session,
        Err(_) => panic!("test session should be uniquely owned"),
    };
    attach_paginated_thread_persistence(&mut session).await;
    let turn_context = session
        .new_default_turn_with_sub_id("migrated-final-turn".to_string())
        .await;
    let (pending, codex_home, repository) =
        prepare_managed_final_for_test(&mut session, &turn_context, "migrated final").await;
    let item_id = pending.item_id.clone();
    let (emission_key, items) =
        stage_and_commit_managed_final(&session, &turn_context, &pending).await;
    session
        .emit_managed_final_items_checked(&turn_context, items)
        .await
        .expect("append legacy final items");
    session
        .services
        .task_evidence
        .mark_final_emission_items_emitted(&turn_context.sub_id, &item_id, &emission_key)
        .await
        .expect("acknowledge legacy final items");
    let terminal = EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_context.sub_id.clone(),
        last_agent_message: Some("migrated final".to_string()),
        completion: None,
        completed_at: Some(456),
        duration_ms: Some(78),
        time_to_first_token_ms: Some(9),
        timing: None,
    });
    emit_managed_final_terminal_checked(&session, &turn_context, &terminal)
        .await
        .expect("complete the pre-migration outbox");

    let evidence_path = codex_home
        .path()
        .join("task-evidence")
        .join(format!("{}.json", session.thread_id));
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&evidence_path).expect("read task evidence"))
            .expect("parse task evidence");
    document["schema_version"] = serde_json::json!(10);
    let committed = document
        .get_mut("committed_final")
        .and_then(serde_json::Value::as_object_mut)
        .expect("committed final");
    committed.remove("terminal_event");
    committed.remove("terminal_event_staged");
    committed.insert("completed".to_string(), serde_json::json!(true));
    fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&document).expect("encode v10 task evidence"),
    )
    .expect("write v10 task evidence");
    while events.try_recv().is_ok() {}

    reload_task_evidence(&mut session, &codex_home, &repository).await;
    assert!(
        session
            .services
            .task_evidence
            .recoverable_final_emission()
            .await
            .expect("inspect migrated outbox")
            .is_some(),
        "v10 item-only completion must reopen the terminal fence"
    );
    recover_pending_managed_final_outbox(&session)
        .await
        .expect("reconcile migrated terminal");

    assert!(
        events.try_recv().is_err(),
        "migration must recognize the existing terminal instead of delivering a duplicate"
    );
    assert_eq!(
        durable_final_item_completed_count(&session, &turn_context.sub_id, &item_id).await,
        1
    );
    assert_eq!(
        durable_turn_complete_count(&session, &turn_context.sub_id).await,
        1
    );
}

#[tokio::test]
async fn abort_pending_managed_final_clears_the_emission_reservation() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let (pending, _codex_home, _repository) =
        prepare_managed_final_for_test(&mut session, &turn_context, "cancelled final").await;
    let mut pending = Some(pending);
    let item_id = pending
        .as_ref()
        .expect("pending managed final")
        .item_id
        .clone();

    abort_pending_managed_final_reservation(&session, &turn_context, &mut pending)
        .await
        .expect("abort pending managed final");

    assert!(pending.is_none());
    assert!(
        !session
            .services
            .task_evidence
            .commit_final_item(&turn_context.sub_id, &item_id)
            .await
            .expect("inspect cleared reservation"),
        "a cancelled or errored managed final must not remain committable"
    );
}

#[tokio::test]
async fn drain_in_flight_returns_first_error_after_draining_remaining_futures() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let remaining_future_polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remaining_future_polled_clone = Arc::clone(&remaining_future_polled);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(async {
        Err(CodexErr::Fatal("first tool failure".to_string()))
    }));
    in_flight.push_back(Box::pin(async move {
        remaining_future_polled_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        Err(CodexErr::Fatal("second tool failure".to_string()))
    }));

    let error = drain_in_flight(&mut in_flight, Arc::new(session), Arc::new(turn_context))
        .await
        .expect_err("the first in-flight tool error should be returned");

    assert!(remaining_future_polled.load(std::sync::atomic::Ordering::SeqCst));
    assert!(matches!(
        error,
        CodexErr::Fatal(message) if message == "first tool failure"
    ));
}

#[tokio::test]
async fn steering_applies_next_turn_settings_without_building_a_candidate_turn_context() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let next_model = codex_models_manager::bundled_models_response()
        .expect("bundled model catalog should parse")
        .models
        .into_iter()
        .find(|model| model.slug != turn_context.model_info.slug)
        .expect("bundled model catalog should contain an alternative model")
        .slug;
    let active_approval_policy = turn_context.approval_policy.value();
    let active_permission_profile = turn_context.permission_profile.clone();
    let next_approval_policy = if active_approval_policy == AskForApproval::Never {
        AskForApproval::OnRequest
    } else {
        AskForApproval::Never
    };
    let next_permission_profile = if active_permission_profile == PermissionProfile::Disabled {
        PermissionProfile::read_only()
    } else {
        PermissionProfile::Disabled
    };
    session
        .services
        .thread_extension_data
        .insert(turn_context.model_info.clone());
    let model_info_before = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should be initialized");
    let finish = CancellationToken::new();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            SignalCompletingTask {
                finish: finish.clone(),
            },
        )
        .await;

    crate::session::handlers::user_input_or_turn_inner(
        &session,
        "steering-submission".to_string(),
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "steer the active turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                approval_policy: Some(next_approval_policy),
                permission_profile: Some(next_permission_profile.clone()),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: next_model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        },
        /*client_user_message_id*/ None,
    )
    .await;

    let model_info_after = session
        .services
        .thread_extension_data
        .get::<codex_protocol::openai_models::ModelInfo>()
        .expect("thread model info should remain initialized");
    assert!(
        Arc::ptr_eq(&model_info_before, &model_info_after),
        "a successful steer must not build a candidate context or replace thread model metadata"
    );
    assert_eq!(session.collaboration_mode().await.model(), next_model);
    let active_context = session
        .turn_context_for_sub_id(&turn_context.sub_id)
        .await
        .expect("the original turn should remain active");
    assert!(Arc::ptr_eq(&active_context, &turn_context));
    assert_eq!(
        active_context.approval_policy.value(),
        active_approval_policy,
        "steering settings must not rebind the active turn's MCP approval policy"
    );
    assert_eq!(
        active_context.permission_profile, active_permission_profile,
        "steering settings must not rebind the active turn's MCP permission profile"
    );

    let terminal = session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|active_turn| active_turn.terminal.clone())
        .expect("active turn should expose its terminal coordinator");
    finish.cancel();
    terminal.wait_completed().await;

    let next_context = session
        .new_default_turn_with_sub_id("next-turn-after-steer".to_string())
        .await;
    assert_eq!(next_context.model_info.slug, next_model);
    assert_eq!(
        next_context.approval_policy.value(),
        next_approval_policy,
        "the next actual turn must install the steered MCP approval policy"
    );
    assert_eq!(
        next_context.permission_profile, next_permission_profile,
        "the next actual turn must install the steered MCP permission profile"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_response_item_triggers_compaction_before_the_stream_request() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response-item-response"),
                responses::ev_assistant_message("seed-response-item-message", "seed complete"),
                responses::ev_completed_with_tokens(
                    "seed-response-item-response",
                    /*total_tokens*/ 90,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-compact-response"),
                responses::ev_assistant_message("response-item-compact-message", "compact summary"),
                responses::ev_completed_with_tokens(
                    "response-item-compact-response",
                    /*total_tokens*/ 20,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-final-response"),
                responses::ev_assistant_message(
                    "response-item-final-message",
                    "initial response item sampled",
                ),
                responses::ev_completed_with_tokens(
                    "response-item-final-response",
                    /*total_tokens*/ 42,
                ),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.model_context_window = Some(64_000);
        config.model_auto_compact_token_limit = Some(100);
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        let _ = config.features.disable(Feature::RemoteCompactionV2);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history near the compaction limit")
        .await?;
    test.codex
        .submit(Op::UserInput {
            items: Vec::new(),
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: BTreeMap::from([(
                "phase-68-large-initial-response-item".to_string(),
                AdditionalContextEntry {
                    value: "large model-visible response context ".repeat(128),
                    kind: AdditionalContextKind::Application,
                },
            )]),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if matches!(
                test.codex.next_event().await.expect("turn event").msg,
                EventMsg::TurnComplete(_)
            ) {
                break;
            }
        }
    })
    .await
    .expect("the response-item turn should complete after pre-turn compaction");

    let request_count = request_log.requests().len();
    assert_eq!(
        request_count, 3,
        "the large initial ResponseItem must trigger compaction before the turn's sampling request"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response"),
                responses::ev_assistant_message("seed-message", "seed complete"),
                responses::ev_completed_with_tokens("seed-response", /*total_tokens*/ 121),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message("compact-message", "compact summary"),
                responses::ev_completed_with_tokens("compact-response", /*total_tokens*/ 20),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "pending input sampled"),
                responses::ev_completed_with_tokens("final-response", /*total_tokens*/ 42),
            ]),
        ],
    )
    .await;
    let provider = non_openai_model_provider(&server);
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider = provider;
        config.model_context_window = Some(64_000);
        config.model_auto_compact_token_limit = Some(100);
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        let _ = config.features.disable(Feature::RemoteCompactionV2);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history").await?;
    test.submit_turn(&"oversized pending payload ".repeat(128))
        .await?;

    assert_eq!(
        request_log.requests().len(),
        3,
        "the second turn should compact once, then sample instead of repeatedly compacting the same pending payload"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot() -> Result<()> {
    let command = match core_test_support::stdio_server_bin() {
        Ok(command) => command,
        Err(err) => {
            tracing::warn!(
                %err,
                "test_stdio_server unavailable; skipping MCP snapshot regression"
            );
            return Ok(());
        }
    };
    let (mut session, mut turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    Arc::get_mut(&mut session)
        .expect("test session should be uniquely owned")
        .services
        .auth_manager = Arc::clone(&auth_manager);
    let turn = Arc::get_mut(&mut turn_context).expect("test turn should be uniquely owned");
    turn.auth_manager = Some(auth_manager);
    turn.model_info.supports_search_tool = false;
    let config = Arc::make_mut(&mut turn.config);
    config
        .features
        .enable(Feature::Apps)
        .expect("apps feature should be configurable in tests");
    let _ = config.features.disable(Feature::ToolSuggest);
    config.orchestrator_mcp_enabled = true;
    let mut servers = config.mcp_servers.get().clone();
    servers.insert(
        "snapshot".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command,
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(10)),
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    config
        .mcp_servers
        .set(servers)
        .expect("test MCP server configuration should be accepted");
    let refresh_config = config.clone();
    session
        .refresh_mcp_servers_now(
            turn_context.as_ref(),
            &refresh_config,
            Some(session.mcp_elicitation_reviewer()),
        )
        .await;
    assert!(
        session
            .services
            .latest_mcp_runtime()
            .manager()
            .wait_for_server_ready("snapshot", Duration::from_secs(10))
            .await,
        "snapshot MCP server should become ready"
    );

    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    const SNAPSHOT_APP_ID: &str = "phase68-snapshot-app";
    const SNAPSHOT_APP_NAME: &str = "Phase 68 Snapshot App";
    const SNAPSHOT_TOOL_NAMESPACE: &str = "mcp__codex_apps__phase_68_snapshot_app";
    assert!(
        !step_context
            .mcp
            .manager()
            .list_all_tools()
            .await
            .iter()
            .any(|tool| tool.connector_id.as_deref() == Some(SNAPSHOT_APP_ID)),
        "the live manager inventory must intentionally differ from the seeded step snapshot"
    );
    step_context
        .seed_mcp_tools_for_test(vec![ToolInfo {
            server_name: CODEX_APPS_MCP_SERVER_NAME.to_string(),
            supports_parallel_tool_calls: false,
            server_origin: None,
            callable_name: "search".to_string(),
            callable_namespace: SNAPSHOT_TOOL_NAMESPACE.to_string(),
            namespace_description: None,
            tool: Tool::new_with_raw("search".to_string(), None, Arc::new(JsonObject::default())),
            connector_id: Some(SNAPSHOT_APP_ID.to_string()),
            connector_name: Some(SNAPSHOT_APP_NAME.to_string()),
            plugin_display_names: Vec::new(),
        }])
        .await;
    let input = vec![TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: format!("use [$snapshot](app://{SNAPSHOT_APP_ID})"),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }];
    let cancellation_token = CancellationToken::new();
    let PendingTurnPlanBuild::Ready(plan) = build_pure_pending_turn_plan(
        &session,
        Arc::clone(&step_context),
        &input,
        &cancellation_token,
    )
    .await?
    else {
        panic!("stable test inputs should produce a ready pending-turn plan");
    };
    assert!(plan.step_context.turn.apps_enabled());
    assert_eq!(
        plan.mentioned_apps,
        vec![(
            SNAPSHOT_APP_ID.to_string(),
            Some(SNAPSHOT_APP_NAME.to_string())
        )],
        "planning must resolve app mentions from the same seeded StepContext inventory as routing"
    );

    let (snapshot_ptr, snapshot_len) = {
        let tools = plan.step_context.mcp_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].connector_id.as_deref(), Some(SNAPSHOT_APP_ID));
        (tools.as_ptr(), tools.len())
    };
    let cached_tools = plan.step_context.mcp_tools().await;
    assert_eq!(cached_tools.as_ptr(), snapshot_ptr);
    assert_eq!(cached_tools.len(), snapshot_len);
    let router_tool_names = plan
        .first_router
        .model_visible_specs()
        .iter()
        .map(|spec| spec.name().to_string())
        .collect::<Vec<_>>();
    assert!(
        router_tool_names.iter().any(|name| {
            name == SNAPSHOT_TOOL_NAMESPACE || name == &format!("{SNAPSHOT_TOOL_NAMESPACE}.search")
        }),
        "the advertised router must be built from the seeded StepContext inventory; expected namespace {SNAPSHOT_TOOL_NAMESPACE:?}, got {router_tool_names:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_hook_continuation_preserves_finalization_warning_for_the_final_response() -> Result<()>
{
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let response_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("plan-response"),
                responses::ev_function_call(
                    "plan-call",
                    "update_plan",
                    &serde_json::json!({
                        "plan": [{
                            "id": "phase-68-warning",
                            "step": "exercise stop-hook continuation",
                            "status": "completed",
                            "acceptance_criteria": [
                                "warning is emitted after continuation"
                            ],
                            "runtime_paths": ["core/src/session/turn.rs"]
                        }]
                    })
                    .to_string(),
                ),
                responses::ev_completed("plan-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("draft-response"),
                responses::ev_assistant_message("draft-message", "draft answer"),
                responses::ev_completed("draft-response"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("final-response"),
                responses::ev_assistant_message("final-message", "final answer"),
                responses::ev_completed("final-response"),
            ]),
        ],
    )
    .await;
    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_one_shot_stop_hook(home).expect("write stop-hook fixture");
        })
        .with_workspace_setup(|cwd, _fs| async move {
            let scripts = cwd.join("scripts");
            tokio::fs::create_dir_all(scripts.as_path()).await?;
            tokio::fs::write(scripts.join("verify_local.py").as_path(), "").await?;
            tokio::fs::write(cwd.join("kd4_features.toml").as_path(), "").await?;
            Ok(())
        })
        .with_config(trust_discovered_hooks);
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "answer, then obey the stop hook".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let mut saw_final_response = false;
    let mut saw_finalization_warning = false;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = test.codex.next_event().await.expect("turn event");
            match event.msg {
                EventMsg::AgentMessage(message) if message.message == "final answer" => {
                    saw_final_response = true;
                }
                EventMsg::Warning(warning)
                    if warning.message.starts_with("KD4 task evidence is") =>
                {
                    assert!(
                        saw_final_response,
                        "the one-shot warning must not be consumed before stop-hook continuation"
                    );
                    saw_finalization_warning = true;
                }
                EventMsg::TurnComplete(_) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("turn should finish after one stop-hook continuation");
    assert!(saw_final_response);
    assert!(saw_finalization_warning);
    assert_eq!(response_log.requests().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_etag_refresh_does_not_block_stream_events_and_is_cancellable() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    const REFRESH_ETAG: &str = "\"phase-68-models-2\"";

    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("gpt-5.2")
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            let _ = config.features.disable(Feature::Apps);
        });
    let test = builder.build(&server).await?;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .insert_header("etag", REFRESH_ETAG)
                .set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let response_log = responses::mount_response_once(
        &server,
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("etag-response"),
            responses::ev_assistant_message("etag-message", "stream continued"),
            responses::ev_completed("etag-response"),
        ]))
        .insert_header("X-Models-Etag", REFRESH_ETAG),
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "exercise deferred ETag refresh".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = test.codex.next_event().await.expect("stream event");
            if matches!(
                event.msg,
                EventMsg::AgentMessage(ref message) if message.message == "stream continued"
            ) {
                break;
            }
        }
    })
    .await
    .expect("assistant stream events should arrive before the delayed models refresh completes");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let model_requests = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|request| request.url.path() == "/v1/models")
                .count();
            if model_requests >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("deferred models refresh should start after stream post-processing");

    test.codex.submit(Op::Interrupt).await?;
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = test.codex.next_event().await.expect("terminal event");
            if matches!(
                event.msg,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_)
            ) {
                break event.msg;
            }
        }
    })
    .await
    .expect("interrupt should cancel the delayed models refresh promptly");
    assert!(
        matches!(terminal, EventMsg::TurnComplete(_)),
        "once the final item is committed, a late interrupt must not retract it"
    );
    assert_eq!(response_log.requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn unchanged_model_and_comp_hash_skip_previous_model_context_reconstruction() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(ModelsResponse { models: Vec::new() }),
        )
        .mount(&server)
        .await;

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let auth_manager =
        AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    let mut config = (*turn_context.config).clone();
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.supports_websockets = false;
    let config = Arc::new(config);
    session.services.auth_manager = Arc::clone(&auth_manager);
    session.services.models_manager = crate::test_support::models_manager_with_provider(
        config.codex_home.to_path_buf(),
        Arc::clone(&auth_manager),
        config.model_provider.clone(),
    );
    turn_context.auth_manager = Some(auth_manager);
    turn_context.config = config;
    session
        .set_previous_turn_settings(Some(crate::session::PreviousTurnSettings {
            model: turn_context.model_info.slug.clone(),
            comp_hash: turn_context.model_info.comp_hash.clone(),
            realtime_active: Some(turn_context.realtime_active),
        }))
        .await;
    let mut client_session = session.services.model_client.new_session();
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);

    assert!(
        !maybe_run_previous_model_inline_compact(&session, &turn_context, &mut client_session,)
            .await?
    );
    let model_requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/models")
        .count();
    assert_eq!(
        model_requests, 0,
        "unchanged settings should return before TurnContext::with_model fetches the catalog"
    );
    Ok(())
}

#[tokio::test]
async fn plan_mode_uses_contributed_turn_item_for_last_agent_message() {
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let mut state = PlanModeStreamState::new(&turn_context.sub_id);
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        /*require_durable_lifecycle*/ false,
        /*prefinalized_turn_item*/ None,
    )
    .await
    .expect("plan item should be handled")
    .expect("assistant message should be recognized");

    assert_eq!(
        handled.last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}
