use super::HandleOutputCtx;
use super::OrderedResponseItemRecorder;
use super::TurnItemContributorPolicy;
use super::completed_item_defers_mailbox_delivery_to_next_turn;
use super::finalize_non_tool_response_item;
use super::handle_non_tool_response_item;
use super::handle_output_item_done;
use super::last_assistant_message_from_item;
use super::response_item_may_include_external_context;
use super::tool_call_arguments_length;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::ToolRouter;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolCall;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::turn_timing::ContinuationCause;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnItemContributor;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::memory_citation::MemoryCitation;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_tools::ToolName;
use codex_tools::ToolPayload;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;

#[test]
fn logging_contract_tool_call_metadata_omits_payload() {
    let call = ToolCall {
        tool_name: ToolName::plain("shell"),
        call_id: "call-secret".to_string(),
        payload: ToolPayload::Function {
            arguments: "argument secret".to_string(),
        },
    };

    assert_eq!(tool_call_arguments_length(&call), 15);
}

struct PersistenceProbeHandler {
    started: Arc<AtomicBool>,
}

impl ToolExecutor<ToolInvocation> for PersistenceProbeHandler {
    fn tool_name(&self) -> codex_tools::ToolName {
        codex_tools::ToolName::plain("persistence_probe")
    }

    fn spec(&self) -> codex_tools::ToolSpec {
        codex_tools::ToolSpec::Function(codex_tools::ResponsesApiTool {
            name: "persistence_probe".to_string(),
            description: "Persistence ordering probe.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: codex_tools::JsonSchema::default(),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle(&self, _invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        self.started.store(true, Ordering::SeqCst);
        Box::pin(async {
            Ok(
                Box::new(FunctionToolOutput::from_text("ok".to_string(), Some(true)))
                    as Box<dyn crate::tools::context::ToolOutput>,
            )
        })
    }
}

impl CoreToolRuntime for PersistenceProbeHandler {}

fn assistant_output_text(text: &str) -> ResponseItem {
    assistant_output_text_with_phase(text, /*phase*/ None)
}

fn assistant_output_text_with_phase(text: &str, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn external_context_pollution_items_include_web_search_and_tool_search() {
    let polluting_items = [
        ResponseItem::WebSearchCall {
            id: None,
            status: Some("completed".to_string()),
            action: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ToolSearchCall {
            id: None,
            call_id: Some("search-1".to_string()),
            status: None,
            execution: "client".to_string(),
            arguments: serde_json::json!({"query": "calendar"}),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ToolSearchOutput {
            id: None,
            call_id: Some("search-1".to_string()),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: Vec::new(),
            omitted_result_count: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    assert!(
        polluting_items
            .iter()
            .all(response_item_may_include_external_context)
    );
}

#[test]
fn external_context_pollution_items_exclude_local_tool_calls() {
    let non_polluting_items = [
        ResponseItem::LocalShellCall {
            id: None,
            call_id: Some("shell-1".to_string()),
            status: LocalShellStatus::Completed,
            action: LocalShellAction::Exec(LocalShellExecAction {
                command: vec!["cat".to_string(), "README.md".to_string()],
                timeout_ms: None,
                working_directory: None,
                env: None,
                user: None,
            }),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "custom-1".to_string(),
            name: "apply_patch".to_string(),
            namespace: None,
            input: "*** Begin Patch\n*** End Patch\n".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::CustomToolCallOutput {
            id: None,
            call_id: "custom-1".to_string(),
            name: Some("apply_patch".to_string()),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        },
        assistant_output_text("plain assistant text"),
    ];

    assert!(
        !non_polluting_items
            .iter()
            .any(response_item_may_include_external_context)
    );
}

#[tokio::test]
async fn external_context_pollution_signal_is_claimed_once_per_turn() {
    let (_, turn_context) = make_session_and_context().await;

    assert!(turn_context.claim_memory_pollution_signal());
    assert!(!turn_context.claim_memory_pollution_signal());
}

#[tokio::test]
async fn handle_non_tool_response_item_strips_citations_from_assistant_message() {
    let (session, _) = make_session_and_context().await;
    let item = assistant_output_text(
        "hello<oai-mem-citation><citation_entries>\nMEMORY.md:1-2|note=[x]\n</citation_entries>\n<rollout_ids>\n019cc2ea-1dff-7902-8d40-c8f6e5d83cc4\n</rollout_ids></oai-mem-citation> world",
    );

    let turn_item = handle_non_tool_response_item(
        &session,
        TurnItemContributorPolicy::Skip,
        &item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant message should parse");

    let TurnItem::AgentMessage(agent_message) = turn_item else {
        panic!("expected agent message");
    };
    let text = agent_message
        .content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect::<String>();
    assert_eq!(text, "hello world");
    let memory_citation = agent_message
        .memory_citation
        .expect("memory citation should be parsed");
    assert_eq!(memory_citation.entries.len(), 1);
    assert_eq!(memory_citation.entries[0].path, "MEMORY.md");
    assert_eq!(
        memory_citation.rollout_ids,
        vec!["019cc2ea-1dff-7902-8d40-c8f6e5d83cc4".to_string()]
    );
}

struct TestTurnItemContributor;

#[derive(Debug)]
struct TurnItemContributorRan;

impl TurnItemContributor for TestTurnItemContributor {
    fn contribute<'a>(
        &'a self,
        _thread_store: &'a ExtensionData,
        turn_store: &'a ExtensionData,
        item: &'a mut TurnItem,
    ) -> codex_extension_api::ExtensionFuture<'a, Result<(), String>> {
        Box::pin(async move {
            turn_store.insert(TurnItemContributorRan);
            if let TurnItem::AgentMessage(agent_message) = item {
                agent_message.memory_citation = Some(MemoryCitation {
                    entries: Vec::new(),
                    rollout_ids: Vec::new(),
                });
            }
            Ok(())
        })
    }
}

struct RewriteAgentMessageContributor;

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
                    text: "contributed assistant text".to_string(),
                }];
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn handle_non_tool_response_item_runs_turn_item_contributors_only_when_requested() {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(TestTurnItemContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let item = assistant_output_text(
        "hello<oai-mem-citation>ignored by memory parser</oai-mem-citation> world",
    );

    let provisional_turn_item = handle_non_tool_response_item(
        &session,
        TurnItemContributorPolicy::Skip,
        &item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant message should parse");

    assert!(turn_store.get::<TurnItemContributorRan>().is_none());
    let TurnItem::AgentMessage(provisional_agent_message) = provisional_turn_item else {
        panic!("expected agent message");
    };
    assert_eq!(provisional_agent_message.memory_citation, None);

    let turn_item = handle_non_tool_response_item(
        &session,
        TurnItemContributorPolicy::Run(&turn_store),
        &item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant message should parse");

    assert!(turn_store.get::<TurnItemContributorRan>().is_some());
    let TurnItem::AgentMessage(agent_message) = turn_item else {
        panic!("expected agent message");
    };
    assert!(agent_message.memory_citation.is_some());
    let text = agent_message
        .content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect::<String>();
    assert_eq!(text, "hello world");
}

#[tokio::test]
async fn handle_output_item_done_returns_contributed_last_agent_message() {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            mcp_tools: None,
            deferred_mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn_context.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let tool_runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker);
    let item = assistant_output_text("original assistant text");
    let mut ctx = HandleOutputCtx {
        sess: session,
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };

    let output = handle_output_item_done(
        &mut ctx, item, /*previously_active_item*/ None, &mut true,
    )
    .await
    .expect("assistant message should complete");

    assert_eq!(
        output.last_agent_message.as_deref(),
        Some("contributed assistant text")
    );
}

#[tokio::test]
async fn malformed_client_tool_search_records_correlated_tool_search_output() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            mcp_tools: None,
            deferred_mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn_context.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let tool_runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker);
    let item = ResponseItem::ToolSearchCall {
        id: None,
        call_id: Some("search-malformed".to_string()),
        status: None,
        execution: "client".to_string(),
        arguments: serde_json::json!({"query": 42}),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&session),
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };

    let output = handle_output_item_done(
        &mut ctx, item, /*previously_active_item*/ None, &mut true,
    )
    .await
    .expect("malformed tool_search call should be recorded for model recovery");

    assert!(output.needs_follow_up);
    assert!(output.tool_future.is_none());
    ctx.response_item_recorder.flush().await;
    let history = session.clone_history().await;
    let [
        ResponseItem::ToolSearchCall {
            call_id: Some(request_call_id),
            ..
        },
        ResponseItem::ToolSearchOutput {
            call_id: Some(output_call_id),
            status,
            execution,
            tools,
            ..
        },
    ] = history.raw_items()
    else {
        panic!("expected a tool_search call followed by its failure output")
    };
    assert_eq!(request_call_id, "search-malformed");
    assert_eq!(output_call_id, request_call_id);
    assert_eq!(status, "incomplete");
    assert_eq!(execution, "client");
    assert_eq!(
        tools,
        &[serde_json::json!({
            "type": "tool_search_error",
            "message": "failed to parse function arguments: invalid type: integer `42`, expected a string",
        })]
    );
}

#[tokio::test]
async fn unstreamed_contributed_assistant_item_replays_finalized_text_between_lifecycle_events() {
    let (mut session, turn_context, rx) = make_session_and_context_with_rx().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    Arc::get_mut(&mut session)
        .expect("test session should be uniquely owned")
        .services
        .extensions = Arc::new(builder.build());
    let step_context = StepContext::for_test(Arc::clone(&turn_context));
    let router = Arc::new(ToolRouter::from_context(
        step_context.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            mcp_tools: None,
            deferred_mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: turn_context.dynamic_tools.as_slice(),
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    let step_context = step_context.with_tool_router_for_test(router);
    let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));
    let tool_runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker);
    let mut ctx = HandleOutputCtx {
        sess: session,
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };

    handle_output_item_done(
        &mut ctx,
        assistant_output_text("original assistant text"),
        /*previously_active_item*/ None,
        &mut true,
    )
    .await
    .expect("assistant message should complete");

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Some(event) = match event.msg {
            EventMsg::ItemStarted(_) => Some("started".to_string()),
            EventMsg::AgentMessageContentDelta(event) => Some(format!("delta:{}", event.delta)),
            EventMsg::ItemCompleted(_) => Some("completed".to_string()),
            _ => None,
        } {
            events.push(event);
        }
    }
    assert_eq!(
        events,
        ["started", "delta:contributed assistant text", "completed"]
    );
}

#[tokio::test]
async fn completed_tool_call_required_persistence_does_not_block_stream_and_precedes_dispatch() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context.turn_timing_state.mark_turn_started();
    let sampling = turn_context.turn_timing_state.begin_sampling();
    let mut pending = None::<ContinuationCause>;
    turn_context
        .turn_timing_state
        .begin_model_generation(&mut pending, &SessionSource::Cli);
    drop(turn_context.turn_timing_state.begin_model_request_wait());
    drop(sampling);
    let started = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(PersistenceProbeHandler {
        started: Arc::clone(&started),
    }) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step_context =
        StepContext::for_test(Arc::clone(&turn_context)).with_tool_router_for_test(router);
    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "persistence_probe".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "persisted-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let response_item_recorder = OrderedResponseItemRecorder::default();
    let (release_persistence, persistence_blocked) = tokio::sync::oneshot::channel();
    response_item_recorder
        .block_required_persistence_for_test(persistence_blocked)
        .await;
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&session),
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder,
    };
    let mut eager_prefix_open = true;

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        handle_output_item_done(
            &mut ctx,
            item,
            /*previously_active_item*/ None,
            &mut eager_prefix_open,
        ),
    )
    .await
    .expect("rollout persistence must not block response stream handling")
    .expect("read-safe tool call should be accepted");

    assert!(!output.eager_read_eligible);
    assert!(!started.load(Ordering::SeqCst));
    assert_eq!(
        turn_context.turn_timing_state.model_tool_call_counts(),
        Some((1, 0)),
        "model emission must be counted before the deferred executor future is polled"
    );
    assert!(session.clone_history().await.raw_items().is_empty());
    let mut tool_future = Box::pin(
        output
            .tool_future
            .expect("accepted tool call should retain its lazy future")
            .into_future(),
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), tool_future.as_mut())
            .await
            .is_err(),
        "dispatch must remain blocked behind ordered persistence"
    );
    assert!(!started.load(Ordering::SeqCst));

    release_persistence
        .send(())
        .expect("persistence blocker should still be active");
    tool_future
        .await
        .result
        .expect("persistence probe handler should succeed");
    let history = session.clone_history().await;
    let [ResponseItem::FunctionCall { call_id, .. }] = history.raw_items() else {
        panic!("completed tool call must be persisted before dispatch")
    };
    assert_eq!(call_id, "persisted-read");
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(
        turn_context.turn_timing_state.model_tool_call_counts(),
        Some((1, 1)),
        "executor polling must be counted separately from model emission"
    );
}

