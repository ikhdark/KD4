use super::HandleOutputCtx;
use super::OrderedResponseItemRecorder;
use super::TurnItemContributorPolicy;
use super::completed_item_defers_mailbox_delivery_to_next_turn;
use super::finalize_non_tool_response_item;
use super::handle_non_tool_response_item;
use super::handle_output_item_done;
use super::last_assistant_message_from_item;
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
use codex_protocol::models::ContentItem;
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
fn tool_call_arguments_length_counts_utf8_payload_bytes() {
    let call = ToolCall {
        tool_name: ToolName::plain("shell"),
        call_id: "call-secret".to_string(),
        payload: ToolPayload::Function {
            arguments: "argument secret".to_string(),
        },
    };

    assert_eq!(tool_call_arguments_length(&call), 15);
    for (arguments, bytes) in [("", 0), ("é🦀", 6)] {
        let mut call = call.clone();
        call.payload = ToolPayload::Function { arguments: arguments.to_string() };
        assert_eq!(tool_call_arguments_length(&call), bytes);
    }
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

#[tokio::test]
async fn handle_non_tool_response_item_keeps_literal_citation_markup_visible() {
    let (session, _) = make_session_and_context().await;
    // Memory citations were removed; text that mentions their tag must not be hidden.
    let original = "the parser handled `<oai-mem-citation>` and everything after it";
    let item = assistant_output_text(original);

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
    assert_eq!(text, original);
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
                agent_message.phase = Some(codex_protocol::models::MessagePhase::Commentary);
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
    let item = assistant_output_text("hello world");

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
    assert_eq!(provisional_agent_message.phase, None);

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
    assert_eq!(
        agent_message.phase,
        Some(codex_protocol::models::MessagePhase::Commentary)
    );
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
async fn malformed_client_tool_search_records_correlated_tool_search_output() {
    use crate::session::turn_execution::SamplingRequestSettledState;
    use crate::session::turn_execution::TurnExecutionControl;

    let mut control = TurnExecutionControl::new();
    let baselines = control.baselines(0);
    let settled = SamplingRequestSettledState {
        mutation_revision: 0,
        attributed_mutation_revision: 0,
        tool_exposure_revision: 0,
    };
    let collector = control.collector(&baselines);
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
    let tool_runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker)
        .with_sampling_request_signals(collector.clone());
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

    let mut eager_prefix_open = true;
    let output = handle_output_item_done(
        &mut ctx, item.clone(), /*previously_active_item*/ None, &mut eager_prefix_open,
    )
    .await
    .expect("malformed tool_search call should be recorded for model recovery");

    assert!(!eager_prefix_open);
    assert!(output.needs_follow_up);
    assert!(output.tool_future.is_none());
    assert!(control.observe_budget_progress(&baselines, &collector, &settled));
    assert!(control.evaluate_convergence(&baselines, &collector, &settled).directive.is_none());
    ctx.response_item_recorder.flush().await.unwrap();
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
        failure_detail,
    ] = history.raw_items()
    else {
        panic!("expected a tool_search call followed by its failure output")
    };
    assert_eq!(request_call_id, "search-malformed");
    assert_eq!(output_call_id, request_call_id);
    assert_eq!(status, "incomplete");
    assert_eq!(execution, "client");
    assert!(crate::parse_turn_item(failure_detail).is_none());
    let ResponseItem::Message { role, content, .. } = failure_detail else {
        panic!("expected recovery context alongside the native failure output");
    };
    assert_eq!(role, "user");
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected a bounded failure explanation");
    };
    assert!(text.contains("search-malformed"));
    assert!(text.contains("failed to parse tool_search arguments"));
    assert!(text.contains("expected a string"));
    assert!(text.contains("kind=\"untrusted\""));
    assert!(
        tools.is_empty(),
        "failed searches must not publish invalid tool declarations"
    );

    let repeated = control.collector(&baselines);
    ctx.tool_runtime = ctx.tool_runtime.clone().with_sampling_request_signals(repeated.clone());
    let mut item = item;
    if let ResponseItem::ToolSearchCall { call_id, .. } = &mut item {
        *call_id = Some("search-malformed-again".into());
    }
    let output = handle_output_item_done(&mut ctx, item, None, &mut true).await.unwrap();
    assert!(output.needs_follow_up);
    assert!(output.tool_future.is_none());
    assert!(!control.observe_budget_progress(&baselines, &repeated, &settled));
    assert!(control.evaluate_convergence(&baselines, &repeated, &settled).directive.is_some());
    ctx.response_item_recorder.flush().await.unwrap();
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

    let output = handle_output_item_done(
        &mut ctx,
        assistant_output_text("original assistant text"),
        /*previously_active_item*/ None,
        &mut true,
    )
    .await
    .expect("assistant message should complete");

    assert_eq!(output.last_agent_message.as_deref(), Some("contributed assistant text"));
    ctx.response_item_recorder.flush().await.unwrap();
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
async fn completed_tool_call_dispatch_overlaps_required_persistence() {
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
    let tool_future = output
        .tool_future
        .expect("accepted tool call")
        .into_future();
    tokio::time::timeout(std::time::Duration::from_secs(5), tool_future)
        .await
        .expect("dispatch must not wait for ordered persistence")
        .result
        .expect("persistence probe handler should succeed");
    assert!(started.load(Ordering::SeqCst));
    assert!(session.clone_history().await.raw_items().is_empty());
    release_persistence
        .send(())
        .expect("persistence blocker still active");
    ctx.response_item_recorder.flush().await.unwrap();
    let history = session.clone_history().await;
    let [ResponseItem::FunctionCall { call_id, .. }] = history.raw_items() else {
        panic!("completed tool call must be persisted before relaying results")
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

    let output = tokio::time::timeout(std::time::Duration::from_secs(1), handle_output_item_done(
        &mut ctx,
        item,
        /*previously_active_item*/ None,
        &mut eager_prefix_open,
    ))
    .await
    .expect("acceptance must not wait for auxiliary persistence")
    .expect("read-safe tool call should be accepted");
    tokio::time::timeout(std::time::Duration::from_secs(1), output
        .tool_future
        .expect("accepted tool call should retain its lazy future")
        .into_future())
        .await
        .expect("dispatch must not wait for auxiliary persistence")
        .result
        .expect("auxiliary persistence must not delay tool dispatch");

    assert!(started.load(Ordering::SeqCst));
    let mut flush = Box::pin(recorder_for_flush.flush());
    assert!(
        futures::poll!(flush.as_mut()).is_pending(),
        "the test must keep auxiliary persistence blocked after dispatch"
    );
    release_auxiliary
        .send(())
        .expect("auxiliary persistence blocker should still be active");
    tokio::time::timeout(std::time::Duration::from_secs(1), flush)
        .await.expect("released persistence must finish").unwrap();
    let history = session.clone_history().await;
    let [ResponseItem::FunctionCall { call_id, .. }] = history.raw_items() else {
        panic!("completed tool call must be model-visible before relaying results")
    };
    assert_eq!(call_id, "auxiliary-read");
}

#[tokio::test]
async fn failed_required_publication_is_reported_before_tool_relay() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    session.close_durable_history_commit_gate_for_test();
    let started = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(PersistenceProbeHandler {
        started: Arc::clone(&started),
    }) as Arc<dyn CoreToolRuntime>;
    let router = Arc::new(ToolRouter::from_parts(
        ToolRegistry::from_tools([handler]),
        Vec::new(),
    ));
    let step = StepContext::for_test(Arc::clone(&turn)).with_tool_router_for_test(router);
    let mut ctx = HandleOutputCtx {
        sess: Arc::clone(&session),
        turn_context: Arc::clone(&turn),
        turn_store: Arc::new(ExtensionData::new(turn.sub_id.clone())),
        tool_runtime: ToolCallRuntime::new(
            Arc::clone(&session),
            step,
            Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        ),
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };
    let mut eager_prefix_open = true;
    let output = handle_output_item_done(
        &mut ctx,
        ResponseItem::FunctionCall {
            id: None,
            name: "persistence_probe".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "rejected-publication".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        None,
        &mut eager_prefix_open,
    )
    .await
    .unwrap();
    output
        .tool_future
        .expect("tool future")
        .into_future()
        .await
        .result
        .expect("dispatch runs independently of rollout recording");
    assert!(started.load(Ordering::SeqCst));
    assert!(session.clone_history().await.raw_items().is_empty());
    assert!(ctx.response_item_recorder.flush().await.is_err());
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
    let Err(CodexErr::Fatal(message)) = third else {
        panic!("conflicting reuse must be rejected as a fatal duplicate");
    };
    assert_eq!(
        message,
        "refusing tool call `duplicate-call` because the same call ID was already accepted in this model generation"
    );
    assert!(!started.load(Ordering::SeqCst));

    ctx.response_item_recorder.flush().await.unwrap();
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
    for (text, phase, plan_mode, defers) in [
        ("<proposed_plan>\n- hidden only\n</proposed_plan>", None, true, true),
        ("still working", Some(MessagePhase::Commentary), false, false),
        ("finished", Some(MessagePhase::FinalAnswer), false, true),
    ] {
        let item = assistant_output_text_with_phase(text, phase);
        let finalized = finalize_non_tool_response_item(
            &session, TurnItemContributorPolicy::Run(&turn_store), &item, plan_mode,
        ).await.expect("assistant message should parse");
        assert_eq!(finalized.facts.last_agent_message.as_deref(), Some("contributed assistant text"));
        assert_eq!(finalized.facts.defers_mailbox_delivery_to_next_turn, defers);
    }
}

#[test]
fn last_assistant_message_from_item_strips_only_plan_blocks() {
    let mixed = "before<oai-mem-citation>doc1</oai-mem-citation>\n<proposed_plan>\n- x\n</proposed_plan>\nafter";
    let hidden = "<proposed_plan>\n- x\n</proposed_plan>";
    for (text, plan_mode, expected) in [
        (mixed, true, Some("before<oai-mem-citation>doc1</oai-mem-citation>\nafter")),
        (mixed, false, Some(mixed)),
        (hidden, true, None),
        (hidden, false, Some(hidden)),
        ("", false, None),
        ("  \n", false, None),
    ] {
        assert_eq!(last_assistant_message_from_item(&assistant_output_text(text), plan_mode).as_deref(), expected);
    }
}

#[test]
fn completed_item_defers_mailbox_only_for_visible_final_text() {
    for phase in [None, Some(MessagePhase::Commentary), Some(MessagePhase::FinalAnswer)] {
        let item = assistant_output_text_with_phase("final answer", phase.clone());
        assert_eq!(completed_item_defers_mailbox_delivery_to_next_turn(&item, false),
            phase != Some(MessagePhase::Commentary));
        let hidden = assistant_output_text_with_phase("<proposed_plan>\n- x\n</proposed_plan>", phase);
        assert!(!completed_item_defers_mailbox_delivery_to_next_turn(&hidden, true));
    }
}

#[tokio::test]
async fn audit_reports_17_19_terminal_generation_rejects_tools_before_acceptance() {
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
    let tool_runtime = ToolCallRuntime::new(Arc::clone(&session), step_context, tracker)
        .with_terminal_completion_only(true);
    let item = ResponseItem::FunctionCall {
        id: None,
        name: "exec_command".to_string(),
        namespace: None,
        arguments: r#"{"cmd":"echo should-not-run"}"#.to_string(),
        call_id: "denied".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut ctx = HandleOutputCtx {
        sess: session,
        turn_context: Arc::clone(&turn_context),
        turn_store: Arc::new(ExtensionData::new(turn_context.sub_id.clone())),
        tool_runtime,
        cancellation_token: CancellationToken::new(),
        response_item_recorder: OrderedResponseItemRecorder::default(),
    };

    let result = handle_output_item_done(&mut ctx, item, None, &mut true).await;
    assert!(matches!(result, Err(CodexErr::Fatal(message)) if message.contains("terminal-only")));
    assert!(ctx.sess.clone_history().await.raw_items().is_empty());
    assert_eq!(turn_context.turn_timing_state.tool_closure_snapshot().accepted_count, 0);
}