#[tokio::test]
async fn completed_tool_call_auxiliary_persistence_does_not_block_dispatch() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context.turn_timing_state.mark_turn_started();
    let sampling = turn_context.turn_timing_state.begin_sampling();
    let mut pending = None::<ContinuationCause>;
    turn_context
        .turn_timing_state
        .begin_model_generation(&mut pending, &SessionSource::Cli);
    drop(turn_context.turn_timing_state.begin_model_request_wait());
    drop(sampling);
    let started = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(PersistenceProbeHandler {
        started: Arc::clone(&started),
    }) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step_context =
        StepContext::for_test(Arc::clone(&turn_context)).with_tool_router_for_test(router);
    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "persistence_probe".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "auxiliary-read".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let response_item_recorder = OrderedResponseItemRecorder::default();
    let recorder_for_flush = response_item_recorder.clone();
    let (release_auxiliary, auxiliary_blocked) = tokio::sync::oneshot::channel();
    response_item_recorder
        .block_auxiliary_persistence_for_test(auxiliary_blocked)
        .await;
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&session),
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder,
    };
    let mut eager_prefix_open = true;

    let output = handle_output_item_done(
        &mut ctx,
        item,
        /*previously_active_item*/ None,
        &mut eager_prefix_open,
    )
    .await
    .expect("read-safe tool call should be accepted");
    output
        .tool_future
        .expect("accepted tool call should retain its lazy future")
        .into_future()
        .await
        .result
        .expect("auxiliary persistence must not delay tool dispatch");

    assert!(started.load(Ordering::SeqCst));
    let history = session.clone_history().await;
    let [ResponseItem::FunctionCall { call_id, .. }] = history.raw_items() else {
        panic!("completed tool call must be model-visible before dispatch")
    };
    assert_eq!(call_id, "auxiliary-read");

    let mut flush = Box::pin(recorder_for_flush.flush());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), flush.as_mut())
            .await
            .is_err(),
        "the test must keep auxiliary persistence blocked after dispatch"
    );
    release_auxiliary
        .send(())
        .expect("auxiliary persistence blocker should still be active");
    flush.await;
}

#[tokio::test]
async fn exact_tool_call_replay_is_deduplicated_but_conflicting_reuse_is_rejected() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    turn_context.turn_timing_state.mark_turn_started();
    let sampling = turn_context.turn_timing_state.begin_sampling();
    let mut pending = None::<ContinuationCause>;
    turn_context
        .turn_timing_state
        .begin_model_generation(&mut pending, &SessionSource::Cli);
    drop(turn_context.turn_timing_state.begin_model_request_wait());
    drop(sampling);
    let started = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(PersistenceProbeHandler {
        started: Arc::clone(&started),
    }) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step_context =
        StepContext::for_test(Arc::clone(&turn_context)).with_tool_router_for_test(router);
    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        step_context,
        Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
    );
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "persistence_probe".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "duplicate-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&session),
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };
    let mut eager_prefix_open = true;

    let first = handle_output_item_done(
        &mut ctx,
        item.clone(),
        /*previously_active_item*/ None,
        &mut eager_prefix_open,
    )
    .await
    .expect("first tool call should be accepted");
    assert!(first.tool_future.is_some());

    let second = handle_output_item_done(
        &mut ctx,
        item.clone(),
        /*previously_active_item*/ None,
        &mut eager_prefix_open,
    )
    .await
    .expect("an exact provider replay should be ignored");
    assert!(second.tool_future.is_none());
    assert!(!second.needs_follow_up);

    let conflicting = ResponseItem::FunctionCall {
        id: None,
        name: "persistence_probe".to_string(),
        namespace: None,
        arguments: r#"{"changed":true}"#.to_string(),
        call_id: "duplicate-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let third = handle_output_item_done(
        &mut ctx,
        conflicting,
        /*previously_active_item*/ None,
        &mut eager_prefix_open,
    )
    .await;
    assert!(matches!(third, Err(CodexErr::Fatal(message)) if message.contains("same call ID")));
    assert!(!started.load(Ordering::SeqCst));

    ctx.response_item_recorder.flush().await;
    let history = session.clone_history().await;
    assert_eq!(history.raw_items().len(), 1);
    let closure = turn_context.turn_timing_state.tool_closure_snapshot();
    assert_eq!(closure.accepted_count, 1);
    assert_eq!(closure.duplicate_call_id_count, 1);
}

#[tokio::test]
async fn finalized_turn_item_defers_mailbox_for_contributed_visible_text() {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let item = assistant_output_text("<oai-mem-citation>hidden only</oai-mem-citation>");

    let finalized = finalize_non_tool_response_item(
        &session,
        TurnItemContributorPolicy::Run(&turn_store),
        &item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant message should parse");

    assert_eq!(
        finalized.facts.last_agent_message.as_deref(),
        Some("contributed assistant text")
    );
    assert!(finalized.facts.defers_mailbox_delivery_to_next_turn);
}

#[tokio::test]
async fn finalized_turn_item_keeps_mailbox_open_for_commentary_text() {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_item_contributor(Arc::new(RewriteAgentMessageContributor));
    session.services.extensions = Arc::new(builder.build());
    let turn_store = ExtensionData::new(turn_context.sub_id.clone());
    let item = assistant_output_text_with_phase("still working", Some(MessagePhase::Commentary));

    let finalized = finalize_non_tool_response_item(
        &session,
        TurnItemContributorPolicy::Run(&turn_store),
        &item,
        /*plan_mode*/ false,
    )
    .await
    .expect("assistant message should parse");

    assert_eq!(
        finalized.facts.last_agent_message.as_deref(),
        Some("contributed assistant text")
    );
    assert!(!finalized.facts.defers_mailbox_delivery_to_next_turn);
}

#[test]
fn last_assistant_message_from_item_strips_citations_and_plan_blocks() {
    let item = assistant_output_text(
        "before<oai-mem-citation>doc1</oai-mem-citation>\n<proposed_plan>\n- x\n</proposed_plan>\nafter",
    );

    let message = last_assistant_message_from_item(&item, /*plan_mode*/ true)
        .expect("assistant text should remain after stripping");

    assert_eq!(message, "before\nafter");
}

#[test]
fn last_assistant_message_from_item_returns_none_for_citation_only_message() {
    let item = assistant_output_text("<oai-mem-citation>doc1</oai-mem-citation>");

    assert_eq!(
        last_assistant_message_from_item(&item, /*plan_mode*/ false),
        None
    );
}

#[test]
fn last_assistant_message_from_item_returns_none_for_plan_only_hidden_message() {
    let item = assistant_output_text("<proposed_plan>\n- x\n</proposed_plan>");

    assert_eq!(
        last_assistant_message_from_item(&item, /*plan_mode*/ true),
        None
    );
}

#[test]
fn completed_item_defers_mailbox_delivery_for_unknown_phase_messages() {
    let item = assistant_output_text("final answer");

    assert!(completed_item_defers_mailbox_delivery_to_next_turn(
        &item, /*plan_mode*/ false,
    ));
}

#[test]
fn completed_item_keeps_mailbox_delivery_open_for_commentary_messages() {
    let item = assistant_output_text_with_phase("still working", Some(MessagePhase::Commentary));

    assert!(!completed_item_defers_mailbox_delivery_to_next_turn(
        &item, /*plan_mode*/ false,
    ));
}
